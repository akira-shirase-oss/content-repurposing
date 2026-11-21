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
    pub concurrency: usize,
    pub lease: Duration,
    pub job_deadline: Duration,
    pub idle_poll: Duration,
    /// When a transiently failed fetch is retried.
    pub retry_after: Duration,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            concurrency: 8, // LIMIT(worker-concurrency)
            lease: Duration::from_mins(5),
            job_deadline: Duration::from_mins(4), // LIMIT(job-deadline)
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

/// How the worker loop ended: every job task joined, none lost silently.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    /// Job tasks that panicked; their leases lapse and the job runs again.
    pub panicked: usize,
    /// Job tasks still running at the drain deadline, aborted.
    pub abandoned: usize,
}

impl Drained {
    fn reap(&mut self, done: Result<(), tokio::task::JoinError>) {
        if let Err(error) = done
            && error.is_panic()
        {
            self.panicked += 1;
            tracing::error!(code = "job_panicked", "a job task panicked");
        }
    }

    /// Whether every job task ended on its own.
    pub fn clean(&self) -> bool {
        self.panicked == 0 && self.abandoned == 0
    }
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
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Drained {
        let permits = Arc::new(Semaphore::new(self.settings.concurrency.max(1)));
        let mut tasks = tokio::task::JoinSet::new();
        let mut drained = Drained::default();
        loop {
            while let Some(done) = tasks.try_join_next() {
                drained.reap(done);
            }
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                permit = Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let claimed = store::claim(&self.pool, self.settings.lease).await;
            if let Ok(Some(job)) = claimed {
                let worker = Arc::clone(&self);
                tasks.spawn(async move {
                    let _permit = permit;
                    worker.finish(job).await;
                });
                continue;
            }
            if claimed.is_err() {
                tracing::warn!(code = "store_unavailable", "claim failed");
            }
            drop(permit);
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.settings.idle_poll) => {}
            }
        }
        let deadline = tokio::time::Instant::now() + self.settings.job_deadline;
        while !tasks.is_empty() {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(done)) => drained.reap(done),
                Ok(None) => break,
                Err(_) => {
                    drained.abandoned = tasks.len();
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
        drained
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
                model: self.model.id().to_owned(),
            })
        };
        tokio::time::timeout(self.settings.job_deadline, work)
            .await
            .unwrap_or(Err(Failure::Deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_job_deadline_ends_inside_the_lease() {
        let s = WorkerSettings::default();
        assert!(
            s.job_deadline < s.lease,
            "a hung job is failed before another worker may claim it"
        );
        assert!(s.job_deadline <= Duration::from_mins(4));
        let fetch = repurpose_ingest::fetch::Limits::default().deadline;
        let analysis = repurpose_analysis::Settings::default().deadline;
        assert!(
            fetch + analysis < s.job_deadline,
            "the analysis gives up before the job deadline cuts it off"
        );
    }
}
