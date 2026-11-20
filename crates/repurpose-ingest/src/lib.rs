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

use thiserror::Error;

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
    let content_type = page.content_type.as_deref();
    let text = decode::decode_text(&page.body, content_type);
    if decode::is_plain_text(content_type) {
        extract::extract_plain(&text, limits)
    } else {
        extract::extract_html(&text, &page.url, limits)
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
    let article = tokio::task::spawn_blocking(move || extract_fetched(&page, &limits))
        .await
        .map_err(|_| IngestError::Worker)??;
    Ok(Ingested { final_url, article })
}
