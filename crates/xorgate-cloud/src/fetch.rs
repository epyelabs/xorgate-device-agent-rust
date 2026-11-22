//! Download ONE file whose size and sha256 are PINNED by the caller
//! (RUST-ONLY, support-other-camera-models Phase 4: the extra OS packages
//! of a camera sensor, `xorgate_core::camera_packages`).
//!
//! The rules, all enforced here:
//! - the URL is the caller's (already checked by it); a REDIRECT is
//!   followed only when the caller's `redirect_ok` accepts the new URL, at
//!   most `max_redirects` times, and never by the HTTP library itself (the
//!   client follows none, so no hop escapes the check);
//! - no credentials, cookies or proxy: one plain GET per hop;
//! - a `Content-Length` other than the pinned size is refused before a byte
//!   is read; the body is streamed to `dest` and abandoned as soon as it
//!   grows past the pinned size;
//! - the file counts only when its size AND its sha256 are the pinned ones;
//! - on ANY failure (and on a timeout) `dest` is removed: no partial file
//!   is left behind;
//! - `dest` is created new (`O_EXCL`), mode 0600: an existing file or a
//!   symlink at that path is never written through.
//!
//! Nothing here decides what to download or what to do with the file.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// The time bounds of one download.
#[derive(Debug, Clone, Copy)]
pub struct FetchLimits {
    /// Per hop: the TCP + TLS connect.
    pub connect: Duration,
    /// Between two reads of the response.
    pub read: Duration,
    /// The whole download, redirects included.
    pub total: Duration,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(20),
            read: Duration::from_secs(30),
            total: Duration::from_secs(300),
        }
    }
}

/// What must come back.
#[derive(Debug, Clone, Copy)]
pub struct Pinned<'a> {
    pub url: &'a str,
    pub bytes: u64,
    /// 64 lowercase hex digits.
    pub sha256: &'a str,
}

/// The client of [`fetch_pinned`]: no proxy from the environment, NO
/// redirect followed by the library, the connect and read bounds of
/// `limits`, the platform's root certificates.
pub fn fetch_client(limits: &FetchLimits) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(limits.connect)
        .read_timeout(limits.read)
        .pool_max_idle_per_host(0)
        .build()
}

/// The innermost cause of a reqwest error, as text (its `Display` alone
/// says only "error sending request").
fn cause(e: &reqwest::Error) -> String {
    let mut src: &dyn std::error::Error = e;
    while let Some(next) = src.source() {
        src = next;
    }
    let text = src.to_string();
    if e.is_timeout() {
        format!("timed out ({text})")
    } else {
        text
    }
}

/// Download `pin.url` to `dest`. `Ok(())` = `dest` holds exactly the pinned
/// bytes. `Err` = one sentence fragment saying why not; `dest` does not
/// exist then.
pub async fn fetch_pinned(
    client: &reqwest::Client,
    pin: Pinned<'_>,
    dest: &Path,
    redirect_ok: &(dyn Fn(&str) -> Result<(), String> + Sync),
    max_redirects: usize,
    limits: &FetchLimits,
) -> Result<(), String> {
    let mut created = false;
    let res = match tokio::time::timeout(
        limits.total,
        fetch_inner(client, pin, dest, redirect_ok, max_redirects, &mut created),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(format!(
            "the download did not finish within {} s",
            limits.total.as_secs().max(1)
        )),
    };
    if res.is_err() && created {
        let _ = tokio::fs::remove_file(dest).await;
    }
    res
}

async fn fetch_inner(
    client: &reqwest::Client,
    pin: Pinned<'_>,
    dest: &Path,
    redirect_ok: &(dyn Fn(&str) -> Result<(), String> + Sync),
    max_redirects: usize,
    created: &mut bool,
) -> Result<(), String> {
    let mut url = pin.url.to_string();
    let mut hops = 0usize;
    let mut resp = loop {
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("the request failed: {}", cause(&e)))?;
        let status = resp.status();
        if status.is_redirection() {
            hops += 1;
            if hops > max_redirects {
                return Err(format!("more than {max_redirects} redirects"));
            }
            let Some(next) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(format!(
                    "HTTP {} without a usable Location header",
                    status.as_u16()
                ));
            };
            redirect_ok(next)?;
            url = next.to_string();
            continue;
        }
        if status != reqwest::StatusCode::OK {
            return Err(format!("the server answered HTTP {}", status.as_u16()));
        }
        break resp;
    };
    if let Some(len) = resp.content_length() {
        if len != pin.bytes {
            return Err(format!(
                "the server announces {len} bytes, the catalog pins {}",
                pin.bytes
            ));
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    *created = true;
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("the download broke off after {got} bytes: {}", cause(&e)))?
    {
        got += chunk.len() as u64;
        if got > pin.bytes {
            return Err(format!(
                "the download is larger than the pinned {} bytes",
                pin.bytes
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    }
    if got != pin.bytes {
        return Err(format!(
            "the download ended after {got} bytes, the catalog pins {}",
            pin.bytes
        ));
    }
    file.sync_all()
        .await
        .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    let sum = hex::encode(hasher.finalize());
    if sum != pin.sha256 {
        return Err(format!(
            "sha256 mismatch: the file is {sum}, the catalog pins {}",
            pin.sha256
        ));
    }
    Ok(())
}
