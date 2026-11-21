//! The bounded extraction pool: a caller dropped at its deadline keeps its slot until
//! the blocking work ends, and the work sees the cancel flag.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use repurpose_ingest::run_bounded;
use tokio::sync::Semaphore;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_caller_holds_its_slot_until_the_work_ends() {
    let pool = Arc::new(Semaphore::new(1));
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let (saw_tx, saw) = mpsc::channel();
    let caller = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move {
            run_bounded(&pool, move |cancel| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                saw_tx.send(cancel.load(Ordering::Relaxed)).unwrap();
            })
            .await
        }
    });
    tokio::task::spawn_blocking(move || started.recv().unwrap())
        .await
        .unwrap();
    // The caller gives up: the work is still running, so its slot is still taken.
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(pool.available_permits(), 0, "the slot outlives the caller");
    let second = run_bounded(&pool, |_| 2);
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut second)
            .await
            .is_err(),
        "nothing else starts while the abandoned work runs"
    );
    release.send(()).unwrap();
    let cancelled = tokio::task::spawn_blocking(move || saw.recv().unwrap())
        .await
        .unwrap();
    assert!(cancelled, "the abandoned work sees the cancel flag");
    assert_eq!(second.await, Ok(2));
    assert_eq!(pool.available_permits(), 1);
}

#[tokio::test]
async fn a_caller_that_waits_gets_its_result_and_frees_the_slot() {
    let pool = Arc::new(Semaphore::new(1));
    let out = run_bounded(&pool, |cancel| cancel.load(Ordering::Relaxed)).await;
    assert_eq!(out, Ok(false));
    assert_eq!(pool.available_permits(), 1);
}

#[test]
fn a_cancelled_parse_stops_between_chunks() {
    use repurpose_ingest::{ExtractError, extract};
    use std::sync::atomic::AtomicBool;
    let html = format!("<html><body>{}</body></html>", "<p>x</p>".repeat(10_000));
    let url = url::Url::parse("https://example.com/").unwrap();
    let out = extract::extract_html_until(
        &html,
        &url,
        &extract::Limits::default(),
        &AtomicBool::new(true),
    );
    assert_eq!(out.unwrap_err(), ExtractError::Cancelled);
}

/// The process-wide pool bounds extraction CPU below the worker's job concurrency (8),
/// so jobs past their deadline cannot multiply the blocking threads.
#[test]
fn the_process_pool_is_smaller_than_the_job_concurrency() {
    const JOB_CONCURRENCY: usize = 8;
    assert!((1..JOB_CONCURRENCY).contains(&repurpose_ingest::EXTRACT_THREADS));
}
