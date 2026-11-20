//! The `job-queue` load scenario (`docs/END_GOAL.md`), M1's half: `TOTAL` sources through
//! fetch → extract → analyse with the recorded model, while worker generations are
//! killed at seeded points (the whole runtime dropped mid-job, as `SIGKILL` would).
//! Prints one JSON line; `scripts/load-queue.sh` adds the machine and keeps it.
//!
//! Thresholds, fixed before the first run: zero lost sources, zero failed sources,
//! zero duplicate ideas, a digest of every stored idea equal to the digest computed from
//! a kill-free reference, and every provider call either settled as a usage event or
//! interrupted by a kill (unsettled calls ≤ kills × concurrency).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "load harness: a panic is the report"
)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the harness reports on stdout"
)]
#![allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::format_collect,
    reason = "a load harness, read top to bottom; the rate is a report, not arithmetic"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

use repurpose_analysis::model::{
    BoxFuture, Model, ModelReply, ModelRequest, ProviderError, Recorded,
};
use repurpose_analysis::spend::MemorySpend;
use repurpose_db::testing::TestDb;
use repurpose_ingest::fetch::{Fetcher, Limits};
use repurpose_ingest::fixture::{PageServer, Reply};
use repurpose_ingest::guard::Policy;
use repurpose_ingest::resolve::Resolver;
use repurpose_ingest::{extract, extract_fetched};
use repurpose_sources::spend::Caps;
use repurpose_sources::{Worker, WorkerSettings};
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use uuid::Uuid;

const VARIANTS: u64 = 50;
const CONCURRENCY: usize = 32;

fn page(k: u64) -> String {
    let paragraphs: String = (0..8)
        .map(|p| {
            format!(
                "<p>Variant {k} paragraph {p}: bounded queues kept the service honest, and the \
                 measured tail latency fell by {} percent after admission control.</p>",
                (k * 7 + p * 3) % 97
            )
        })
        .collect();
    format!(
        "<html><head><title>Variant {k}</title></head><body><article>{paragraphs}</article></body></html>"
    )
}

/// The recorded model, counting calls without keeping a million requests.
struct Counting {
    calls: AtomicU64,
    echo: Recorded,
}

impl Model for Counting {
    fn id(&self) -> &'static str {
        "recorded"
    }

    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<ModelReply, ProviderError>> {
        self.calls.fetch_add(1, SeqCst);
        Box::pin(async move {
            // A provider takes a moment: kills land mid-call too.
            tokio::time::sleep(Duration::from_millis(2)).await;
            self.echo.complete(request).await
        })
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn digest_line(hash: &mut Sha256, n: i64, ordinal: i32, kind: &str, text: &str, spans: &str) {
    hash.update(
        format!("{n}\u{1f}{ordinal}\u{1f}{kind}\u{1f}{text}\u{1f}{spans}\u{1e}").as_bytes(),
    );
}

fn main() {
    let total = env("TOTAL", 100_000);
    let kills = env("KILLS", 10);
    let seed = env("SEED", 20_261_120).max(1);
    let main_rt = tokio::runtime::Runtime::new().unwrap();
    let db = main_rt.block_on(TestDb::new());
    let site = main_rt
        .block_on(PageServer::start(|path| {
            let k: u64 = path.trim_start_matches("/a/").parse().unwrap_or(0);
            Reply::ok("text/html; charset=utf-8", page(k))
        }))
        .unwrap();
    let base = site.url("/a/");
    let pool = db.pool.clone();
    let model = Arc::new(Counting {
        calls: AtomicU64::new(0),
        echo: Recorded::echo(),
    });

    // The reference: each variant's ideas, computed once without the queue or a kill.
    let reference: Vec<Vec<(i32, String, String, String)>> = main_rt.block_on(async {
        let fetcher = Fetcher::new(
            Policy::loopback_fixture(),
            Limits::default(),
            Resolver::default(),
        )
        .unwrap();
        let mut all = Vec::new();
        for k in 0..VARIANTS {
            let fetched = fetcher.fetch(&format!("{base}{k}")).await.unwrap();
            let article = extract_fetched(&fetched, &extract::Limits::default()).unwrap();
            let analysis = repurpose_analysis::analyse(
                &Recorded::echo(),
                &MemorySpend::with_cap(u64::MAX / 2),
                &article.text,
                &repurpose_analysis::Settings::default(),
            )
            .await
            .unwrap();
            let ideas = analysis
                .ideas
                .iter()
                .enumerate()
                .map(|(i, idea)| {
                    let kind = serde_json::to_value(idea.kind)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned();
                    (
                        i32::try_from(i).unwrap(),
                        kind,
                        idea.text.clone(),
                        serde_json::to_value(&idea.spans).unwrap().to_string(),
                    )
                })
                .collect();
            all.push(ideas);
        }
        all
    });

    let ws = Uuid::now_v7();
    main_rt.block_on(async {
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'load')")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
        // Seeded in batches: one statement per 50,000 rows stays inside the statement timeout.
        let total = i64::try_from(total).unwrap();
        for from in (0..total).step_by(50_000) {
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, idempotency_key, url, created_at) \
                 SELECT gen_random_uuid(), $1, lpad(n::text, 9, '0'), $2 || (n % $3)::text, \
                        now() + make_interval(secs => n / 1000000.0) \
                 FROM generate_series($4, least($4 + 50000, $5) - 1) n",
            )
            .bind(ws)
            .bind(&base)
            .bind(i64::try_from(VARIANTS).unwrap())
            .bind(from)
            .bind(total)
            .execute(&pool)
            .await
            .unwrap();
        }
    });

    let mut rng = seed;
    let mut kill_points: Vec<u64> = (0..kills)
        .map(|_| xorshift(&mut rng) % total.max(1))
        .collect();
    kill_points.sort_unstable();
    let ready = |rt: &tokio::runtime::Runtime| -> i64 {
        rt.block_on(
            sqlx::query_scalar("SELECT count(*) FROM sources WHERE status IN ('ready', 'failed')")
                .fetch_one(&pool),
        )
        .unwrap()
    };
    let started = Instant::now();
    let mut generations = 0u64;
    let mut killed = 0u64;
    let deadline = Duration::from_secs(env("MAX_SECONDS", 4 * 3600));
    loop {
        generations += 1;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let worker = Arc::new(Worker {
            pool: pool.clone(),
            fetcher: Fetcher::new(
                Policy::loopback_fixture(),
                Limits::default(),
                Resolver::default(),
            )
            .unwrap(),
            model: model.clone(),
            analysis: repurpose_analysis::Settings::default(),
            extract: extract::Limits::default(),
            caps: Caps {
                workspace_daily: i64::MAX / 4,
                global_daily: i64::MAX / 4,
            },
            settings: WorkerSettings {
                concurrency: CONCURRENCY,
                lease: Duration::from_secs(3),
                job_deadline: Duration::from_secs(2),
                idle_poll: Duration::from_millis(20),
                retry_after: Duration::from_secs(1),
            },
        });
        let token = tokio_util::sync::CancellationToken::new();
        rt.spawn(worker.run(token.clone()));
        let next_kill = kill_points.get(usize::try_from(killed).unwrap()).copied();
        let done = loop {
            std::thread::sleep(Duration::from_secs(1));
            let finished = u64::try_from(ready(&main_rt)).unwrap();
            if finished >= total || started.elapsed() > deadline {
                break true;
            }
            if next_kill.is_some_and(|at| finished >= at) {
                break false;
            }
        };
        // SIGKILL semantics: every task of this generation stops where it stands.
        rt.shutdown_background();
        if done {
            break;
        }
        killed += 1;
    }
    let seconds = started.elapsed().as_secs_f64();

    let pool_ref = &pool;
    let count = |sql: &'static str| -> i64 {
        main_rt
            .block_on(sqlx::query_scalar::<_, i64>(sql).fetch_one(pool_ref))
            .unwrap()
    };
    let ready_n = count("SELECT count(*) FROM sources WHERE status = 'ready'");
    let failed = count("SELECT count(*) FROM sources WHERE status = 'failed'");
    let usage = count("SELECT count(*) FROM usage_events");
    let ideas = count("SELECT count(*) FROM ideas");
    let index = |n: u64| usize::try_from(n % VARIANTS).unwrap();
    let expected_ideas: i64 = (0..total)
        .map(|n| i64::try_from(reference[index(n)].len()).unwrap())
        .sum();
    // Stored spans come back as jsonb text: normalise the reference the same way, once per variant.
    let reference: Vec<Vec<(i32, String, String, String)>> = reference
        .into_iter()
        .map(|ideas| {
            ideas
                .into_iter()
                .map(|(o, k, t, spans)| {
                    let v: serde_json::Value = serde_json::from_str(&spans).unwrap();
                    let text: String = main_rt
                        .block_on(
                            sqlx::query_scalar("SELECT $1::jsonb::text")
                                .bind(v)
                                .fetch_one(pool_ref),
                        )
                        .unwrap();
                    (o, k, t, text)
                })
                .collect()
        })
        .collect();
    let mut stored = Sha256::new();
    let mut after: i64 = -1;
    let last = i64::try_from(total).unwrap() - 1;
    while after < last {
        let rows = main_rt
            .block_on(
                sqlx::query(
                    "SELECT s.idempotency_key::bigint AS n, i.ordinal, i.kind, i.text, i.spans::text AS spans \
                     FROM sources s JOIN ideas i ON i.source_id = s.id \
                     WHERE s.workspace_id = $2 AND s.idempotency_key::bigint > $1 AND s.idempotency_key::bigint <= $1 + 20000 \
                     ORDER BY 1, 2",
                )
                .bind(after)
                .bind(ws)
                .fetch_all(pool_ref),
            )
            .unwrap();
        for row in &rows {
            if std::env::var("DEBUG").is_ok() && row.get::<i64, _>("n") == 0 {
                eprintln!(
                    "S {} {} {} {}",
                    row.get::<i32, _>("ordinal"),
                    row.get::<String, _>("kind"),
                    row.get::<String, _>("text"),
                    row.get::<String, _>("spans")
                );
            }
            digest_line(
                &mut stored,
                row.get("n"),
                row.get("ordinal"),
                row.get("kind"),
                row.get("text"),
                row.get("spans"),
            );
        }
        after += 20000;
    }
    let mut expected = Sha256::new();
    for n in 0..total {
        for (ordinal, kind, text, spans) in &reference[index(n)] {
            if std::env::var("DEBUG").is_ok() && n == 0 {
                eprintln!("R {ordinal} {kind} {text} {spans}");
            }
            digest_line(
                &mut expected,
                i64::try_from(n).unwrap(),
                *ordinal,
                kind,
                text,
                spans,
            );
        }
    }
    let calls = model.calls.load(SeqCst);
    let stored = hex::encode(stored.finalize());
    let expected = hex::encode(expected.finalize());
    let unsettled = i64::try_from(calls).unwrap() - usage;
    let total_i = i64::try_from(total).unwrap();
    let pass = ready_n == total_i
        && failed == 0
        && ideas == expected_ideas
        && stored == expected
        && unsettled >= 0
        && unsettled <= i64::try_from(killed).unwrap() * i64::try_from(CONCURRENCY).unwrap();
    println!(
        "{}",
        serde_json::json!({
            "scenario": "job-queue (M1: ingest -> analyse)",
            "total": total, "seed": seed, "kills": killed, "generations": generations,
            "concurrency": CONCURRENCY, "seconds": seconds, "jobs_per_second": (total as f64) / seconds,
            "ready": ready_n, "failed": failed, "lost": total_i - ready_n - failed,
            "ideas": ideas, "expected_ideas": expected_ideas,
            "provider_calls": calls, "usage_events": usage, "unsettled_calls": unsettled,
            "digest": stored, "reference_digest": expected,
            "thresholds": {
                "lost": "== 0", "failed": "== 0", "ideas": "== expected_ideas",
                "digest": "== reference_digest", "unsettled_calls": "0 ..= kills x concurrency"
            },
            "pass": pass,
        })
    );
    main_rt.block_on(db.cleanup());
    if !pass {
        std::process::exit(1);
    }
}
