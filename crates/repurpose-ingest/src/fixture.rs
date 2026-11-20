//! A raw-TCP page server for fetcher tests: it speaks just enough HTTP/1.1 to serve
//! well-formed, hostile and broken answers, and records every request line it reads.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// How the fixture answers one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// Bytes written verbatim, then the connection closes.
    Raw(Vec<u8>),
    /// `head` verbatim, then `chunk` repeated `times` (`u64::MAX`: until the client leaves).
    Stream {
        head: Vec<u8>,
        chunk: Vec<u8>,
        times: u64,
    },
    /// `bytes` written one byte per `every`.
    Drip { bytes: Vec<u8>, every: Duration },
    /// Accepts and reads, never answers.
    Silent,
}

impl Reply {
    /// A complete response with `Content-Length`.
    pub fn ok(content_type: &str, body: impl AsRef<[u8]>) -> Self {
        Self::with_headers(200, &[("content-type", content_type)], body)
    }

    pub fn with_headers(status: u16, headers: &[(&str, &str)], body: impl AsRef<[u8]>) -> Self {
        let body = body.as_ref();
        let mut out =
            format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n", body.len()).into_bytes();
        for (name, value) in headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"connection: close\r\n\r\n");
        out.extend_from_slice(body);
        Self::Raw(out)
    }

    pub fn redirect(location: &str) -> Self {
        Self::with_headers(302, &[("location", location)], b"")
    }
}

type Route = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

/// A running fixture. Dropping it stops the accept loop.
pub struct PageServer {
    pub port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PageServer {
    /// Serves `route(path)` for every request on an ephemeral loopback port.
    pub async fn start(
        route: impl Fn(&str) -> Reply + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let route: Route = Arc::new(route);
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let route = Arc::clone(&route);
                let seen = Arc::clone(&seen);
                tokio::spawn(serve(stream, route, seen));
            }
        });
        Ok(Self {
            port,
            requests,
            task,
        })
    }

    /// `http://127.0.0.1:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The request lines read so far (`GET /path HTTP/1.1`).
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

async fn serve(mut stream: TcpStream, route: Route, seen: Arc<Mutex<Vec<String>>>) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 64 * 1024 {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next().unwrap_or("").to_owned();
    let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
    if let Ok(mut seen) = seen.lock() {
        seen.push(line);
    }
    match route(&path) {
        Reply::Raw(bytes) => {
            let _ = stream.write_all(&bytes).await;
        }
        Reply::Stream { head, chunk, times } => {
            if stream.write_all(&head).await.is_err() {
                return;
            }
            let mut sent = 0;
            while sent < times {
                if stream.write_all(&chunk).await.is_err() {
                    return;
                }
                sent += 1;
            }
        }
        Reply::Drip { bytes, every } => {
            for byte in bytes {
                if stream.write_all(&[byte]).await.is_err() {
                    return;
                }
                tokio::time::sleep(every).await;
            }
        }
        Reply::Silent => {
            let _ = stream.read(&mut buf).await;
            std::future::pending::<()>().await;
        }
    }
    let _ = stream.shutdown().await;
}
