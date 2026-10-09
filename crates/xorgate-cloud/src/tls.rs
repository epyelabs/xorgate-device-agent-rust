//! mTLS client configuration for AWS IoT Core from PEM material.
//!
//! - Device cert + key from `CERT_PATH` / `KEY_PATH` (AWS one-click certs are
//!   RSA-2048 PKCS#1; `rustls-pki-types` auto-detects PKCS#1/PKCS#8/SEC1).
//! - Roots from `CA_PATH` when readable (the installer writes Amazon Root CA 1
//!   there); when no CA is supplied, the bundled Amazon Root CA 1 is used so a
//!   device with a missing root file can still connect to the `-ats` endpoint.
//!   A supplied-but-wrong CA is honoured as given (it must FAIL the handshake —
//!   that is how A6 forces a TLS error on the bench).

use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// Amazon Root CA 1 (public; the root of every `*-ats.iot.<region>.amazonaws.com` chain).
pub const AMAZON_ROOT_CA1_PEM: &[u8] = include_bytes!("../assets/AmazonRootCA1.pem");

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("no certificate found in CA PEM")]
    EmptyCa,
    #[error("no certificate found in client cert PEM")]
    EmptyClientCert,
    #[error("client key PEM: {0}")]
    ClientKey(rustls_pki_types::pem::Error),
    #[error("CA PEM: {0}")]
    CaPem(rustls_pki_types::pem::Error),
    #[error("client cert PEM: {0}")]
    ClientCertPem(rustls_pki_types::pem::Error),
    #[error("root store rejected the CA: {0}")]
    RootStore(rustls::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Build the rustls `ClientConfig` for the IoT data endpoint.
pub fn client_config(
    ca_pem: Option<&[u8]>,
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<ClientConfig>, TlsError> {
    let ca = ca_pem.unwrap_or(AMAZON_ROOT_CA1_PEM);
    let mut roots = RootCertStore::empty();
    let mut n = 0;
    for cert in CertificateDer::pem_slice_iter(ca) {
        let cert = cert.map_err(TlsError::CaPem)?;
        roots.add(cert).map_err(TlsError::RootStore)?;
        n += 1;
    }
    if n == 0 {
        return Err(TlsError::EmptyCa);
    }

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(TlsError::ClientCertPem)?;
    if certs.is_empty() {
        return Err(TlsError::EmptyClientCert);
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(TlsError::ClientKey)?;

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_root_parses() {
        let certs: Vec<_> = CertificateDer::pem_slice_iter(AMAZON_ROOT_CA1_PEM)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn empty_ca_is_an_error() {
        let err = client_config(Some(b"not a pem"), b"", b"").unwrap_err();
        assert!(
            matches!(err, TlsError::EmptyCa | TlsError::CaPem(_)),
            "{err}"
        );
    }
}
