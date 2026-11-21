//! `/v1/workspaces/{workspace}/sources`: paste a URL, read the result. Every route
//! authorizes through the accounts kit; another workspace's source is 404.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use repurpose_accounts::Accounts;
use repurpose_accounts::Via;
use repurpose_accounts::domain::Action;
use repurpose_accounts::error::AccountsError;
use repurpose_db::DbError;
use repurpose_http::stack::error;
use repurpose_ingest::guard::{Policy, check_url};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::spend::{self, Caps};
use crate::store::{self, Submitted};

/// What the routes need.
#[derive(Debug, Clone)]
pub struct Sources {
    pub accounts: Accounts,
    pub pool: PgPool,
    pub policy: Policy,
    pub caps: Caps,
    pub limits: store::Limits,
}

impl Sources {
    pub fn new(accounts: Accounts, pool: PgPool, caps: Caps) -> Self {
        Self {
            accounts,
            pool,
            policy: Policy::default(),
            caps,
            limits: store::Limits::default(),
        }
    }
}

#[derive(Debug)]
pub enum SourcesError {
    Accounts(AccountsError),
    Invalid(&'static str, &'static str),
    QueueFull,
    BudgetExhausted,
    RateLimited,
    NotFound,
    Unavailable,
    Internal,
}

impl From<AccountsError> for SourcesError {
    fn from(e: AccountsError) -> Self {
        Self::Accounts(e)
    }
}

impl From<DbError> for SourcesError {
    fn from(e: DbError) -> Self {
        if e.is_unavailable() {
            Self::Unavailable
        } else {
            tracing::error!(error = %repurpose_db::chain(&e), "sources store failed");
            Self::Internal
        }
    }
}

impl IntoResponse for SourcesError {
    fn into_response(self) -> Response {
        use StatusCode as S;
        match self {
            Self::Accounts(e) => e.into_response(),
            Self::Invalid(code, message) => error(S::UNPROCESSABLE_ENTITY, code, message),
            Self::QueueFull => error(
                S::TOO_MANY_REQUESTS,
                "queue_full",
                "too many sources are waiting; retry when some finish",
            ),
            Self::RateLimited => error(
                S::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many sources submitted this hour; retry later",
            ),
            Self::BudgetExhausted => error(
                S::TOO_MANY_REQUESTS,
                "budget_exhausted",
                "today's analysis budget is spent",
            ),
            Self::NotFound => error(S::NOT_FOUND, "not_found", "not found"),
            Self::Unavailable => error(S::SERVICE_UNAVAILABLE, "unavailable", "retry later"),
            Self::Internal => error(S::INTERNAL_SERVER_ERROR, "internal", "internal error"),
        }
    }
}

pub fn router(sources: Sources) -> Router {
    Router::new()
        .route("/workspaces/{workspace}/sources", get(list).post(submit))
        .route("/workspaces/{workspace}/sources/{source}", get(read))
        .with_state(sources)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    url: String,
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, SourcesError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(SourcesError::Invalid(
            "idempotency_key_required",
            "send an Idempotency-Key header",
        ))?;
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(SourcesError::Invalid(
            "idempotency_key_invalid",
            "the Idempotency-Key is 1-128 visible ASCII characters",
        ));
    }
    Ok(key.to_owned())
}

async fn submit(
    State(sources): State<Sources>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
    body: Bytes,
) -> Result<Response, SourcesError> {
    let access = sources
        .accounts
        .access(&headers, workspace, Action::Edit)
        .await?;
    let key = idempotency_key(&headers)?;
    let body: Submit = serde_json::from_slice(&body)
        .map_err(|_| SourcesError::Invalid("body_invalid", "send {\"url\": \"https://...\"}"))?;
    let url = check_url(body.url.trim(), sources.policy)
        .map_err(|refusal| SourcesError::Invalid(refusal.code(), "this URL cannot be fetched"))?;
    if spend::exhausted(&sources.pool, sources.caps, access.workspace).await? {
        return Err(SourcesError::BudgetExhausted);
    }
    let user = match access.via {
        Via::Member { user, .. } => Some(user),
        Via::Token { .. } => None,
    };
    match store::submit_as(
        &sources.pool,
        access.workspace,
        user,
        &key,
        url.as_str(),
        sources.limits,
    )
    .await?
    {
        Submitted::Created(id) => Ok((
            StatusCode::ACCEPTED,
            axum::Json(json!({ "id": id, "status": "pending" })),
        )
            .into_response()),
        Submitted::Existing(id) => {
            Ok((StatusCode::OK, axum::Json(json!({ "id": id }))).into_response())
        }
        Submitted::KeyReused => Err(SourcesError::Invalid(
            "idempotency_key_reused",
            "this Idempotency-Key was used for another URL",
        )),
        Submitted::QueueFull => Err(SourcesError::QueueFull),
        Submitted::RateLimited => Err(SourcesError::RateLimited),
    }
}

async fn read(
    State(sources): State<Sources>,
    headers: HeaderMap,
    Path((workspace, source)): Path<(Uuid, Uuid)>,
) -> Result<Response, SourcesError> {
    let access = sources
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let view = store::get(&sources.pool, access.workspace, source)
        .await?
        .ok_or(SourcesError::NotFound)?;
    Ok(axum::Json(view).into_response())
}

async fn list(
    State(sources): State<Sources>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<Response, SourcesError> {
    let access = sources
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let listed = store::list(&sources.pool, access.workspace, 50).await?;
    Ok(axum::Json(json!({ "sources": listed })).into_response())
}
