//! `END_GOAL` failure case 7: hostile pages cost bounded memory and time and end in a named
//! refusal. Fixtures live in `tests/hostile-pages/`; the large ones are generated here.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use std::io::Write;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use repurpose_ingest::fetch::{Fetcher, Limits};
use repurpose_ingest::fixture::{PageServer, Reply};
use repurpose_ingest::guard::Policy;
use repurpose_ingest::resolve::Resolver;
use repurpose_ingest::{ExtractError, FetchError, IngestError, extract, ingest};

const GIB: u64 = 1024 * 1024 * 1024;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/hostile-pages/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn fetcher() -> Fetcher {
    Fetcher::new(
        Policy::loopback_fixture(),
        Limits::default(),
        Resolver::default(),
    )
    .unwrap()
}

async fn serve(reply: Reply) -> PageServer {
    PageServer::start(move |_| reply.clone()).await.unwrap()
}

async fn fetch_err(reply: Reply) -> FetchError {
    let server = serve(reply).await;
    fetcher()
        .fetch(&server.url("/"))
        .await
        .map(|_| ())
        .unwrap_err()
}

async fn ingest_page(
    content_type: &str,
    body: Vec<u8>,
) -> Result<repurpose_ingest::Ingested, IngestError> {
    let server = serve(Reply::ok(content_type, body)).await;
    ingest(&fetcher(), &server.url("/post"), extract::Limits::default()).await
}

#[tokio::test]
async fn a_one_gib_body_is_cut_off_at_the_wire_cap() {
    // Declared: refused from the header, before the body is read.
    let declared =
        format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {GIB}\r\n\r\n");
    let err = fetch_err(Reply::Stream {
        head: declared.into_bytes(),
        chunk: vec![b'a'; 64 * 1024],
        times: GIB / (64 * 1024),
    })
    .await;
    assert_eq!(err, FetchError::TooLarge);
    // Undeclared (chunked, never-ending): refused once the cap is crossed.
    let chunk = format!("{:x}\r\n{}\r\n", 64 * 1024, "a".repeat(64 * 1024));
    let err = fetch_err(Reply::Stream {
        head: b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec(),
        chunk: chunk.into_bytes(),
        times: u64::MAX,
    })
    .await;
    assert_eq!(err, FetchError::TooLarge);
}

#[tokio::test]
async fn a_gzip_bomb_is_refused_by_name() {
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    let zeros = vec![b' '; 1024 * 1024];
    for _ in 0..64 {
        enc.write_all(&zeros).unwrap();
    }
    let bomb = enc.finish().unwrap();
    assert!(
        bomb.len() < 128 * 1024,
        "64 MiB of spaces compresses to {} bytes",
        bomb.len()
    );
    let err = fetch_err(Reply::with_headers(
        200,
        &[("content-type", "text/html"), ("content-encoding", "gzip")],
        bomb,
    ))
    .await;
    assert_eq!(err, FetchError::DecompressionBomb);
    let corrupt = fetch_err(Reply::with_headers(
        200,
        &[("content-type", "text/html"), ("content-encoding", "gzip")],
        b"\x1f\x8b\x08\x00garbage",
    ))
    .await;
    assert_eq!(corrupt, FetchError::CorruptEncoding);
    let brotli = fetch_err(Reply::with_headers(
        200,
        &[("content-type", "text/html"), ("content-encoding", "br")],
        b"x",
    ))
    .await;
    assert_eq!(brotli, FetchError::UnsupportedEncoding);
}

#[tokio::test]
async fn a_gzip_page_within_the_cap_is_inflated() {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&fixture("article.html")).unwrap();
    let server = serve(Reply::with_headers(
        200,
        &[("content-type", "text/html"), ("content-encoding", "gzip")],
        enc.finish().unwrap(),
    ))
    .await;
    let done = ingest(&fetcher(), &server.url("/"), extract::Limits::default())
        .await
        .unwrap();
    assert!(done.article.text.contains("37 percent"));
}

#[tokio::test]
async fn endless_redirects_stop_at_the_bound() {
    let server = PageServer::start(|path| {
        let n: u64 = path.trim_start_matches("/r/").parse().unwrap_or(0);
        Reply::redirect(&format!("/r/{}", n + 1))
    })
    .await
    .unwrap();
    let f = fetcher();
    assert_eq!(
        f.fetch(&server.url("/r/0")).await.map(|_| ()),
        Err(FetchError::TooManyRedirects)
    );
    assert_eq!(
        server.requests().len(),
        6,
        "the first request and five redirects"
    );
    let looping = serve(Reply::redirect("/")).await;
    assert_eq!(
        f.fetch(&looping.url("/")).await.map(|_| ()),
        Err(FetchError::TooManyRedirects)
    );
    let nowhere = serve(Reply::with_headers(302, &[], b"")).await;
    assert_eq!(
        f.fetch(&nowhere.url("/")).await.map(|_| ()),
        Err(FetchError::BadRedirect)
    );
}

/// Virtual time: the drip and the deadline run on the paused clock, so the outcome does
/// not depend on machine speed.
#[tokio::test(start_paused = true)]
async fn a_slow_loris_server_hits_the_whole_fetch_deadline() {
    let head =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 100000\r\n\r\n".to_vec();
    // Headers dripped a byte a second: never complete inside 15 s.
    let err = fetch_err(Reply::Drip {
        bytes: head.clone(),
        every: Duration::from_secs(1),
    })
    .await;
    assert_eq!(err, FetchError::Timeout);
    // Headers fast, then a body that drips forever: each byte resets nothing.
    let mut slow_body = head;
    slow_body.extend(std::iter::repeat_n(b'a', 100_000));
    let started = tokio::time::Instant::now();
    let err = fetch_err(Reply::Drip {
        bytes: slow_body,
        every: Duration::from_millis(100),
    })
    .await;
    assert_eq!(err, FetchError::Timeout);
    assert!(started.elapsed() <= Duration::from_secs(16));
    // Accepts and never answers.
    assert_eq!(fetch_err(Reply::Silent).await, FetchError::Timeout);
}

#[tokio::test]
async fn broken_http_and_wrong_types_fail_by_name() {
    assert_eq!(
        fetch_err(Reply::Raw(b"SSH-2.0-OpenSSH_9.6\r\n".to_vec())).await,
        FetchError::Protocol
    );
    let mut huge_header = b"HTTP/1.1 200 OK\r\nx-pad: ".to_vec();
    huge_header.extend(std::iter::repeat_n(b'a', 256 * 1024));
    assert_eq!(
        fetch_err(Reply::Raw(huge_header)).await,
        FetchError::Protocol
    );
    assert_eq!(
        fetch_err(Reply::ok("image/png", b"\x89PNG")).await,
        FetchError::UnsupportedType
    );
    assert_eq!(
        fetch_err(Reply::ok("application/pdf", b"%PDF")).await,
        FetchError::UnsupportedType
    );
    let gone = fetch_err(Reply::with_headers(503, &[], b"")).await;
    assert_eq!(gone, FetchError::Status(503));
    assert!(gone.is_transient());
    assert!(!FetchError::Status(404).is_transient());
}

#[tokio::test]
async fn a_benign_article_extracts() {
    let done = ingest_page("text/html", fixture("article.html"))
        .await
        .unwrap();
    assert_eq!(done.article.title, "Why bounded queues win");
    assert!(done.article.text.contains("37 percent fewer pages"));
    assert!(!done.article.text.contains("Copyright"));
}

#[tokio::test]
async fn a_script_only_page_is_no_article() {
    let err = ingest_page("text/html", fixture("script-only.html"))
        .await
        .unwrap_err();
    assert_eq!(err, IngestError::Extract(ExtractError::NoArticle));
}

#[tokio::test]
async fn malformed_html_extracts_without_markup() {
    let done = ingest_page("text/html", fixture("malformed.html"))
        .await
        .unwrap();
    assert!(done.article.text.contains("Paragraph 5"));
    assert!(!done.article.text.contains("<script"));
    let swallowed = ingest_page("text/html", fixture("unclosed-title.html"))
        .await
        .unwrap_err();
    assert_eq!(swallowed, IngestError::Extract(ExtractError::NoArticle));
}

#[tokio::test]
async fn charset_tricks_decode_by_the_standard() {
    let cyr = ingest_page("text/html", fixture("charset-meta-windows-1251.html"))
        .await
        .unwrap();
    assert!(
        cyr.article.text.contains("очереди с ограничением"),
        "meta charset honoured"
    );
    let utf7 = ingest_page("text/html", fixture("charset-utf7-trick.html"))
        .await
        .unwrap();
    assert!(
        utf7.article.text.contains("+ADw-script+AD4-"),
        "UTF-7 ignored, payload inert"
    );
    let bom = ingest_page(
        "text/html; charset=windows-1252",
        fixture("charset-bom-vs-header.html"),
    )
    .await
    .unwrap();
    assert!(
        bom.article.text.contains("Café paragraph"),
        "BOM beats header and meta"
    );
}

/// Work thresholds for a refused page, fixed before the first run: refusal within two
/// parser input chunks, whatever the page's length.
const MAX_BYTES_FED: usize = 8 * 1024;
const MAX_SINK_OPS: u64 = 4_096;

#[test]
fn nesting_hidden_behind_svg_raw_text_or_a_bang_comment_is_refused_in_bounded_work() {
    for name in [
        "svg-title-nest.html",
        "svg-style-nest.html",
        "comment-bang-nest.html",
    ] {
        let html = String::from_utf8(fixture(name)).unwrap();
        let (out, work) = extract::parse_work(&html, &extract::Limits::default());
        assert_eq!(out, Err(ExtractError::TooDeep), "{name}");
        assert!(work.bytes_fed <= MAX_BYTES_FED, "{name}: {work:?}");
        assert!(work.sink_ops <= MAX_SINK_OPS, "{name}: {work:?}");
        // Eight times the nesting costs the same work: refusal does not wait for the end.
        let bigger = html.replace(&"<div>".repeat(5_000), &"<div>".repeat(40_000));
        assert!(bigger.len() > 7 * html.len());
        let (out, more) = extract::parse_work(&bigger, &extract::Limits::default());
        assert_eq!(out, Err(ExtractError::TooDeep), "{name}");
        assert_eq!(more, work, "{name}");
    }
}

#[tokio::test]
async fn nesting_behind_svg_raw_text_or_a_bang_comment_ends_named() {
    for name in [
        "svg-title-nest.html",
        "svg-style-nest.html",
        "comment-bang-nest.html",
    ] {
        let err = ingest_page("text/html", fixture(name)).await.unwrap_err();
        assert_eq!(err, IngestError::Extract(ExtractError::TooDeep), "{name}");
    }
}
