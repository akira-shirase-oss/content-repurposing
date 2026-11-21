//! The M1 pipeline against a real database, fixture pages and the recorded model: a
//! pasted URL becomes ideas with exact spans; the queue is idempotent, bounded and
//! crash-safe; the spend cap refuses before the model is called.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use std::sync::Arc;
use std::time::Duration;

use repurpose_analysis::model::{Model, ModelReply, Recorded, Step, Stop, Usage};
use repurpose_analysis::spend::{Estimate, Spend, SpendError};
use repurpose_db::testing::TestDb;
use repurpose_ingest::extract;
use repurpose_ingest::fetch::{Fetcher, Limits};
use repurpose_ingest::fixture::{PageServer, Reply};
use repurpose_ingest::guard::Policy;
use repurpose_ingest::resolve::Resolver;
use repurpose_sources::spend::{Caps, PgSpend, exhausted};
use repurpose_sources::store::{self, Submitted};
use repurpose_sources::{Outcome, Worker, WorkerSettings};
use sqlx::PgPool;
use uuid::Uuid;

const ARTICLE: &str = include_str!("../../repurpose-ingest/tests/hostile-pages/article.html");

fn est(cost: u64) -> Estimate {
    Estimate {
        usage: Usage {
            input_tokens: 100,
            output_tokens: 100,
        },
        cost,
    }
}

async fn workspace(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

fn worker(pool: &PgPool, model: Arc<dyn Model>, caps: Caps) -> Worker {
    let fetcher = Fetcher::new(
        Policy::loopback_fixture(),
        Limits {
            deadline: Duration::from_secs(10),
            ..Limits::default()
        },
        Resolver::default(),
    )
    .unwrap();
    Worker {
        pool: pool.clone(),
        fetcher,
        model,
        analysis: repurpose_analysis::Settings::default(),
        extract: extract::Limits::default(),
        caps,
        settings: WorkerSettings::default(),
    }
}

async fn pages() -> PageServer {
    PageServer::start(|path| match path {
        "/article" => Reply::ok("text/html; charset=utf-8", ARTICLE),
        "/to-metadata" => Reply::redirect("http://169.254.169.254/latest/meta-data/"),
        "/busy" => Reply::with_headers(503, &[], "busy"),
        _ => Reply::with_headers(404, &[], "no"),
    })
    .await
    .unwrap()
}

async fn submit(pool: &PgPool, ws: Uuid, key: &str, url: &str) -> Uuid {
    match store::submit(pool, ws, key, url, 100).await.unwrap() {
        Submitted::Created(id) => id,
        other => panic!("{other:?}"),
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn a_pasted_url_becomes_ideas_with_exact_spans() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    let id = submit(&db.pool, ws, "k1", &site.url("/article")).await;
    let model = Recorded::echo();
    let w = worker(&db.pool, Arc::new(model.clone()), Caps::default());
    assert_eq!(w.run_once().await.unwrap(), Some((id, Outcome::Ready)));
    let view = store::get(&db.pool, ws, id).await.unwrap().unwrap();
    assert_eq!(view.status, "ready");
    assert!(view.title.is_some());
    assert_ne!(view.ideas.len(), 0);
    // The view carries the text its spans index, and says which model answered.
    let body = view.body.clone().unwrap();
    assert_eq!(view.model.as_deref(), Some("recorded"));
    assert_eq!(view.ideas[0].kind, "summary");
    for idea in &view.ideas {
        assert_ne!(idea.spans.len(), 0);
        for span in &idea.spans {
            assert!(span.verified);
            assert_eq!(&body[span.start..span.end], span.text);
        }
    }
    assert_eq!(model.call_count(), 1);
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM usage_events").await,
        1
    );
    assert!(
        store::get(&db.pool, workspace(&db.pool).await, id)
            .await
            .unwrap()
            .is_none(),
        "other workspace: none"
    );
    assert_eq!(w.run_once().await.unwrap(), None, "nothing left to claim");
    db.cleanup().await;
}

#[tokio::test]
async fn a_tampered_span_reads_back_unverified() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    let id = submit(&db.pool, ws, "k1", &site.url("/article")).await;
    let w = worker(&db.pool, Arc::new(Recorded::echo()), Caps::default());
    w.run_once().await.unwrap();
    sqlx::query("UPDATE ideas SET spans = jsonb_set(spans, '{0,start}', to_jsonb((spans->0->>'start')::int + 1)) WHERE source_id = $1")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    let view = store::get(&db.pool, ws, id).await.unwrap().unwrap();
    assert!(view.ideas.iter().all(|i| !i.spans[0].verified));
    db.cleanup().await;
}

#[tokio::test]
async fn resubmitting_with_the_same_key_is_a_no_op_and_the_queue_is_bounded() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let first = submit(&db.pool, ws, "same", "https://example.com/a").await;
    assert_eq!(
        store::submit(&db.pool, ws, "same", "https://example.com/a", 100)
            .await
            .unwrap(),
        Submitted::Existing(first)
    );
    for n in 0..2 {
        submit(&db.pool, ws, &format!("k{n}"), "https://example.com/a").await;
    }
    assert_eq!(
        store::submit(&db.pool, ws, "k-over", "https://example.com/a", 3)
            .await
            .unwrap(),
        Submitted::QueueFull
    );
    assert_eq!(count(&db.pool, "SELECT count(*) FROM sources").await, 3);
    // Concurrent submissions with one key create one row.
    let other = workspace(&db.pool).await;
    let racing: Vec<_> = (0..16)
        .map(|_| {
            let pool = db.pool.clone();
            tokio::spawn(async move {
                store::submit(&pool, other, "race", "https://example.com/r", 100)
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut created = 0;
    for r in racing {
        if matches!(r.await.unwrap(), Submitted::Created(_)) {
            created += 1;
        }
    }
    assert_eq!(created, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn a_lapsed_lease_is_reclaimed_and_the_stale_worker_cannot_write() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let id = submit(&db.pool, ws, "k", "https://example.com/a").await;
    let first = store::claim(&db.pool, Duration::from_mins(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((first.id, first.attempt), (id, 1));
    assert_eq!(
        store::claim(&db.pool, Duration::from_mins(5))
            .await
            .unwrap(),
        None,
        "leased: not claimable"
    );
    // The first worker dies: its lease lapses.
    sqlx::query("UPDATE sources SET lease_until = now() - interval '1 second'")
        .execute(&db.pool)
        .await
        .unwrap();
    let second = store::claim(&db.pool, Duration::from_mins(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert!(
        !store::fail(&db.pool, &first, "late").await.unwrap(),
        "a stale attempt writes nothing"
    );
    assert!(
        store::fail(&db.pool, &second, "fetch_timeout")
            .await
            .unwrap()
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_source_out_of_attempts_fails_named() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let id = submit(&db.pool, ws, "k", "https://example.com/a").await;
    sqlx::query("UPDATE sources SET status = 'running', attempts = 5, lease_until = now() - interval '1 second'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        store::claim(&db.pool, Duration::from_mins(5))
            .await
            .unwrap(),
        None
    );
    let view = store::get(&db.pool, ws, id).await.unwrap().unwrap();
    assert_eq!(
        (view.status.as_str(), view.failure.as_deref()),
        ("failed", Some("attempts_exhausted"))
    );
    db.cleanup().await;
}

#[tokio::test]
async fn the_workspace_spend_cap_refuses_before_the_model_is_called() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    submit(&db.pool, ws, "k", &site.url("/article")).await;
    let model = Recorded::echo();
    let caps = Caps {
        workspace_daily: 10,
        ..Caps::default()
    };
    let w = worker(&db.pool, Arc::new(model.clone()), caps);
    let (_, outcome) = w.run_once().await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Failed("budget_exhausted"));
    assert_eq!(model.call_count(), 0, "no provider call");
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM usage_events").await,
        0
    );
    assert_eq!(count(&db.pool, "SELECT count(*) FROM ideas").await, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn admissions_never_exceed_the_cap_under_concurrency() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let caps = Caps {
        workspace_daily: 1_000_000,
        global_daily: 50_000_000,
    };
    let tasks: Vec<_> = (0..40)
        .map(|_| {
            let spend = PgSpend {
                pool: db.pool.clone(),
                caps,
                workspace: ws,
                source: None,
            };
            tokio::spawn(async move { spend.admit("recorded", est(100_000)).await })
        })
        .collect();
    let mut admitted = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            Ok(a) => admitted.push(a),
            Err(e) => assert_eq!(e, SpendError::Exhausted),
        }
    }
    assert_eq!(admitted.len(), 10, "exactly the cap's worth is admitted");
    assert!(
        exhausted(&db.pool, caps, ws).await.unwrap(),
        "a full workspace refuses new sources"
    );
    // Settling releases the reservation down to the real cost and records the call.
    let spend = PgSpend {
        pool: db.pool.clone(),
        caps,
        workspace: ws,
        source: None,
    };
    for a in admitted {
        spend
            .settle(
                a,
                Usage {
                    input_tokens: 10,
                    output_tokens: 10,
                },
                7,
            )
            .await
            .unwrap();
    }
    assert!(!exhausted(&db.pool, caps, ws).await.unwrap());
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM usage_events").await,
        10
    );
    let spent_micros: i64 =
        sqlx::query_scalar("SELECT spent_micros FROM spend_days WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(spent_micros, 70);
    // Another workspace is unaffected by this one's cap.
    assert!(
        !exhausted(&db.pool, caps, workspace(&db.pool).await)
            .await
            .unwrap()
    );
    db.cleanup().await;
}

#[tokio::test]
async fn usage_events_are_append_only() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let spend = PgSpend {
        pool: db.pool.clone(),
        caps: Caps::default(),
        workspace: ws,
        source: None,
    };
    let a = spend.admit("recorded", est(10)).await.unwrap();
    spend.settle(a, Usage::default(), 0).await.unwrap();
    assert!(
        sqlx::query("UPDATE usage_events SET cost_micros = 0")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM usage_events")
            .execute(&db.pool)
            .await
            .is_err()
    );
    db.cleanup().await;
}

#[tokio::test]
async fn ssrf_redirects_and_transient_fetches_are_named() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    let model = Recorded::echo();
    let w = worker(&db.pool, Arc::new(model.clone()), Caps::default());
    submit(&db.pool, ws, "ssrf", &site.url("/to-metadata")).await;
    assert_eq!(
        w.run_once().await.unwrap().unwrap().1,
        Outcome::Failed("fetch_refused")
    );
    let busy = submit(&db.pool, ws, "busy", &site.url("/busy")).await;
    assert_eq!(
        w.run_once().await.unwrap().unwrap(),
        (busy, Outcome::Retry("fetch_status"))
    );
    assert_eq!(
        w.run_once().await.unwrap(),
        None,
        "a released job waits out its retry delay"
    );
    assert_eq!(model.call_count(), 0);
    db.cleanup().await;
}

#[tokio::test]
async fn provider_failures_end_named_and_store_no_ideas() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    let invented = Step::Reply(ModelReply {
        text: r#"{"ideas":[{"kind":"number","text":"x","quotes":["a number the page never said: 99%"]}]}"#.to_owned(),
        stop: Stop::EndTurn,
        usage: Usage { input_tokens: 10, output_tokens: 10 },
    });
    let cases: [(Vec<Step>, &str); 2] = [
        (vec![invented], "spans_unmatched"),
        (
            vec![Step::Error(
                repurpose_analysis::model::ProviderError::SpendLimit,
            )],
            "budget_exhausted",
        ),
    ];
    for (n, (steps, code)) in cases.into_iter().enumerate() {
        let id = submit(&db.pool, ws, &format!("k{n}"), &site.url("/article")).await;
        let w = worker(&db.pool, Arc::new(Recorded::steps(steps)), Caps::default());
        assert_eq!(
            w.run_once().await.unwrap(),
            Some((id, Outcome::Failed(code)))
        );
        let view = store::get(&db.pool, ws, id).await.unwrap().unwrap();
        assert_eq!(view.failure.as_deref(), Some(code));
        assert_eq!(view.ideas.len(), 0);
    }
    // A hung provider is cut by the job deadline, never left running.
    let id = submit(&db.pool, ws, "hang", &site.url("/article")).await;
    let mut w = worker(
        &db.pool,
        Arc::new(Recorded::steps(vec![Step::Hang])),
        Caps::default(),
    );
    w.settings.job_deadline = Duration::from_millis(200);
    assert_eq!(
        w.run_once().await.unwrap(),
        Some((id, Outcome::Failed("job_deadline")))
    );
    // The cut call was recorded before it started and stays billed at its estimate.
    let (events, settled, billed): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(e.id), count(s.event_id), coalesce(sum(e.cost_micros), 0)::bigint \
         FROM usage_events e LEFT JOIN usage_settlements s ON s.event_id = e.id \
         WHERE e.source_id = $1",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((events, settled), (1, 0), "one event, never settled");
    assert!(billed > 0, "billed at the estimate, not zero");
    db.cleanup().await;
}

/// Counts calls in flight at once; each call takes a while.
/// Holds each call until `barrier`'s size of calls are in flight at once: concurrency is
/// proved by arrival, not by sleeping and hoping the jobs overlap.
struct Gauge {
    now: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    barrier: tokio::sync::Barrier,
}

impl Model for Gauge {
    fn id(&self) -> &'static str {
        "gauge"
    }

    fn complete<'a>(
        &'a self,
        request: &'a repurpose_analysis::model::ModelRequest,
    ) -> repurpose_analysis::model::BoxFuture<
        'a,
        Result<ModelReply, repurpose_analysis::model::ProviderError>,
    > {
        use std::sync::atomic::Ordering::SeqCst;
        Box::pin(async move {
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(n, SeqCst);
            self.barrier.wait().await;
            self.now.fetch_sub(1, SeqCst);
            Recorded::echo().complete(request).await
        })
    }
}

#[tokio::test]
async fn the_worker_runs_at_most_its_concurrency_and_drains() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    for n in 0..24 {
        submit(&db.pool, ws, &format!("k{n}"), &site.url("/article")).await;
    }
    let concurrency = WorkerSettings::default().concurrency;
    assert_eq!(24 % concurrency, 0, "every barrier generation fills");
    let gauge = Arc::new(Gauge {
        now: 0.into(),
        peak: 0.into(),
        barrier: tokio::sync::Barrier::new(concurrency),
    });
    let mut w = worker(&db.pool, gauge.clone(), Caps::default());
    w.settings.idle_poll = Duration::from_millis(20);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let running = tokio::spawn(Arc::new(w).run(shutdown.clone()));
    for _ in 0..600 {
        if count(
            &db.pool,
            "SELECT count(*) FROM sources WHERE status = 'ready'",
        )
        .await
            == 24
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    running.await.unwrap();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM sources WHERE status = 'ready'"
        )
        .await,
        24
    );
    let peak = gauge.peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        peak <= WorkerSettings::default().concurrency.min(8),
        "peak {peak}"
    );
    assert_eq!(
        peak, concurrency,
        "a full set of jobs ran at once, and never more"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_reused_key_for_another_url_is_refused() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let first = submit(&db.pool, ws, "k", "https://example.com/a").await;
    assert_eq!(
        store::submit(&db.pool, ws, "k", "https://example.com/a", 100)
            .await
            .unwrap(),
        Submitted::Existing(first)
    );
    assert_eq!(
        store::submit(&db.pool, ws, "k", "https://example.com/b", 100)
            .await
            .unwrap(),
        Submitted::KeyReused
    );
    assert_eq!(count(&db.pool, "SELECT count(*) FROM sources").await, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn the_submission_lock_leaves_foreign_key_checks_free() {
    let db = TestDb::new().await;
    let ws = workspace(&db.pool).await;
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query(store::SUBMIT_LOCK)
        .bind(ws)
        .execute(&mut *held)
        .await
        .unwrap();
    // The lock a foreign-key insert takes on the row it references.
    let mut other = db.pool.begin().await.unwrap();
    let shared = sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR KEY SHARE NOWAIT")
        .bind(ws)
        .execute(&mut *other)
        .await;
    assert!(shared.is_ok(), "{shared:?}");
    // Another submission still waits its turn.
    let mut third = db.pool.begin().await.unwrap();
    let queued = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{} NOWAIT",
        store::SUBMIT_LOCK
    )))
    .bind(ws)
    .execute(&mut *third)
    .await;
    assert!(queued.is_err());
    drop((held, other, third));
    db.cleanup().await;
}

/// A model whose call panics: a bug in a job, which the worker must report, not lose.
struct Panics;

impl Model for Panics {
    fn id(&self) -> &'static str {
        "panics"
    }

    fn complete<'a>(
        &'a self,
        _request: &'a repurpose_analysis::model::ModelRequest,
    ) -> repurpose_analysis::model::BoxFuture<
        'a,
        Result<ModelReply, repurpose_analysis::model::ProviderError>,
    > {
        Box::pin(async { panic!("a bug in a job") })
    }
}

#[tokio::test]
async fn a_panicking_job_is_reported_when_the_worker_drains() {
    let db = TestDb::new().await;
    let site = pages().await;
    let ws = workspace(&db.pool).await;
    submit(&db.pool, ws, "k", &site.url("/article")).await;
    let mut w = worker(&db.pool, Arc::new(Panics), Caps::default());
    w.settings.idle_poll = Duration::from_millis(20);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let running = tokio::spawn(Arc::new(w).run(shutdown.clone()));
    for _ in 0..600 {
        if count(&db.pool, "SELECT count(*) FROM usage_events").await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    let drained = running.await.unwrap();
    assert_eq!(drained.panicked, 1);
    assert!(!drained.clean());
    db.cleanup().await;
}

#[tokio::test]
async fn a_member_is_rate_limited_across_workspaces() {
    let db = TestDb::new().await;
    let user = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, 'm@example.test', 'x')")
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();
    let limits = store::Limits {
        pending: 100,
        per_user_hour: 3,
    };
    let (a, b) = (workspace(&db.pool).await, workspace(&db.pool).await);
    for (ws, key) in [(a, "1"), (a, "2"), (b, "3")] {
        let out = store::submit_as(
            &db.pool,
            ws,
            Some(user),
            key,
            "https://example.com/",
            limits,
        )
        .await
        .unwrap();
        assert!(matches!(out, Submitted::Created(_)), "{out:?}");
    }
    // A replay is not a new submission.
    let replay = store::submit_as(&db.pool, a, Some(user), "1", "https://example.com/", limits)
        .await
        .unwrap();
    assert!(matches!(replay, Submitted::Existing(_)), "{replay:?}");
    let over = store::submit_as(&db.pool, b, Some(user), "4", "https://example.com/", limits)
        .await
        .unwrap();
    assert_eq!(over, Submitted::RateLimited);
    assert_eq!(count(&db.pool, "SELECT count(*) FROM sources").await, 3);
    db.cleanup().await;
}
