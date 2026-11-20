//! The bounded, SSRF-guarded HTTP(S) fetcher.
//!
//! One whole-fetch deadline covers every lookup, connect, handshake, header and body
//! read across all redirect hops, so a slow-drip server costs at most that long. Each
//! hop is checked by the [`guard`](crate::guard) before its lookup, every answer
//! before its connect, and the socket is opened to the checked address itself.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::TlsConnector;
use url::{Host, Position, Url};

use crate::decode;
use crate::error::{FetchError, Refusal};
use crate::guard::{self, MAX_URL_BYTES, Policy};
use crate::resolve::Resolver;

const USER_AGENT: &str = "content-repurposing-fetcher/0.1";
const ACCEPT: &str = "text/html,application/xhtml+xml,text/plain;q=0.8";

/// Response header bytes read before the exchange is abandoned.
const MAX_HEADER_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Redirects followed; one more is `TooManyRedirects`.
    pub max_redirects: usize,
    /// Body bytes read off the wire (compressed size).
    pub max_wire_bytes: usize,
    /// Body bytes after decompression.
    pub max_decoded_bytes: usize,
    /// Whole fetch, all hops.
    pub deadline: Duration,
    /// Each TCP connect, within the whole deadline.
    pub connect_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // LIMIT(fetch-redirects)
            max_redirects: 5,
            // LIMIT(fetch-wire-bytes)
            max_wire_bytes: 2 * 1024 * 1024,
            // LIMIT(fetch-decoded-bytes)
            max_decoded_bytes: 4 * 1024 * 1024,
            // LIMIT(fetch-deadline)
            deadline: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// A fetched page: the final URL after redirects and its decoded (uncompressed) body.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: Url,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub redirects: usize,
}

#[derive(Clone)]
pub struct Fetcher {
    policy: Policy,
    limits: Limits,
    resolver: Resolver,
    tls: TlsConnector,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("policy", &self.policy)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

enum Hop {
    Redirect(Url),
    Page(Option<String>, Vec<u8>),
}

/// Aborts the connection task when the exchange ends, however it ends.
struct Driver(JoinHandle<()>);

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Fetcher {
    pub fn new(policy: Policy, limits: Limits, resolver: Resolver) -> Result<Self, FetchError> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| FetchError::Tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            policy,
            limits,
            resolver,
            tls: TlsConnector::from(Arc::new(config)),
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Fetches `raw`, following at most `max_redirects` re-checked redirects.
    pub async fn fetch(&self, raw: &str) -> Result<Fetched, FetchError> {
        let mut url = guard::check_url(raw, self.policy)?;
        let deadline = Instant::now() + self.limits.deadline;
        let mut redirects = 0;
        loop {
            match self.hop(&url, deadline).await? {
                Hop::Page(content_type, body) => {
                    return Ok(Fetched {
                        url,
                        content_type,
                        body,
                        redirects,
                    });
                }
                Hop::Redirect(next) => {
                    if redirects >= self.limits.max_redirects {
                        return Err(FetchError::TooManyRedirects);
                    }
                    redirects += 1;
                    url = next;
                }
            }
        }
    }

    async fn hop(&self, url: &Url, deadline: Instant) -> Result<Hop, FetchError> {
        guard::check_parsed(url, self.policy)?;
        let port = url
            .port_or_known_default()
            .ok_or(FetchError::Refused(Refusal::BadUrl))?;
        let ip = self.address(url, port, deadline).await?;
        let connect_by = deadline.min(Instant::now() + self.limits.connect_timeout);
        let tcp = timeout_at(connect_by, TcpStream::connect(SocketAddr::new(ip, port)))
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|_| FetchError::Connect)?;
        let _ = tcp.set_nodelay(true);
        if url.scheme() == "https" {
            let name = match url.host() {
                Some(Host::Domain(name)) => {
                    ServerName::try_from(name.to_owned()).map_err(|_| FetchError::Tls)?
                }
                _ => ServerName::IpAddress(ip.into()),
            };
            let tls = timeout_at(deadline, self.tls.connect(name, tcp))
                .await
                .map_err(|_| FetchError::Timeout)?
                .map_err(|_| FetchError::Tls)?;
            self.exchange(TokioIo::new(tls), url, deadline).await
        } else {
            self.exchange(TokioIo::new(tcp), url, deadline).await
        }
    }

    /// The one address this hop dials: a literal, or a looked-up name whose every answer
    /// passed the policy (a name with any internal answer is refused outright).
    async fn address(&self, url: &Url, port: u16, deadline: Instant) -> Result<IpAddr, FetchError> {
        let candidates = match url.host() {
            Some(Host::Ipv4(v4)) => vec![IpAddr::V4(v4)],
            Some(Host::Ipv6(v6)) => vec![IpAddr::V6(v6)],
            Some(Host::Domain(name)) => timeout_at(deadline, self.resolver.lookup(name, port))
                .await
                .map_err(|_| FetchError::Timeout)?
                .map_err(|_| FetchError::Dns)?,
            None => return Err(Refusal::BadUrl.into()),
        };
        if candidates.iter().any(|&ip| !self.policy.allows_ip(ip)) {
            return Err(Refusal::Address.into());
        }
        candidates.first().copied().ok_or(FetchError::Dns)
    }

    async fn exchange<T>(&self, io: T, url: &Url, deadline: Instant) -> Result<Hop, FetchError>
    where
        T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut sender, conn) = timeout_at(
            deadline,
            hyper::client::conn::http1::Builder::new()
                .max_buf_size(MAX_HEADER_BYTES)
                .handshake::<_, Empty<Bytes>>(io),
        )
        .await
        .map_err(|_| FetchError::Timeout)?
        .map_err(|_| FetchError::Protocol)?;
        let _driver = Driver(tokio::spawn(async move {
            let _ = conn.await;
        }));
        let request = Request::get(&url[Position::BeforePath..Position::AfterQuery])
            .header(
                header::HOST,
                &url[Position::BeforeHost..Position::AfterPort],
            )
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, ACCEPT)
            .header(header::ACCEPT_ENCODING, "gzip")
            .header(header::CONNECTION, "close")
            .body(Empty::new())
            .map_err(|_| FetchError::Refused(Refusal::BadUrl))?;
        let response = timeout_at(deadline, sender.send_request(request))
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|_| FetchError::Protocol)?;
        self.respond(response, url, deadline).await
    }

    async fn respond(
        &self,
        response: Response<Incoming>,
        url: &Url,
        deadline: Instant,
    ) -> Result<Hop, FetchError> {
        let status = response.status();
        if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
            return redirect_target(url, response.headers(), self.policy).map(Hop::Redirect);
        }
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        let headers = response.headers();
        let content_type = header_text(headers, &header::CONTENT_TYPE);
        if !content_type.as_deref().is_none_or(decode::is_textual) {
            return Err(FetchError::UnsupportedType);
        }
        let gzip = match header_text(headers, &header::CONTENT_ENCODING)
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "identity") => false,
            Some("gzip" | "x-gzip") => true,
            Some(_) => return Err(FetchError::UnsupportedEncoding),
        };
        let declared = header_text(headers, &header::CONTENT_LENGTH)
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > self.limits.max_wire_bytes as u64) {
            return Err(FetchError::TooLarge);
        }
        let mut body = response.into_body();
        let mut wire = Vec::new();
        loop {
            let frame = timeout_at(deadline, body.frame())
                .await
                .map_err(|_| FetchError::Timeout)?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|_| FetchError::Protocol)?;
            if let Ok(data) = frame.into_data() {
                if wire.len() + data.len() > self.limits.max_wire_bytes {
                    return Err(FetchError::TooLarge);
                }
                wire.extend_from_slice(&data);
            }
        }
        let body = if gzip {
            decode::gunzip(&wire, self.limits.max_decoded_bytes)?
        } else {
            if wire.len() > self.limits.max_decoded_bytes {
                return Err(FetchError::TooLarge);
            }
            wire
        };
        Ok(Hop::Page(content_type, body))
    }
}

fn header_text(headers: &HeaderMap, name: &header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// The next hop of a redirect, checked like a pasted URL.
fn redirect_target(base: &Url, headers: &HeaderMap, policy: Policy) -> Result<Url, FetchError> {
    let location = headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(FetchError::BadRedirect)?;
    if location.len() > MAX_URL_BYTES {
        return Err(Refusal::UrlTooLong.into());
    }
    let next = base.join(location).map_err(|_| FetchError::BadRedirect)?;
    if next.as_str().len() > MAX_URL_BYTES {
        return Err(Refusal::UrlTooLong.into());
    }
    guard::check_parsed(&next, policy)?;
    Ok(next)
}
