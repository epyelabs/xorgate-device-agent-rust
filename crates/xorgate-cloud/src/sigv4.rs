//! Hand-rolled AWS Signature V4 for one virtual-hosted S3 `PUT` — the pure
//! half of `src/recording/s3-put.ts` (`encodeS3Key` :35-42, `signPut`
//! :54-103). Deterministic given the clock value and the credentials, so it
//! is unit-tested without a network (`fixtures/s3/sign-put.json`, dumped from
//! the TS by tsx, plus the AWS-published S3 example vectors).
//!
//! It lives in `xorgate-cloud` (network-adjacent) rather than `xorgate-core`
//! because it needs the hashing crates; it does no I/O and reads no clock.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The credential triple SigV4 needs (a view of
/// [`crate::iot_creds::AwsCredentials`] minus the expiry).
#[derive(Clone, Copy)]
pub struct SigningCredentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub session_token: &'a str,
}

/// `SignedHeaders` (`s3-put.ts:44-48`): `headers` keeps the TS object's key
/// order (`content-type, host, x-amz-content-sha256, x-amz-date,
/// x-amz-security-token, content-length, authorization`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    pub host: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl SignedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// `sha256` (`s3-put.ts:29-30`): lowercase hex digest.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `hmac` (`s3-put.ts:31-32`).
fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// `encodeS3Key` (`s3-put.ts:35-42`): `encodeURIComponent` per `/`-separated
/// segment, then `!'()*` → `%21 %27 %28 %29 %2A`. Net effect: only
/// `A-Z a-z 0-9 - _ . ~` stay literal; every other UTF-8 byte is `%XX`
/// (uppercase hex). A `str` cannot hold the lone surrogate on which
/// `encodeURIComponent` throws, so this is total.
pub fn encode_s3_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (i, seg) in key.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `amzDate` (`s3-put.ts:58-59`): `new Date(nowMs).toISOString()` with every
/// `-`/`:` removed and the first `.sss` dropped → `YYYYMMDDTHHMMSSZ`
/// (milliseconds truncated, not rounded).
pub fn amz_date(now_ms: f64) -> String {
    let iso = xorgate_core::js::to_iso_string(now_ms);
    let stripped: String = iso.chars().filter(|c| *c != '-' && *c != ':').collect();
    // `.replace(/\.\d{3}/, "")`: first `.` followed by three ASCII digits.
    let bytes = stripped.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'.'
            && bytes.len() >= i + 4
            && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit)
        {
            return format!("{}{}", &stripped[..i], &stripped[i + 4..]);
        }
    }
    stripped
}

/// The generic SigV4 core (`s3-put.ts:70-90`) over an already-built header
/// list: canonical headers are the lowercase names sorted (`Object.keys(…)
/// .sort()` — UTF-16 order, identical to byte order for ASCII names) with
/// JS-`trim`med values; no query string. Returns `(signed_headers, scope,
/// signature)`.
#[allow(clippy::too_many_arguments)]
pub fn signature_v4(
    method: &str,
    canonical_path: &str,
    headers: &[(&str, &str)],
    payload_sha256: &str,
    amz_date: &str,
    secret_access_key: &str,
    region: &str,
    service: &str,
) -> (String, String, String) {
    let date_stamp: String = amz_date.chars().take(8).collect();
    let mut names: Vec<&(&str, &str)> = headers.iter().collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = names
        .iter()
        .map(|(h, v)| format!("{h}:{}\n", xorgate_core::js::trim(v)))
        .collect();
    let signed_headers = names.iter().map(|(h, _)| *h).collect::<Vec<_>>().join(";");

    let canonical_request = [
        method,
        canonical_path,
        "", // no query string
        &canonical_headers,
        &signed_headers,
        payload_sha256,
    ]
    .join("\n");

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = [
        "AWS4-HMAC-SHA256",
        amz_date,
        &scope,
        &sha256_hex(canonical_request.as_bytes()),
    ]
    .join("\n");

    let k_date = hmac(format!("AWS4{secret_access_key}").as_bytes(), &date_stamp);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex::encode(hmac(&k_signing, &string_to_sign));
    (signed_headers, scope, signature)
}

/// `signPut` (`s3-put.ts:54-103`) with the body already hashed: virtual-host
/// `"<bucket>.s3.<region>.amazonaws.com"`, path `"/" + encodeS3Key(key)`,
/// signed payload (`x-amz-content-sha256` = hex sha256 of the body), session
/// token signed, `content-length` + `authorization` appended unsigned.
#[allow(clippy::too_many_arguments)]
pub fn sign_put_hashed(
    region: &str,
    bucket: &str,
    key: &str,
    payload_sha256: &str,
    body_len: usize,
    content_type: &str,
    creds: SigningCredentials<'_>,
    now_ms: f64,
) -> SignedRequest {
    let host = format!("{bucket}.s3.{region}.amazonaws.com");
    let path = format!("/{}", encode_s3_key(key));
    let amz_date = amz_date(now_ms);

    let headers: [(&str, &str); 5] = [
        ("content-type", content_type),
        ("host", &host),
        ("x-amz-content-sha256", payload_sha256),
        ("x-amz-date", &amz_date),
        ("x-amz-security-token", creds.session_token),
    ];
    let (signed_headers, scope, signature) = signature_v4(
        "PUT",
        &path,
        &headers,
        payload_sha256,
        &amz_date,
        creds.secret_access_key,
        region,
        "s3",
    );
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    out.push(("content-length".into(), body_len.to_string()));
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    SignedRequest {
        host,
        path,
        headers: out,
    }
}

/// `signPut` (`s3-put.ts:54-103`): hashes `body`, then [`sign_put_hashed`].
pub fn sign_put(
    region: &str,
    bucket: &str,
    key: &str,
    body: &[u8],
    content_type: &str,
    creds: SigningCredentials<'_>,
    now_ms: f64,
) -> SignedRequest {
    sign_put_hashed(
        region,
        bucket,
        key,
        &sha256_hex(body),
        body.len(),
        content_type,
        creds,
        now_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    /// AWS S3 docs, "Signature Calculations for the Authorization Header:
    /// Transferring Payload in a Single Chunk", Example: PUT Object.
    #[test]
    fn aws_published_put_object_vector() {
        let body = b"Welcome to Amazon S3.";
        let sha = sha256_hex(body);
        let (signed, scope, sig) = signature_v4(
            "PUT",
            &format!("/{}", encode_s3_key("test$file.text")),
            &[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("host", "examplebucket.s3.amazonaws.com"),
                ("x-amz-date", "20130524T000000Z"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ("x-amz-content-sha256", &sha),
            ],
            &sha,
            "20130524T000000Z",
            SECRET,
            "us-east-1",
            "s3",
        );
        assert_eq!(
            signed,
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class"
        );
        assert_eq!(scope, "20130524/us-east-1/s3/aws4_request");
        assert_eq!(
            sig,
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    /// Same doc, Example: GET Object (Range header, empty payload hash).
    #[test]
    fn aws_published_get_object_vector() {
        let sha = sha256_hex(b"");
        let (_, _, sig) = signature_v4(
            "GET",
            "/test.txt",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", &sha),
                ("x-amz-date", "20130524T000000Z"),
            ],
            &sha,
            "20130524T000000Z",
            SECRET,
            "us-east-1",
            "s3",
        );
        assert_eq!(
            sig,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn amz_date_truncates_millis() {
        assert_eq!(amz_date(1_835_481_599_999.0), "20280229T235959Z");
        assert_eq!(amz_date(0.0), "19700101T000000Z");
    }
}
