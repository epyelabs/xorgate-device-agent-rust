//! `xorgate-cloud` — the cloud plane of the Rust device agent (plan README
//! §3.1): the AWS IoT Core MQTT v5 client wrapper over `rumqttc` + `rustls`.
//!
//! Phase 0 ships `mqtt` (connect/LWT/resubscribe/backoff/state watch) and
//! `tls` (mTLS client config from PEM, Amazon Root CA 1 bundled as the
//! fallback root). The IoT credentials provider and S3 PUT arrive in Phase 2/3.

pub mod backoff;
pub mod mqtt;
pub mod tls;
