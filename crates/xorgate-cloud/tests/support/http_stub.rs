//! A tiny HTTP/1.1 server for the pinned-download tests
//! (`xorgate_cloud::fetch`): one request per connection, the reply chosen
//! by the request path. It can lie about its length, stop early, redirect,
//! or never answer.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the server does with one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `status`, extra headers, the body; `length` = the `Content-Length`
    /// it announces (`None` = none: the body ends when the socket closes).
    Send {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        length: Option<u64>,
    },
    /// Read the request and never answer.
    Hang,
}

impl Reply {
    /// 200 with a truthful `Content-Length`.
    pub fn ok(body: &[u8]) -> Self {
        Reply::Send {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
            length: Some(body.len() as u64),
        }
    }

    /// 200 with no `Content-Length`.
    pub fn unsized_body(body: &[u8]) -> Self {
        Reply::Send {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
            length: None,
        }
    }

    /// 200 announcing `length` and sending `body` (a lie, or a truncation).
    pub fn announcing(body: &[u8], length: u64) -> Self {
        Reply::Send {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
            length: Some(length),
        }
    }

    pub fn redirect(to: &str) -> Self {
        Reply::Send {
            status: 302,
            headers: vec![("Location".to_string(), to.to_string())],
            body: Vec::new(),
            length: Some(0),
        }
    }

    pub fn status(status: u16) -> Self {
        Reply::Send {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            length: Some(0),
        }
    }
}

/// A running stub: `http://127.0.0.1:<port>`; `hits` counts the requests.
pub struct HttpStub {
    pub port: u16,
    pub hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl HttpStub {
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for HttpStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start a stub; `route(path, port)` picks the reply of each request.
pub async fn serve(route: impl Fn(&str, u16) -> Reply + Send + Sync + 'static) -> HttpStub {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let (route, counter) = (Arc::new(route), hits.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (route, counter) = (route.clone(), counter.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&head);
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                match route(&path, port) {
                    Reply::Hang => {
                        // Hold the socket open until the client gives up.
                        let _ = sock.read(&mut buf).await;
                        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                    }
                    Reply::Send {
                        status,
                        headers,
                        body,
                        length,
                    } => {
                        let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                        for (k, v) in headers {
                            out.push_str(&format!("{k}: {v}\r\n"));
                        }
                        if let Some(n) = length {
                            out.push_str(&format!("Content-Length: {n}\r\n"));
                        }
                        out.push_str("\r\n");
                        let _ = sock.write_all(out.as_bytes()).await;
                        let _ = sock.write_all(&body).await;
                        let _ = sock.shutdown().await;
                    }
                }
            });
        }
    });
    HttpStub { port, hits, task }
}
