//! The worker: claims sources, fetches, extracts and analyses them, stores the result.
//! Bounded concurrency; every job has a deadline shorter than its lease, so a job that
//! hangs is failed by its own worker before another may claim it.

use std::sync::Arc;
use std::time::Duration;

use repurpose_analysis::model::Model;
use repurpose_analysis::{AnalysisError, analyse, analyse::PROMPT_VERSION};
use repurpose_db::DbError;
use repurpose_ingest::fetch::Fetcher;
use repurpose_ingest::{IngestError, extract, ingest};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::spend::{Caps, PgSpend};
use crate::store::{self, Done, Job, MAX_ATTEMPTS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSettings {
    // LIMIT(worker-concurrency)
    pub concurrency: usize,
    pub lease: Duration,
    // LIMIT(job-deadline)
    pub job_deadline: Duration,
    pub idle_poll: Duration,
    /// When a transiently failed fetch is retried.
    pub retry_after: Duration,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            concurrency: 8,
            lease: Duration::from_mins(5),
            job_deadline: Duration::from_mins(4),
            idle_poll: Duration::from_millis(500),
            retry_after: Duration::from_secs(30),
        }
    }
}

pub struct Worker {
    pub pool: PgPool,
    pub fetcher: Fetcher,
    pub model: Arc<dyn Model>,
    pub analysis: repurpose_analysis::Settings,
    pub extract: extract::Limits,
    pub caps: Caps,
    pub settings: WorkerSettings,
}

/// What became of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    Failed(&'static str),
    /// Handed back for a later attempt.
    Retry(&'static str),
    /// The attempt was superseded; nothing was written.
    Stale,
}

enum Failure {
    Ingest(IngestError),
    Analysis(AnalysisError),
    Deadline,
}

impl Worker {
    /// Claims and runs jobs until `shutdown`; in-flight jobs finish (or hit their
    /// deadline) before it returns.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let permits = Arc::new(Semaphore::new(self.settings.concurrency.max(1)));
        loop {
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                permit = Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            if let Ok(Some(job)) = store::claim(&self.pool, self.settings.lease).await {
                let worker = Arc::clone(&self);
                tokio::spawn(async move {
                    let _permit = permit;
                    worker.finish(job).await;
                });
            } else {
                drop(permit);
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(self.settings.idle_poll) => {}
                }
            }
        }
        let all = u32::try_from(self.settings.concurrency.max(1)).unwrap_or(u32::MAX);
        let _ = tokio::time::timeout(self.settings.job_deadline, permits.acquire_many(all)).await;
    }

    /// Claims and runs one job inline: tests and drills.
    pub async fn run_once(&self) -> Result<Option<(Uuid, Outcome)>, DbError> {
        let Some(job) = store::claim(&self.pool, self.settings.lease).await? else {
            return Ok(None);
        };
        let id = job.id;
        Ok(Some((id, self.finish(job).await)))
    }

    async fn finish(&self, job: Job) -> Outcome {
        let outcome = match self.process(&job).await {
            Ok(done) => match store::complete(&self.pool, &job, &done).await {
                Ok(true) => Outcome::Ready,
                Ok(false) => Outcome::Stale,
                // The lease will lapse and another attempt will run.
                Err(_) => Outcome::Retry("store_unavailable"),
            },
            Err(failure) => self.record(&job, failure).await,
        };
        let code = match outcome {
            Outcome::Ready => "ready",
            Outcome::Failed(code) | Outcome::Retry(code) => code,
            Outcome::Stale => "stale",
        };
        // Ids and codes only: never the URL or the content.
        tracing::info!(source = %job.id, attempt = job.attempt, outcome = code, "source job finished");
        outcome
    }

    async fn record(&self, job: &Job, failure: Failure) -> Outcome {
        let (code, transient) = match failure {
            Failure::Ingest(e) => (e.code(), e.is_transient()),
            Failure::Analysis(e) => (e.code(), e == AnalysisError::LedgerUnavailable),
            Failure::Deadline => ("job_deadline", false),
        };
        let written = if transient && job.attempt < MAX_ATTEMPTS {
            store::release(&self.pool, job, self.settings.retry_after)
                .await
                .map(|ok| {
                    if ok {
                        Outcome::Retry(code)
                    } else {
                        Outcome::Stale
                    }
                })
        } else {
            store::fail(&self.pool, job, code).await.map(|ok| {
                if ok {
                    Outcome::Failed(code)
                } else {
                    Outcome::Stale
                }
            })
        };
        written.unwrap_or(Outcome::Retry("store_unavailable"))
    }

    async fn process(&self, job: &Job) -> Result<Done, Failure> {
        let work = async {
            let ingested = ingest(&self.fetcher, &job.url, self.extract)
                .await
                .map_err(Failure::Ingest)?;
            let spend = PgSpend {
                pool: self.pool.clone(),
                caps: self.caps,
                workspace: job.workspace,
                source: Some(job.id),
            };
            let analysis = analyse(&*self.model, &spend, &ingested.article.text, &self.analysis)
                .await
                .map_err(Failure::Analysis)?;
            Ok(Done {
                final_url: ingested.final_url.to_string(),
                title: ingested.article.title,
                body: ingested.article.text,
                truncated: ingested.article.truncated,
                ideas: analysis.ideas,
                refused: analysis.refused,
                prompt_version: PROMPT_VERSION,
            })
        };
        tokio::time::timeout(self.settings.job_deadline, work)
            .await
            .unwrap_or(Err(Failure::Deadline))
    }
}
