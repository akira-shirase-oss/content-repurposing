//! The sources routes through the whole router: `END_GOAL` case 7 (another workspace's
//! sources are 404 on every route), API tokens read but never submit, refused URLs are
//! named before anything is queued, and a retried POST is a no-op.
#![expect(clippy::unwrap_used, reason = "integration test helpers")]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use repurpose_accounts::mail::SmtpRelay;
use repurpose_accounts::testing::SmtpSink;
use repurpose_accounts::{Accounts, Settings, store, tokens};
use repurpose_db::testing::TestDb;
use repurpose_http::stack::{ErrorWindow, StackLimits};
use repurpose_server::health::Health;
use repurpose_server::{AppState, app};
use repurpose_sources::spend::Caps;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

struct Kit {
    db: TestDb,
    app: Router,
    sink: SmtpSink,
}

#[derive(Clone, Copy)]
enum Auth<'a> {
    Session(&'a str),
    Token(&'a str),
    None,
}

impl Kit {
    async fn new() -> Self {
        let db = TestDb::new().await;
        let sink = SmtpSink::start("127.0.0.1:0", None).await.unwrap();
        let settings = Settings {
            public_url: "https://app.example.test".to_owned(),
            cookie_secure: false,
            ..Settings::default()
        };
        let accounts = Accounts::new(
            db.pool.clone(),
            settings,
            Arc::new(SmtpRelay::new(sink.addr).unwrap()),
        )
        .await
        .unwrap();
        let errors = Arc::new(ErrorWindow::default());
        let app = app(AppState {
            sources: repurpose_sources::Sources::new(
                accounts.clone(),
                db.pool.clone(),
                Caps::default(),
            ),
            accounts,
            health: Health::new(
                db.pool.clone(),
                std::time::Duration::from_secs(2),
                errors.clone(),
                50,
                20,
            ),
            errors,
            stack: StackLimits::default(),
            max_in_flight: 64,
        });
        Self { db, app, sink }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        auth: Auth<'_>,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        match auth {
            Auth::Session(id) => {
                request = request.header(header::COOKIE, format!("repurpose_session={id}"));
            }
            Auth::Token(t) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {t}"));
            }
            Auth::None => {}
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = match body {
            Some(v) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let mut request = request.body(body).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "203.0.113.7:1000".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn user(&self, email: &str) -> String {
        let code = tokens::new_invite().unwrap();
        store::create_invites(
            &self.db.pool,
            &[tokens::digest(&code)],
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
        let body = json!({ "invite": code, "email": email, "password": PASSWORD });
        let (status, _) = self
            .call(Method::POST, "/v1/signup", Auth::None, None, Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let token = self.sink.latest_to(email).await.unwrap().token().unwrap();
        let (status, _) = self
            .call(
                Method::POST,
                "/v1/verify",
                Auth::None,
                None,
                Some(json!({ "token": token })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let mut request = Request::post("/v1/session")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "203.0.113.7:1000".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        cookie
            .strip_prefix("repurpose_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn workspace(&self, session: &str) -> Uuid {
        let (status, body) = self
            .call(
                Method::POST,
                "/v1/workspaces",
                Auth::Session(session),
                None,
                Some(json!({ "name": "w" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        body["id"].as_str().unwrap().parse().unwrap()
    }

    async fn token(&self, session: &str, ws: Uuid) -> String {
        let (status, body) = self
            .call(
                Method::POST,
                &format!("/v1/workspaces/{ws}/tokens"),
                Auth::Session(session),
                None,
                Some(json!({ "name": "ci" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        body["token"].as_str().unwrap().to_owned()
    }
}

fn url(u: &str) -> Option<Value> {
    (!u.is_empty()).then(|| json!({ "url": u }))
}

#[tokio::test]
async fn a_reused_key_and_an_overlong_encoded_url_are_named() {
    let kit = Kit::new().await;
    let alice = kit.user("alice@example.test").await;
    let a = kit.workspace(&alice).await;
    let path = format!("/v1/workspaces/{a}/sources");
    let (status, first) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("k1"),
            url("https://example.com/post"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, body) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("k1"),
            url("https://example.com/another"),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("idempotency_key_reused")
        )
    );
    // Within the cap as typed, over it once percent-encoded: refused by name, not a 500.
    let encoded = format!("https://example.com/{}", "é".repeat(800));
    let (status, body) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("encoded"),
            url(&encoded),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("url_too_long"))
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn submit_is_idempotent_and_refuses_unfetchable_urls_by_name() {
    let kit = Kit::new().await;
    let alice = kit.user("alice@example.test").await;
    let a = kit.workspace(&alice).await;
    let path = format!("/v1/workspaces/{a}/sources");
    let (status, first) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("k1"),
            url("https://example.com/post"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, again) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("k1"),
            url("https://example.com/post"),
        )
        .await;
    assert_eq!((status, &again["id"]), (StatusCode::OK, &first["id"]));
    for (raw, code) in [
        ("http://169.254.169.254/latest/meta-data/", "url_address"),
        ("http://10.0.0.1/", "url_address"),
        ("http://[::ffff:127.0.0.1]/", "url_address"),
        ("file:///etc/passwd", "url_scheme"),
        ("http://localhost/", "url_local_name"),
        ("https://user:pw@example.com/", "url_credentials"),
        ("https://example.com:6379/", "url_port"),
    ] {
        let (status, body) = kit
            .call(
                Method::POST,
                &path,
                Auth::Session(&alice),
                Some(raw),
                url(raw),
            )
            .await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some(code)),
            "{raw}"
        );
    }
    let (status, body) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            None,
            url("https://example.com/"),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("idempotency_key_required")
        )
    );
    let (status, body) = kit
        .call(
            Method::POST,
            &path,
            Auth::Session(&alice),
            Some("k2"),
            Some(json!({ "url": "https://example.com/", "x": 1 })),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("body_invalid"))
    );
    let (status, listed) = kit
        .call(Method::GET, &path, Auth::Session(&alice), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed["sources"].as_array().unwrap().len(),
        1,
        "refusals queue nothing"
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn another_workspaces_sources_are_404_and_tokens_only_read() {
    let kit = Kit::new().await;
    let alice = kit.user("alice@example.test").await;
    let bob = kit.user("bob@example.test").await;
    let a = kit.workspace(&alice).await;
    let b = kit.workspace(&bob).await;
    let (_, made) = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{b}/sources"),
            Auth::Session(&bob),
            Some("k"),
            url("https://example.com/b"),
        )
        .await;
    let b_source = made["id"].as_str().unwrap().to_owned();
    let a_token = kit.token(&alice, a).await;
    let b_routes = [
        (Method::GET, format!("/v1/workspaces/{b}/sources")),
        (Method::POST, format!("/v1/workspaces/{b}/sources")),
        (
            Method::GET,
            format!("/v1/workspaces/{b}/sources/{b_source}"),
        ),
    ];
    for (method, path) in &b_routes {
        for auth in [Auth::Session(&alice), Auth::Token(&a_token)] {
            let (status, _) = kit
                .call(
                    method.clone(),
                    path,
                    auth,
                    Some("x"),
                    url("https://example.com/x"),
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }
    // B's source id under A's path is not found either.
    let (status, _) = kit
        .call(
            Method::GET,
            &format!("/v1/workspaces/{a}/sources/{b_source}"),
            Auth::Session(&alice),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A token reads its own workspace but cannot submit.
    let (status, _) = kit
        .call(
            Method::GET,
            &format!("/v1/workspaces/{a}/sources"),
            Auth::Token(&a_token),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{a}/sources"),
            Auth::Token(&a_token),
            Some("t"),
            url("https://example.com/t"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = kit
        .call(
            Method::GET,
            &format!("/v1/workspaces/{a}/sources"),
            Auth::None,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // B is unchanged: still one source.
    let (_, listed) = kit
        .call(
            Method::GET,
            &format!("/v1/workspaces/{b}/sources"),
            Auth::Session(&bob),
            None,
            None,
        )
        .await;
    assert_eq!(listed["sources"].as_array().unwrap().len(), 1);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_spent_workspace_refuses_new_sources_as_budget_exhausted() {
    let kit = Kit::new().await;
    let alice = kit.user("alice@example.test").await;
    let a = kit.workspace(&alice).await;
    sqlx::query("INSERT INTO spend_days (workspace_id, day, spent_micros) VALUES ($1, (now() AT TIME ZONE 'utc')::date, $2)")
        .bind(a)
        .bind(1_000_000_i64)
        .execute(&kit.db.pool)
        .await
        .unwrap();
    let (status, body) = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{a}/sources"),
            Auth::Session(&alice),
            Some("k"),
            url("https://example.com/"),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("budget_exhausted"))
    );
    let other = kit.workspace(&alice).await;
    let (status, _) = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{other}/sources"),
            Auth::Session(&alice),
            Some("k"),
            url("https://example.com/"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "the cap is per workspace");
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_full_queue_refuses_new_sources() {
    let kit = Kit::new().await;
    let alice = kit.user("alice@example.test").await;
    let a = kit.workspace(&alice).await;
    sqlx::query(
        "INSERT INTO sources (id, workspace_id, idempotency_key, url) \
         SELECT gen_random_uuid(), $1, 'seed-' || n, 'https://example.com/' FROM generate_series(1, 100) n",
    )
    .bind(a)
    .execute(&kit.db.pool)
    .await
    .unwrap();
    let (status, body) = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{a}/sources"),
            Auth::Session(&alice),
            Some("k"),
            url("https://example.com/"),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("queue_full"))
    );
    kit.db.cleanup().await;
}
