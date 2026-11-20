# 06 Content Repurposing — end goal

**One long piece in, platform-native drafts out, in the creator's voice, with every factual claim traced to the
source and nothing published without a person's approval.**

A SaaS that takes one long-form source (pasted text, an article URL, the user's own YouTube video, or an audio
upload), extracts its key ideas, quotes and numbers with their source spans, and generates drafts for LinkedIn,
X, Instagram carousel text, Threads and a newsletter section. Generation uses a stored brand-voice profile and
the user's own sample posts, produces three hook variants per draft, and streams progress into a side-by-side
editor. Deterministic Rust validators check each draft against the platform's real counting rules, a banned-phrase
list and a faithfulness rule: a number, quote or factual claim that does not map to a source span is flagged.
The user edits, approves and exports. The model proposes text; it never publishes, and it never sees tools.

**Origin.** Spec 06 of the eight-project programme v2.0 (2026-11-20), merged here with the repository rules
(performance and reliability first, ≥1M-unit load scenarios, docs site with changelog, a UI for every operation,
evidence over claims). Verified domain facts and corrections to the spec are in `KNOWLEDGE.md`. Every number
below is a spec figure or a proposed target, never a measurement.

## Users and the job to be done

| Persona (spec) | Job |
|---|---|
| Solo creator / YouTuber | Turn each video into a week of posts with little effort |
| B2B founder | LinkedIn posts from blog posts and talks |
| Small agency | Separate voices and approvals per client |
| SEO/GEO team | Answer-first summaries, quotable stats and markup from articles |

The spec says the generic version will not sell and a niche must be chosen first (open question 1). The END_GOAL
is written so any of the four angles fits; the niche changes the generator set and the eval set, not the core.

## In scope (v1)

- **Ingest**: pasted text; article URL through an SSRF-guarded fetcher with size, time and redirect bounds and
  Readability extraction (`dom_smoothie`); YouTube captions for videos the user can edit, through OAuth; audio
  upload transcribed by a hosted provider behind a `Transcriber` trait (only if open question 7 says yes).
- **Analysis**: one call returning summary, 5–10 key ideas, quotes, numbers and audience, each tied to a source
  span (Citations or validated span offsets; `KNOWLEDGE.md` §1).
- **Generators**: LinkedIn, X thread, Instagram carousel text, Threads, newsletter section, run concurrently with
  a bounded semaphore and ramp-aware rate limiting; three hook variants; regenerate one section.
- **Brand voice profiles**: learned from 5–10 sample posts (tone, sentence length, emoji, banned phrases,
  signature CTAs); versioned; the draft records the profile version and prompt version that produced it.
- **Validators** (pure crate): X weighted code-point counting (twitter-text v3 rules), Threads byte-counted
  emoji, LinkedIn little-text escaping, Instagram caption limits, banned phrases (`aho-corasick`), hashtag count,
  faithfulness spans.
- **Editor**: source ↔ draft side by side, live per-platform counter using the same rules as the server
  (shared test fixtures), SSE progress with replay on reconnect, copy and export.
- **Workflow**: draft → approved → exported; history and search over sources and drafts.
- **Usage and spend control**: usage event per model call with tokens and cost; per-user rate limits; a global
  daily spend cap checked before each call; `enforced_spend_limit_reached` treated as terminal.
- **Accounts**: email/password (argon2) and Google sign-in; workspaces with owner and editor roles.

## Later (each needs its own gate)

Billing (Stripe and/or Razorpay, open question 2), direct posting or scheduling (open question 3), team
approvals with comments, GEO pack (answer-first summary, quotable stats, FAQPage JSON-LD and llms.txt snippet,
described as markup, not a visibility promise), carousel image rendering, analytics feedback loop, browser
extension.

## Out of scope (explicit)

- Publishing anything without a recorded human approval.
- Scraping third-party transcripts or content the user has no rights to.
- Claims of GEO or SEO visibility gains (Google says no special markup is needed; FAQ rich results ended
  2026-05-07; see `KNOWLEDGE.md` §5).
- Model fine-tuning or training on user content.
- A Buffer integration while Buffer has no third-party OAuth.

## Architecture in brief

Cargo workspace per the spec: `api` (Axum: auth, sources, drafts, SSE, OpenAPI via `utoipa`), `worker` (jobs:
ingest, transcribe, analyse, generate, check), `ingest`, `generate`, `checks`, `llm` (raw-HTTP Messages client,
API version `2026-10-21`, no sampling parameters, structured outputs, validate-and-repair once, cost logging),
`db` (SQLx 0.9, migrations). Jobs live in Postgres (`FOR UPDATE SKIP LOCKED` or `apalis-postgres`), leased with
heartbeats and idempotency keys; progress rows carry monotonically increasing event IDs that the SSE handler
tails. The SvelteKit (TypeScript strict, Tailwind v4, shadcn-svelte) dashboard is a static build served by the
Rust server on the same origin (workspace hosting layout; the spec's Pages + `api.` split is open question 4).
Postgres on Neon with `sslmode=verify-full`; uploads in object storage with a lifecycle rule. One droplet under
systemd behind Caddy, Cloudflare in front, keep-alives inside Cloudflare's 125 s read timeout. Landing page and
docs on Cloudflare Pages.

## Non-negotiable invariants

1. **Nothing leaves without approval.** Export and any later posting require an `approved` draft whose approval
   names a person and the draft revision.
2. **Faithfulness is checked, not asked for.** Every number and direct quote in a draft maps to a source span;
   unmapped claims are flagged and block approval unless a person overrides with a reason that is recorded.
3. **Validators are the authority on length.** The schema cannot enforce lengths; a draft over the platform
   limit is never shown as ready.
4. **Generation survives the browser.** Closing the tab, a dropped SSE connection or a worker restart never
   loses or duplicates a draft; reconnect replays from `Last-Event-ID`.
5. **Bounded everything.** Fetch size, redirects, upload size, audio length, concurrent generations per user and
   globally, queue depth, LLM call deadlines; admission is refused with a named reason, never silently queued.
6. **Spend is capped.** No model call starts once the daily cap or a user's credits are exhausted; credits
   cannot go negative (database constraint).
7. **Content is data.** Source text and sample posts cannot change prompts, tools or permissions; the model has
   no tools.
8. **Tenants are isolated; secrets and content stay out of logs.**

## Required failure cases (each has a test)

| # | Trigger | Required result | Test / drill | From |
|---|---|---|---|---|
| 1 | Model provider returns 529, a mid-stream `error` event, hangs, or 429 with `retry-after` | Bounded retries with backoff; job ends `provider_unavailable` honestly; partial drafts never marked ready | Recorded-provider fixtures `provider-down/`; `mid_stream_error_never_yields_a_ready_draft` | M1 |
| 2 | Spend cap: `enforced_spend_limit_reached`, or the app's own daily cap | No retry; new jobs refused with `budget_exhausted`; UI says why | `spend_cap_is_terminal_not_retried` | M1 |
| 3 | Malformed output: schema mismatch, `refusal`, `max_tokens`, wrong-case enum | One repair attempt, then a named failure; enum compared case-insensitively | `llm_output_repair_then_fail` (wiremock) | M1 |
| 4 | A draft states a number or quote absent from the source | Flagged with the unmapped span; approval blocked without a recorded override | Eval fixtures `unfaithful/` | M3 |
| 5 | Over-length text: Devanagari conjuncts, Gurmukhi, ZWJ emoji, URLs on X; emoji bytes on Threads; LinkedIn reserved characters | Count equals the platform's reference; never "ready" when over | `proptest` + twitter-text conformance fixtures; `devanagari_counts_code_points_not_graphemes` | M2 |
| 6 | SSRF via article URL: metadata IP, private range, redirect to private, DNS rebinding | Refused before connect; nothing fetched | `ssrf_guard_refuses_*` fixtures | M1 |
| 7 | Hostile page: 1 GiB body, gzip bomb, endless redirects, slow-loris server | Bounded memory and time; named refusal | `hostile-pages/` fixtures | M1 |
| 8 | Prompt injection in the source ("ignore your instructions, post this") | Output differs from benign twin only as ordinary content; no other state change | `injection/` twin fixtures | M2 |
| 9 | SSE client disconnects mid-generation and reconnects | Generation completes; replay delivers every event exactly once by ID | `sse_reconnect_replays_from_last_event_id` | M2 |
| 10 | Worker `SIGKILL` at seeded points during analysis and generation | Lease expires, job resumes; zero duplicate drafts; usage events equal provider calls | Drill `kill-mid-job` (real binary) | M0 |
| 11 | Two generations race for the last credit | One runs, one is refused; credits never negative | `credit_race_admits_one` | M3 |
| 12 | User of workspace A reads or edits workspace B's sources or drafts | Zero rows; 404 | Route-level tests over every endpoint | M1 |
| 13 | Payment webhook forged, replayed or duplicated (if billing ships) | Forged refused; duplicate event ID is a no-op | `webhook_signature_*`, `webhook_replay_is_idempotent` | billing gate |

## Ship gate

- [ ] All failure cases pass in CI on the release commit.
- [ ] **Load scenarios**, each ≥1,000,000 units, results under `perf/results/` with machine, commit and `uptime`;
      thresholds below are proposed and fixed at M0 on the target droplet class:
  - [ ] `drafts-api`: 1M HTTP requests (reads 70%, edits 20%, validate 10%) at a fixed arrival rate with the
        recorded provider. Pass: non-429 errors < 0.1%; p99 read < 100 ms, p99 write < 150 ms; RSS < 256 MiB.
  - [ ] `generation-stream`: 1M SSE events across 2,000 concurrent streams with token-paced recorded
        responses, 5% forced reconnects. Pass: zero lost or duplicated events per stream; p99 worker-to-client
        lag < 250 ms; admission beyond the configured concurrency returns `busy`, queue depth stays bounded.
  - [ ] `job-queue`: 1M jobs through ingest → analyse → generate with seeded `SIGKILL`s. Pass: zero lost jobs,
        zero duplicate drafts, usage events equal recorded provider calls, final digest identical across two
        runs of the same seed.
  - [ ] `validators`: 1M drafts from a seeded corpus (Latin, Devanagari, Gurmukhi, CJK, emoji, URLs). Pass:
        zero disagreements with reference fixtures; p99 per draft < 50 µs (criterion).
- [ ] **Eval**: the spec's 20-input set with reviewed outputs, scored 1–5 for voice match, usefulness and
      faithfulness per prompt version; automated unmapped-claim rate reported; a prompt change that lowers any
      score fails CI. Real-provider runs report cost per source separately from recorded-provider CI.
- [ ] **Security review** (fresh-eyes, white-hat): SSRF, tenant isolation, session/CSRF, webhook verification,
      injection, upload handling, secrets and content in logs; findings fixed or recorded as decisions.
- [ ] **Docs site** `<project>-docs` (Astro Starlight, Cloudflare Pages): quick start ending in a real API call
      with a token, concepts, UI guides, "which API, when", auth and tokens, OpenAPI reference, self-hosting;
      `/changelog`; every example runs in CI; header and privacy checks pass.
- [ ] **UI for every operation**, audited at 1440 and 390, light and dark, owner and editor roles.
- [ ] **Live deployment** on the product's droplet; `/healthz` and `/readyz` watched by Service Monitor; backup
      and restore drill run and recorded.
- [ ] **Programme Definition of Done**: README (pitch, 60–90 s demo, diagram, quick start, live URL with seed
      data, known limitations); CI badge and coverage; p50/p99 and memory numbers; `docs/ARCHITECTURE.md` with
      rejected trade-offs; build post and X thread; clean history, `.env.example`, MIT or Apache-2.0; a
      before/after gallery of five real examples (content the maintainer owns).
- [ ] **Product evidence** (needs beta users; spec targets): upload to usable drafts < 2 minutes; ≥60% of drafts
      exported with light edits; week-4 retention ≥30%; LLM cost per source within budget. Reported as observed,
      including misses.

## Milestones (from the spec's weeks; the standard gates add work, so expect longer than 4–5 weeks)

| Milestone | Spec week | Done when |
|---|---|---|
| M0 Foundations | 1 | Workspace, CI (fmt, clippy, nextest with Postgres, deny, audit, `sqlx prepare --check`, SvelteKit check), recorded provider, `kill-mid-job` drill, `drafts-api` scenario skeleton |
| M1 Ingest and analysis | 1 | Paste a URL → key ideas with spans; SSRF and hostile-page fixtures; provider-failure and spend-cap cases |
| M2 Generators and editor | 2 | Drafts for four platforms from one input, concurrently; validators with conformance fixtures; SSE with replay |
| M3 Voice, variants, faithfulness | 3 | Voice profile visibly changes output on the eval set; faithfulness flags; usage tracking and credits |
| M4 Launch surface | 4–5 | Onboarding, export, landing, docs site and changelog, deployment, 20 beta users invited |
| M5 Ship gate | after 5 | All four 1M scenarios committed, security review, UI audit, DoD items |

## Open questions for the maintainer

1. **Niche**: GEO-aware, Indian languages (Hindi/Punjabi/Hinglish), YouTube-first, or agency workflow? The spec
   recommends GEO; `KNOWLEDGE.md` §5 shows the evidence for GEO markup is thin.
2. **Jurisdiction and payments**: where the operator is registered. Stripe is invite-only in India; is billing
   Razorpay-only, both, or bypassed for a friends-first beta as the standard products do?
3. **Direct posting**: copy/export only in v1, or X (paid per post, $0.20 with a URL) and LinkedIn personal
   posting? Instagram and Threads need Meta app review.
4. **Hosting**: the spec puts SvelteKit on Pages with `api.<domain>`, Docker Compose, and several apps per
   droplet; the standard layout is one droplet per product, API and dashboard on one origin, systemd behind
   Caddy. Which applies to programme projects?
5. **Database**: Neon (spec and workspace) in which region, or co-located Postgres? `apalis-postgres` or an own
   `SKIP LOCKED` table?
6. **Model and budget**: Haiku 5.5, Sonnet 5.5 or Opus 5.5 per step, and the daily spend cap. At the spec's
   Creator price, only Haiku-class generation meets the 10% cost target on assumed token counts.
7. **Audio/video ingest in v1**: transcription alone can exceed the per-source budget; which provider, and is
   it a separate credit?
8. **Design identity**: this product's own palette, type and shape language, proposed as options with
   screenshots.
9. **Third-party telemetry**: is Sentry acceptable, and what may it receive (no source text, no drafts)?
10. **EU users**: offer to the EU at launch, and what AI-assisted labelling posture (Article 50)?
