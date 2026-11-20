//! The model port and the recorded adapter.
//!
//! The source text travels as data inside the request, never spliced into the
//! instructions, and the model has no tools: its whole effect is the JSON it returns,
//! which the analyser validates.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// The fixed instructions (prompt version included).
    pub system: String,
    /// The extracted article, delivered as a document, never as instructions.
    pub source: String,
    /// Why the previous answer was rejected, on the one repair attempt.
    pub repair: Option<String>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    EndTurn,
    MaxTokens,
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReply {
    pub text: String,
    pub stop: Stop,
    pub usage: Usage,
}

/// A provider failure, classified. Never carries the request or response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderError {
    #[error("the provider is overloaded")]
    Overloaded,
    #[error("the provider rate-limited the call")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("the provider failed with status {0}")]
    Server(u16),
    #[error("the provider stream ended with an error event")]
    MidStream,
    #[error("the provider did not answer in time")]
    Timeout,
    #[error("the provider's spend limit is reached")]
    SpendLimit,
    #[error("the provider rejected the request")]
    BadRequest,
    #[error("the connection to the provider failed")]
    Transport,
}

impl ProviderError {
    /// Whether retrying later can help. `SpendLimit` and `BadRequest` are terminal.
    pub fn is_transient(self) -> bool {
        !matches!(self, Self::SpendLimit | Self::BadRequest)
    }
}

/// The port every model adapter implements.
pub trait Model: Send + Sync {
    /// The model identifier recorded with every usage event.
    fn id(&self) -> &str;
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<ModelReply, ProviderError>>;
}

/// One recorded provider step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Reply(ModelReply),
    Error(ProviderError),
    /// The provider accepts and never answers.
    Hang,
}

/// What a recorded adapter answers.
#[derive(Debug, Clone)]
enum Script {
    /// The steps in order, then the last one forever.
    Steps(Vec<Step>),
    /// A deterministic, faithful answer built from the source's own sentences: the
    /// development and load-test stand-in for a provider.
    Echo,
}

/// The recorded adapter: replays a cassette (`tests/provider-down/*.json` and friends)
/// or echoes the source. Records every request it is sent.
#[derive(Debug, Clone)]
pub struct Recorded {
    script: Script,
    calls: Arc<Mutex<Vec<ModelRequest>>>,
}

#[derive(Debug, Deserialize)]
struct Cassette {
    steps: Vec<Step>,
}

impl Recorded {
    pub fn steps(steps: Vec<Step>) -> Self {
        Self {
            script: Script::Steps(steps),
            calls: Arc::default(),
        }
    }

    /// A cassette file's JSON: `{"steps": [...]}`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let cassette: Cassette = serde_json::from_str(json)?;
        Ok(Self::steps(cassette.steps))
    }

    pub fn echo() -> Self {
        Self {
            script: Script::Echo,
            calls: Arc::default(),
        }
    }

    /// Every request received so far.
    pub fn calls(&self) -> Vec<ModelRequest> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn step(&self, request: &ModelRequest) -> Step {
        let n = match self.calls.lock() {
            Ok(mut calls) => {
                calls.push(request.clone());
                calls.len() - 1
            }
            Err(_) => 0,
        };
        match &self.script {
            Script::Steps(steps) => steps
                .get(n)
                .or_else(|| steps.last())
                .cloned()
                .unwrap_or(Step::Error(ProviderError::Transport)),
            Script::Echo => Step::Reply(echo(&request.source)),
        }
    }
}

impl Model for Recorded {
    fn id(&self) -> &'static str {
        "recorded"
    }

    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<ModelReply, ProviderError>> {
        let step = self.step(request);
        Box::pin(async move {
            match step {
                Step::Reply(reply) => Ok(reply),
                Step::Error(error) => Err(error),
                Step::Hang => {
                    tokio::time::sleep(Duration::from_hours(24)).await;
                    Err(ProviderError::Timeout)
                }
            }
        })
    }
}

/// Up to five ideas, each quoting one sentence of the source verbatim.
fn echo(source: &str) -> ModelReply {
    let ideas: Vec<serde_json::Value> = source
        .split_inclusive(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| s.len() >= 40 && s.len() <= 400)
        .take(5)
        .map(|sentence| {
            let kind = if sentence.chars().any(|c| c.is_ascii_digit()) {
                "number"
            } else {
                "idea"
            };
            serde_json::json!({ "kind": kind, "text": sentence, "quotes": [sentence] })
        })
        .collect();
    let text = serde_json::json!({ "ideas": ideas }).to_string();
    let output_tokens = (text.len() / 4) as u64;
    ModelReply {
        text,
        stop: Stop::EndTurn,
        usage: Usage {
            input_tokens: (source.len() / 4) as u64,
            output_tokens,
        },
    }
}
