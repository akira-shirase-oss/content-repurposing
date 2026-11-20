//! `END_GOAL` failure cases 1-3 against recorded provider cassettes: provider down,
//! spend cap, malformed output. Time is virtual (paused clock), so backoff and
//! timeouts do not depend on machine speed.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use std::time::Duration;

use repurpose_analysis::model::{Model, Recorded};
use repurpose_analysis::spans::check_span;
use repurpose_analysis::spend::MemorySpend;
use repurpose_analysis::{AnalysisError, Kind, Settings, analyse};

const SOURCE: &str = "Bounded queues keep a service honest under load.\n\nIn 2024 the survey found 37 percent fewer pages after adding admission control.\n\nUnbounded is a bug you have not met yet.";

fn cassette(name: &str) -> Recorded {
    let path = format!("{}/tests/{name}.json", env!("CARGO_MANIFEST_DIR"));
    Recorded::from_json(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A generous cap: $10.
fn spend() -> MemorySpend {
    MemorySpend::with_cap(10_000_000)
}

async fn run(
    name: &str,
) -> (
    Result<repurpose_analysis::Analysis, AnalysisError>,
    Recorded,
    MemorySpend,
) {
    let model = cassette(name);
    let ledger = spend();
    let out = analyse(&model, &ledger, SOURCE, &Settings::default()).await;
    (out, model, ledger)
}

#[tokio::test(start_paused = true)]
async fn provider_down_ends_provider_unavailable_after_bounded_retries() {
    for name in [
        "provider-down/overloaded-529",
        "provider-down/hang",
        "provider-down/retry-after-too-long",
    ] {
        let started = tokio::time::Instant::now();
        let (out, model, ledger) = run(name).await;
        assert_eq!(out, Err(AnalysisError::ProviderUnavailable), "{name}");
        let calls = model.calls().len();
        assert!(
            calls <= Settings::default().max_attempts as usize,
            "{name}: {calls} calls"
        );
        assert_eq!(
            ledger.events().len(),
            calls,
            "{name}: one usage event per call"
        );
        assert!(
            started.elapsed() < Duration::from_mins(4),
            "{name}: bounded in time"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn mid_stream_error_never_yields_a_ready_draft() {
    let (out, model, _) = run("provider-down/mid-stream-error").await;
    assert_eq!(out, Err(AnalysisError::ProviderUnavailable));
    assert_eq!(
        model.calls().len(),
        Settings::default().max_attempts as usize
    );
}

#[tokio::test(start_paused = true)]
async fn transient_failures_back_off_then_succeed() {
    let started = tokio::time::Instant::now();
    let (out, model, _) = run("provider-down/recovers").await;
    let analysis = out.unwrap();
    assert_eq!(analysis.calls, 3);
    assert_eq!(model.calls().len(), 3);
    // Backoff 1 s then 2 s.
    assert_eq!(started.elapsed().as_secs(), 3);
    let (out, _, _) = run("provider-down/rate-limited-retry-after").await;
    assert_eq!(out.unwrap().calls, 2);
}

#[tokio::test(start_paused = true)]
async fn spend_cap_is_terminal_not_retried() {
    let (out, model, ledger) = run("provider-down/spend-limit").await;
    assert_eq!(out, Err(AnalysisError::BudgetExhausted));
    assert_eq!(
        model.calls().len(),
        1,
        "enforced_spend_limit_reached is never retried"
    );
    assert_eq!(ledger.events().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_tenant_cap_refuses_before_any_call() {
    let model = Recorded::echo();
    let nearly_empty = MemorySpend::with_cap(10);
    let out = analyse(&model, &nearly_empty, SOURCE, &Settings::default()).await;
    assert_eq!(out, Err(AnalysisError::BudgetExhausted));
    assert!(
        model.calls().is_empty(),
        "no provider call once the cap would be crossed"
    );
    assert_eq!(nearly_empty.events().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn llm_output_repair_then_fail() {
    let cases = [
        ("malformed/schema-mismatch", AnalysisError::OutputInvalid),
        ("malformed/refusal", AnalysisError::Refused),
        ("malformed/max-tokens", AnalysisError::MaxTokens),
        ("malformed/unmatched-spans", AnalysisError::SpansUnmatched),
    ];
    for (name, want) in cases {
        let (out, model, _) = run(name).await;
        assert_eq!(out, Err(want), "{name}");
        let calls = model.calls();
        assert_eq!(calls.len(), 2, "{name}: exactly one repair attempt");
        assert!(calls[0].repair.is_none());
        assert!(
            calls[1]
                .repair
                .as_deref()
                .is_some_and(|r| r.contains(want.code())),
            "{name}"
        );
    }
    let (out, model, _) = run("malformed/not-json").await;
    assert_eq!(out.unwrap().ideas.len(), 1, "a repaired answer is accepted");
    assert_eq!(model.calls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn wrong_case_enums_are_accepted_and_spans_are_exact() {
    let (out, _, _) = run("malformed/wrong-case-enum").await;
    let analysis = out.unwrap();
    assert_eq!(analysis.ideas.len(), 2);
    assert_eq!(analysis.ideas[0].kind, Kind::Number);
    assert_eq!(analysis.ideas[1].kind, Kind::Idea);
    for idea in &analysis.ideas {
        for cited in &idea.spans {
            assert_eq!(&SOURCE[cited.span.start..cited.span.end], cited.text);
            assert!(check_span(SOURCE, cited.span, &cited.text).is_ok());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn ideas_with_unmatched_spans_are_refused_and_counted() {
    let (out, _, _) = run("malformed/partly-unmatched").await;
    let analysis = out.unwrap();
    assert_eq!(analysis.ideas.len(), 1);
    assert_eq!(analysis.refused, 1);
    assert!(analysis.ideas[0].text.contains("37 percent"));
}

#[tokio::test(start_paused = true)]
async fn the_source_is_sent_as_data_and_echo_is_faithful() {
    let model = Recorded::echo();
    let ledger = spend();
    let analysis = analyse(&model, &ledger, SOURCE, &Settings::default())
        .await
        .unwrap();
    assert_ne!(analysis.ideas.len(), 0);
    let sent = &model.calls()[0];
    assert_eq!(sent.source, SOURCE);
    assert!(
        !sent.system.contains("Bounded queues"),
        "source text never enters the instructions"
    );
    assert_eq!(model.id(), "recorded");
    assert_eq!(ledger.events().len(), 1);
    assert_eq!(ledger.spent(), analysis.cost);
}
