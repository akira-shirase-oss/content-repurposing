//! The sources table as a crash-safe queue. A worker claims a row with a lease; the row's
//! `attempts` is the fencing token, so a worker whose lease ran out and was re-claimed
//! cannot write its late result over the new attempt's.

use repurpose_analysis::spans::Span;
use repurpose_analysis::{Idea, Kind};
use repurpose_db::DbError;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row as _};
use std::time::Duration;
use uuid::Uuid;

/// Attempts before a source fails as `attempts_exhausted`.
// LIMIT(source-attempts)
pub const MAX_ATTEMPTS: i32 = 5;

/// The per-workspace lock a submission holds.
pub const SUBMIT_LOCK: &str = "SELECT 1 FROM workspaces WHERE id = $1 FOR NO KEY UPDATE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// A new source, queued.
    Created(Uuid),
    /// The same idempotency key was used before, for the same URL: that source.
    Existing(Uuid),
    /// The same idempotency key was used before for another URL.
    KeyReused,
    /// The workspace already has this many unfinished sources.
    QueueFull,
}

/// Queues `url` for `workspace`, once per `key`; refused when `max_pending` unfinished
/// sources already wait. The count and the insert are serialised per workspace.
pub async fn submit(
    pool: &PgPool,
    workspace: Uuid,
    key: &str,
    url: &str,
    max_pending: i64,
) -> Result<Submitted, DbError> {
    let mut tx = pool.begin().await?;
    // Serialises submissions per workspace so the queue bound cannot be raced; NO KEY
    // leaves the key-share lock a foreign-key insert takes free.
    sqlx::query(SUBMIT_LOCK)
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    if let Some(row) =
        sqlx::query("SELECT id, url FROM sources WHERE workspace_id = $1 AND idempotency_key = $2")
            .bind(workspace)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?
    {
        let stored: String = row.try_get("url")?;
        if stored != url {
            return Ok(Submitted::KeyReused);
        }
        return Ok(Submitted::Existing(row.try_get("id")?));
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sources WHERE workspace_id = $1 AND status IN ('pending', 'running')",
    )
    .bind(workspace)
    .fetch_one(&mut *tx)
    .await?;
    if pending >= max_pending {
        return Ok(Submitted::QueueFull);
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sources (id, workspace_id, idempotency_key, url) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(workspace)
    .bind(key)
    .bind(url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Submitted::Created(id))
}

/// A claimed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: Uuid,
    pub workspace: Uuid,
    pub url: String,
    /// The fencing token.
    pub attempt: i32,
}

/// Claims the oldest claimable source: pending, or running with a lapsed lease (its
/// worker died). Sources out of attempts are failed on the way.
pub async fn claim(pool: &PgPool, lease: Duration) -> Result<Option<Job>, DbError> {
    sqlx::query(
        "UPDATE sources SET status = 'failed', failure = 'attempts_exhausted', lease_until = NULL, \
         updated_at = now() \
         WHERE status = 'running' AND lease_until < now() AND attempts >= $1",
    )
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await?;
    let row = sqlx::query(
        "UPDATE sources SET status = 'running', attempts = attempts + 1, \
             lease_until = now() + make_interval(secs => $1), updated_at = now() \
         WHERE id = (SELECT id FROM sources \
                      WHERE (status = 'pending' OR (status = 'running' AND lease_until < now())) \
                        AND attempts < $2 \
                      ORDER BY created_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
         RETURNING id, workspace_id, url, attempts",
    )
    .bind(lease.as_secs_f64())
    .bind(MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(Job {
            id: row.try_get("id")?,
            workspace: row.try_get("workspace_id")?,
            url: row.try_get("url")?,
            attempt: row.try_get("attempts")?,
        })
    })
    .transpose()
}

/// What a finished job stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub final_url: String,
    pub title: String,
    pub body: String,
    pub truncated: bool,
    pub ideas: Vec<Idea>,
    pub refused: usize,
    pub prompt_version: &'static str,
}

/// Stores a finished job's article and ideas in one transaction. `false` when the
/// attempt is no longer current (the lease lapsed and another worker took it).
pub async fn complete(pool: &PgPool, job: &Job, done: &Done) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE sources SET status = 'ready', final_url = $3, title = $4, body = $5, truncated = $6, \
             refused_ideas = $7, prompt_version = $8, lease_until = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(&done.final_url)
    .bind(&done.title)
    .bind(&done.body)
    .bind(done.truncated)
    .bind(i32::try_from(done.refused).unwrap_or(i32::MAX))
    .bind(done.prompt_version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM ideas WHERE source_id = $1")
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
    for (ordinal, idea) in done.ideas.iter().enumerate() {
        let spans =
            serde_json::to_value(&idea.spans).map_err(|e| DbError::Invalid(e.to_string()))?;
        sqlx::query(
            "INSERT INTO ideas (source_id, ordinal, kind, text, spans) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(job.id)
        .bind(i32::try_from(ordinal).unwrap_or(i32::MAX))
        .bind(kind_name(idea.kind))
        .bind(&idea.text)
        .bind(spans)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Fails the current attempt with a named code. `false` when the attempt is stale.
pub async fn fail(pool: &PgPool, job: &Job, code: &str) -> Result<bool, DbError> {
    let updated = sqlx::query(
        "UPDATE sources SET status = 'failed', failure = $3, lease_until = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(code)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Hands a transiently failed attempt back to the queue, claimable after `after`.
pub async fn release(pool: &PgPool, job: &Job, after: Duration) -> Result<bool, DbError> {
    let updated = sqlx::query(
        "UPDATE sources SET lease_until = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(after.as_secs_f64())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Idea => "idea",
        Kind::Quote => "quote",
        Kind::Number => "number",
    }
}

/// One stored span, re-checked against the body when read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSpan {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpanView {
    pub start: usize,
    pub end: usize,
    pub text: String,
    /// The span's text is exactly the body's bytes at `[start, end)`.
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdeaView {
    pub kind: String,
    pub text: String,
    pub spans: Vec<SpanView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceView {
    pub id: Uuid,
    pub status: String,
    pub failure: Option<String>,
    pub url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub truncated: bool,
    pub refused_ideas: i32,
    pub ideas: Vec<IdeaView>,
}

/// One source of `workspace`, with its ideas; `None` when it is not this workspace's.
pub async fn get(pool: &PgPool, workspace: Uuid, id: Uuid) -> Result<Option<SourceView>, DbError> {
    let Some(row) = sqlx::query(
        "SELECT id, status, failure, url, final_url, title, body, truncated, refused_ideas \
         FROM sources WHERE id = $1 AND workspace_id = $2",
    )
    .bind(id)
    .bind(workspace)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let body: Option<String> = row.try_get("body")?;
    let rows =
        sqlx::query("SELECT kind, text, spans FROM ideas WHERE source_id = $1 ORDER BY ordinal")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let mut ideas = Vec::with_capacity(rows.len());
    for idea in rows {
        let stored: Vec<StoredSpan> = serde_json::from_value(idea.try_get("spans")?)
            .map_err(|e| DbError::Invalid(e.to_string()))?;
        let spans = stored
            .into_iter()
            .map(|s| {
                let verified = body.as_deref().is_some_and(|b| {
                    repurpose_analysis::spans::check_span(
                        b,
                        Span {
                            start: s.start,
                            end: s.end,
                        },
                        &s.text,
                    )
                    .is_ok()
                });
                SpanView {
                    start: s.start,
                    end: s.end,
                    text: s.text,
                    verified,
                }
            })
            .collect();
        ideas.push(IdeaView {
            kind: idea.try_get("kind")?,
            text: idea.try_get("text")?,
            spans,
        });
    }
    Ok(Some(SourceView {
        id: row.try_get("id")?,
        status: row.try_get("status")?,
        failure: row.try_get("failure")?,
        url: row.try_get("url")?,
        final_url: row.try_get("final_url")?,
        title: row.try_get("title")?,
        truncated: row.try_get("truncated")?,
        refused_ideas: row.try_get("refused_ideas")?,
        ideas,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    pub id: Uuid,
    pub status: String,
    pub failure: Option<String>,
    pub url: String,
    pub title: Option<String>,
}

/// The newest `limit` sources of `workspace`.
pub async fn list(pool: &PgPool, workspace: Uuid, limit: i64) -> Result<Vec<Listed>, DbError> {
    let rows = sqlx::query(
        "SELECT id, status, failure, url, title FROM sources WHERE workspace_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT $2",
    )
    .bind(workspace)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Listed {
                id: row.try_get("id")?,
                status: row.try_get("status")?,
                failure: row.try_get("failure")?,
                url: row.try_get("url")?,
                title: row.try_get("title")?,
            })
        })
        .collect()
}
