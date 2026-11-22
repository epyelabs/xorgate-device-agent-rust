//! `xorgate-cloud` — the cloud plane of the Rust device agent (plan README
//! §3.1): the AWS IoT Core MQTT v5 client wrapper over `rumqttc` + `rustls`.
//!
//! Phase 0 ships `mqtt` (connect/LWT/resubscribe/backoff/state watch) and
//! `tls` (mTLS client config from PEM; Amazon Root CA 1 is bundled for callers
//! that pass no CA — the agent always passes `CA_PATH`, unreadable = fatal). Phase 2 WP5 adds `iot_creds` (role-alias credentials over
//! mTLS), `sigv4` (pure signer) and `s3` (`ObjectPut` + the S3 PUT transport);
//! the uploader that consumes them is Phase 3.

pub mod backoff;
pub mod fetch;
pub mod iot_creds;
pub mod mqtt;
pub mod s3;
pub mod sigv4;
pub mod tls;
