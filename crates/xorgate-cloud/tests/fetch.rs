//! `xorgate_cloud::fetch::fetch_pinned` against a local server: the file
//! counts only when its size AND sha256 are the pinned ones, a redirect is
//! followed only where the caller allows it, every bound holds, and no
//! partial file is ever left behind.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};
use xorgate_cloud::fetch::{fetch_client, fetch_pinned, FetchLimits, Pinned};

#[path = "support/http_stub.rs"]
mod http_stub;
use http_stub::{serve, Reply};

const BODY: &[u8] = b"a tiny package, sixty-four bytes of it or so, for the pin test.\n";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-fetch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn limits() -> FetchLimits {
    FetchLimits {
        connect: Duration::from_secs(5),
        read: Duration::from_secs(5),
        total: Duration::from_secs(20),
    }
}

/// Fetch `url` pinned to [`BODY`], with redirects allowed only back to the
/// stub itself (`http://127.0.0.1:<port>/…`).
async fn fetch(
    url: &str,
    port: u16,
    dest: &std::path::Path,
    limits: &FetchLimits,
) -> Result<(), String> {
    let client = fetch_client(limits).unwrap();
    let home = format!("http://127.0.0.1:{port}/");
    let pin_sha = sha(BODY);
    fetch_pinned(
        &client,
        Pinned {
            url,
            bytes: BODY.len() as u64,
            sha256: &pin_sha,
        },
        dest,
        &move |to: &str| {
            if to.starts_with(&home) {
                Ok(())
            } else {
                Err(format!("a redirect to a host that is not allowed ({to})"))
            }
        },
        4,
        limits,
    )
    .await
}

#[tokio::test]
async fn the_pinned_file_is_accepted_and_nothing_else() {
    let stub = serve(|path, port| match path {
        "/ok" => Reply::ok(BODY),
        "/ok-unsized" => Reply::unsized_body(BODY),
        // The right size, other bytes.
        "/wrong-hash" => Reply::ok(&BODY.iter().map(|b| b ^ 1).collect::<Vec<u8>>()),
        // Announces another size than the pin.
        "/wrong-size" => Reply::ok(&BODY[..10]),
        // No announced size, more bytes than the pin.
        "/bigger" => Reply::unsized_body(&[BODY, BODY].concat()),
        // No announced size, fewer bytes than the pin.
        "/shorter" => Reply::unsized_body(&BODY[..20]),
        // Announces the pinned size and stops early.
        "/truncated" => Reply::announcing(&BODY[..20], BODY.len() as u64),
        "/moved" => Reply::redirect(&format!("http://127.0.0.1:{port}/ok")),
        "/moved-twice" => Reply::redirect(&format!("http://127.0.0.1:{port}/moved")),
        "/foreign" => Reply::redirect("https://evil.example/libcamera.deb"),
        "/relative" => Reply::redirect("/ok"),
        "/loop" => Reply::redirect(&format!("http://127.0.0.1:{port}/loop")),
        "/no-location" => Reply::status(302),
        "/hang" => Reply::Hang,
        _ => Reply::status(404),
    })
    .await;
    let dir = scratch("pin");
    let dest = dir.join("pkg.deb");
    let l = limits();

    // Accepted: with and without an announced length, and through
    // redirects the caller allows. The file is exactly the pinned bytes,
    // readable by its owner only.
    for path in ["/ok", "/ok-unsized", "/moved", "/moved-twice"] {
        assert_eq!(
            fetch(&stub.url(path), stub.port, &dest, &l).await,
            Ok(()),
            "{path}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), BODY, "{path}");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{path}");
        std::fs::remove_file(&dest).unwrap();
    }

    // Refused, each with its reason, and NO file left behind.
    let refused: [(&str, &str); 9] = [
        ("/wrong-hash", "sha256 mismatch: the file is "),
        (
            "/wrong-size",
            "the server announces 10 bytes, the catalog pins 64",
        ),
        ("/bigger", "the download is larger than the pinned 64 bytes"),
        (
            "/shorter",
            "the download ended after 20 bytes, the catalog pins 64",
        ),
        ("/truncated", "the download broke off after "),
        (
            "/foreign",
            "a redirect to a host that is not allowed (https://evil.example/libcamera.deb)",
        ),
        (
            "/relative",
            "a redirect to a host that is not allowed (/ok)",
        ),
        ("/loop", "more than 4 redirects"),
        ("/missing", "the server answered HTTP 404"),
    ];
    assert_eq!(BODY.len(), 64);
    for (path, why) in refused {
        let err = fetch(&stub.url(path), stub.port, &dest, &l)
            .await
            .unwrap_err();
        assert!(err.starts_with(why), "{path}: {err}");
        assert!(!dest.exists(), "{path}: a partial file was left");
    }
    let err = fetch(&stub.url("/no-location"), stub.port, &dest, &l)
        .await
        .unwrap_err();
    assert_eq!(err, "HTTP 302 without a usable Location header");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "the directory is empty"
    );

    // A redirect that is refused is never followed: the stub saw one
    // request for it, and nothing went to the other host.
    let before = stub.hits();
    let _ = fetch(&stub.url("/foreign"), stub.port, &dest, &l).await;
    assert_eq!(stub.hits(), before + 1);

    // A file (or a link) already at the destination is never written through.
    std::fs::write(&dest, b"mine").unwrap();
    let err = fetch(&stub.url("/ok"), stub.port, &dest, &l)
        .await
        .unwrap_err();
    assert!(err.starts_with("cannot create "), "{err}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"mine", "left as it was");
    std::fs::remove_file(&dest).unwrap();
    let target = dir.join("elsewhere");
    std::os::unix::fs::symlink(&target, &dest).unwrap();
    let err = fetch(&stub.url("/ok"), stub.port, &dest, &l)
        .await
        .unwrap_err();
    assert!(err.starts_with("cannot create "), "{err}");
    assert!(!target.exists(), "nothing was written through the link");
    std::fs::remove_file(&dest).unwrap();

    // A server that never answers: the read bound, then the total bound.
    let quick = FetchLimits {
        read: Duration::from_millis(300),
        ..l
    };
    let err = fetch(&stub.url("/hang"), stub.port, &dest, &quick)
        .await
        .unwrap_err();
    assert!(err.starts_with("the request failed: timed out"), "{err}");
    let capped = FetchLimits {
        total: Duration::from_millis(300),
        ..l
    };
    let err = fetch(&stub.url("/hang"), stub.port, &dest, &capped)
        .await
        .unwrap_err();
    assert_eq!(err, "the download did not finish within 1 s");
    assert!(!dest.exists());

    // Nobody listening.
    let port = stub.port;
    drop(stub);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = fetch(&format!("http://127.0.0.1:{port}/ok"), port, &dest, &l)
        .await
        .unwrap_err();
    assert!(err.starts_with("the request failed: "), "{err}");
    assert!(!dest.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
