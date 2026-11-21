//! Ingest: an untrusted URL in, an extracted article out.
//!
//! [`guard`] decides what may be fetched, [`fetch`] fetches it inside size and time
//! caps, [`decode`] turns bytes into text and [`extract`] runs Readability. Nothing here
//! logs a URL, a header or page content.

pub mod decode;
mod error;
pub mod extract;
pub mod fetch;
#[cfg(feature = "test-support")]
pub mod fixture;
pub mod guard;
pub mod resolve;

pub use error::{ExtractError, FetchError, Refusal};
pub use extract::Article;
pub use fetch::{Fetched, Fetcher};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use thiserror::Error;
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IngestError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Extract(#[from] ExtractError),
    #[error("the extraction worker failed")]
    Worker,
}

impl IngestError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Fetch(e) => e.code(),
            Self::Extract(e) => e.code(),
            Self::Worker => "extract_worker",
        }
    }

    pub fn is_transient(self) -> bool {
        matches!(self, Self::Fetch(e) if e.is_transient())
    }
}

/// A fetched and extracted article, with the URL it finally came from.
#[derive(Debug, Clone)]
pub struct Ingested {
    pub final_url: url::Url,
    pub article: Article,
}

/// Decodes and extracts a fetched page. CPU-bound: run it on a blocking thread.
pub fn extract_fetched(page: &Fetched, limits: &extract::Limits) -> Result<Article, ExtractError> {
    extract_fetched_until(page, limits, &AtomicBool::new(false))
}

/// [`extract_fetched`], abandoned between parser input chunks once `cancel` is set.
pub fn extract_fetched_until(
    page: &Fetched,
    limits: &extract::Limits,
    cancel: &AtomicBool,
) -> Result<Article, ExtractError> {
    let content_type = page.content_type.as_deref();
    let text = decode::decode_text(&page.body, content_type);
    if decode::is_plain_text(content_type) {
        extract::extract_plain(&text, limits)
    } else {
        extract::extract_html_until(&text, &page.url, limits, cancel)
    }
}

/// Fetches `raw` and extracts its article; the CPU work runs on the blocking pool.
pub async fn ingest(
    fetcher: &Fetcher,
    raw: &str,
    limits: extract::Limits,
) -> Result<Ingested, IngestError> {
    let page = fetcher.fetch(raw).await?;
    let final_url = page.url.clone();
    let article = run_bounded(extract_pool(), move |cancel| {
        extract_fetched_until(&page, &limits, cancel)
    })
    .await??;
    Ok(Ingested { final_url, article })
}

/// Blocking extractions running at once, process-wide, whatever the number of workers.
// LIMIT(extract-threads)
pub const EXTRACT_THREADS: usize = 4;

fn extract_pool() -> &'static Arc<Semaphore> {
    static POOL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    POOL.get_or_init(|| Arc::new(Semaphore::new(EXTRACT_THREADS)))
}

/// Runs `work` on a blocking thread once `pool` admits it. The permit moves into the
/// blocking closure, so it is released when the work ends, not when the caller stops
/// waiting: a caller dropped at its deadline cannot free a slot while the CPU work goes
/// on. Dropping the caller also sets the cancel flag the work polls.
pub async fn run_bounded<T, F>(pool: &Arc<Semaphore>, work: F) -> Result<T, IngestError>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> T + Send + 'static,
{
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let permit = Arc::clone(pool)
        .acquire_owned()
        .await
        .map_err(|_| IngestError::Worker)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let guard = CancelOnDrop(Arc::clone(&cancel));
    let out = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(&cancel)
    })
    .await
    .map_err(|_| IngestError::Worker);
    drop(guard);
    out
}
