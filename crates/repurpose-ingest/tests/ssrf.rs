//! `END_GOAL` failure case 6: SSRF via the article URL is refused before any connect.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use std::net::IpAddr;
use std::time::Duration;

use repurpose_ingest::fetch::{Fetcher, Limits};
use repurpose_ingest::fixture::{PageServer, Reply};
use repurpose_ingest::guard::Policy;
use repurpose_ingest::resolve::{Canned, Resolver};
use repurpose_ingest::{FetchError, Refusal};

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn fetcher(policy: Policy, resolver: Resolver) -> Fetcher {
    let limits = Limits {
        deadline: Duration::from_secs(10),
        ..Limits::default()
    };
    Fetcher::new(policy, limits, resolver).unwrap()
}

const REFUSED: FetchError = FetchError::Refused(Refusal::Address);

#[tokio::test]
async fn ssrf_guard_refuses_metadata_ip() {
    let f = fetcher(Policy::default(), Resolver::default());
    for url in [
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        "http://169.254.170.2/v2/credentials",
        "http://[fd00:ec2::254]/latest/meta-data/",
        "http://[::ffff:169.254.169.254]/",
        "http://[64:ff9b::a9fe:a9fe]/",
        "http://2852039166/",
        "http://0xa9.0xfe.0xa9.0xfe/",
    ] {
        assert_eq!(f.fetch(url).await.map(|_| ()), Err(REFUSED), "{url}");
    }
    assert_eq!(
        f.fetch("http://metadata.google.internal/computeMetadata/v1/")
            .await
            .map(|_| ()),
        Err(FetchError::Refused(Refusal::LocalName))
    );
}

#[tokio::test]
async fn ssrf_guard_refuses_private_range() {
    let canned = Canned::new(&[
        ("private.example.com", &[&[ip("10.0.0.5")]]),
        (
            "mixed.example.com",
            &[&[ip("93.184.215.14"), ip("192.168.1.10")]],
        ),
        ("mapped.example.com", &[&[ip("::ffff:127.0.0.1")]]),
        ("cgnat.example.com", &[&[ip("100.100.100.200")]]),
        ("v6local.example.com", &[&[ip("fe80::1")]]),
    ]);
    let f = fetcher(Policy::default(), Resolver::Canned(canned.clone()));
    for url in [
        "http://private.example.com/",
        "https://mixed.example.com/",
        "http://mapped.example.com/",
        "http://cgnat.example.com/",
        "http://v6local.example.com/",
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://0.0.0.0/",
        "http://172.16.5.4/",
    ] {
        assert_eq!(f.fetch(url).await.map(|_| ()), Err(REFUSED), "{url}");
    }
    // The fixture policy admits 127.0.0.1 and nothing else: a mapped spelling of it is refused.
    let lab = fetcher(Policy::loopback_fixture(), Resolver::Canned(canned));
    assert_eq!(
        lab.fetch("http://mapped.example.com:8080/")
            .await
            .map(|_| ()),
        Err(REFUSED)
    );
}

#[tokio::test]
async fn ssrf_guard_refuses_redirect_to_private() {
    let canned = Canned::new(&[("internal.example.com", &[&[ip("192.168.0.1")]])]);
    let server = PageServer::start(|path| match path {
        "/to-metadata" => Reply::redirect("http://169.254.169.254/latest/meta-data/"),
        "/to-name" => Reply::redirect("http://internal.example.com/admin"),
        "/to-file" => Reply::redirect("file:///etc/passwd"),
        "/to-gopher" => Reply::redirect("gopher://127.0.0.1:6379/_FLUSHALL"),
        "/to-port" => Reply::redirect("http://example.com:6379/"),
        _ => Reply::ok("text/html", "unreachable"),
    })
    .await
    .unwrap();
    let f = fetcher(Policy::loopback_fixture(), Resolver::Canned(canned));
    let cases = [
        ("/to-metadata", REFUSED),
        ("/to-name", REFUSED),
        ("/to-file", FetchError::Refused(Refusal::Scheme)),
        ("/to-gopher", FetchError::Refused(Refusal::Scheme)),
    ];
    for (path, want) in cases {
        assert_eq!(
            f.fetch(&server.url(path)).await.map(|_| ()),
            Err(want),
            "{path}"
        );
    }
    // Each fetch reached the fixture exactly once: no refused hop was dialled.
    assert_eq!(server.requests().len(), cases.len());
}

#[tokio::test]
async fn ssrf_guard_refuses_dns_rebinding() {
    let server = PageServer::start(|path| match path {
        "/first" => Reply::redirect("/second"),
        _ => Reply::ok("text/html", "second hop"),
    })
    .await
    .unwrap();
    // The name answers loopback (allowed in the fixture policy) once, then the metadata
    // address — a rebinding attack. Each hop is a fresh lookup that is checked again, and
    // each connect goes to the address that was checked, so the rebound answer is refused.
    let canned = Canned::new(&[(
        "rebind.example.com",
        &[&[ip("127.0.0.1")], &[ip("169.254.169.254")]],
    )]);
    let f = fetcher(Policy::loopback_fixture(), Resolver::Canned(canned.clone()));
    let url = format!("http://rebind.example.com:{}/first", server.port);
    assert_eq!(f.fetch(&url).await.map(|_| ()), Err(REFUSED));
    assert_eq!(server.requests(), vec!["GET /first HTTP/1.1".to_owned()]);
    assert_eq!(
        canned.lookups().len(),
        2,
        "one lookup per hop, none between check and connect"
    );
}

#[tokio::test]
async fn a_public_page_behind_a_checked_name_is_fetched() {
    let server = PageServer::start(|_| Reply::ok("text/html; charset=utf-8", "<p>hello</p>"))
        .await
        .unwrap();
    let canned = Canned::new(&[("blog.example.com", &[&[ip("127.0.0.1")]])]);
    let f = fetcher(Policy::loopback_fixture(), Resolver::Canned(canned));
    let page = f
        .fetch(&format!(
            "http://blog.example.com:{}/post?id=1#frag",
            server.port
        ))
        .await
        .unwrap();
    assert_eq!(page.body, b"<p>hello</p>");
    assert_eq!(
        server.requests(),
        vec!["GET /post?id=1 HTTP/1.1".to_owned()]
    );
}
