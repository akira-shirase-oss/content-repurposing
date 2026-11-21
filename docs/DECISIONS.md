# Decisions

Trade-offs and decisions, D-001 onward. Each entry says what was chosen and why; a
superseded decision is marked, never rewritten.

## D-001 — Base: the Backend Platform starter template

The repository begins as a copy of the Backend Platform's copy-and-own starter template —
the bounded HTTP layer, the three-role PostgreSQL split with its protections manifest,
the accounts kit, releases, restore drills, and the CI + evidence kit — renamed for this
product. The commit it came from is in `TEMPLATE_ORIGIN`; later template changes arrive
only as reviewed diffs. Nothing shares a package, service, login or database with the
template's repository or any sibling product.

## D-002 — Port band 63400-63499

Every port this repository binds on a shared development machine stays inside
63400–63499: the server on 63450, the development PostgreSQL on 63432, the template's
other defaults shifted into the band. `scripts/check-ports.sh` fails on a literal outside
the band, and the gate proves it can fail. Rationale: Linux's ephemeral range ends at
60999, so CI and a second checkout never collide.

## D-003 — Stack

Rust backend on Axum, SQLx and tokio (the template's stack); SvelteKit + TypeScript
(strict) for the web UI once the maintainer chooses this product's design system — no
`web/` product code before that. No on-chain code; the model provider is behind a
recorded fixture port. Everything else follows `docs/END_GOAL.md`.

## D-004 — M1 dependencies: Readability extraction, charset and gzip decoding, outbound TLS

The article fetcher and extractor add: `dom_smoothie` (a Readability port) and the
`dom_query` DOM it parses into (used directly to bound element count and nesting depth
before Readability's recursive passes run); `encoding_rs` (the WHATWG encoding standard);
`flate2` on its pure-Rust backend (bounded gzip); `hyper` client, `rustls` and
`tokio-rustls` on ring with `webpki-roots` (the TLS stack the database client already
links; no OpenSSL); `url`. `dom_query` brings four MPL-2.0 crates (`cssparser`,
`cssparser-macros`, `selectors`, `dtoa-short`): file-level copyleft, used unmodified, so
`deny.toml` allows MPL-2.0 for those four crates only, never as a general license. A
change to any of them, or a vendored copy, reopens this decision.

## D-005 — Key ideas cite verbatim quotes; the service computes the spans

The model returns each idea with verbatim quotes from the source, never offsets. The
service locates each quote exactly (byte-for-byte, case-sensitive, no fuzzy match) in the
stored article text and stores half-open byte spans; a quote that is not in the source is
refused, the answer gets one repair attempt, and an idea with no located quote is dropped
and counted (`refused_ideas`). Every read re-checks every stored span against the stored
text and marks a mismatch `verified: false`. Rationale: models cannot count bytes, and a
quote the service found is a claim the service can prove. Until a live provider is chosen
the server runs the recorded adapter (it quotes source sentences back) and says so at
start-up; the provider client is owed before M1 can be *Done*.


## D-006 — A usage event is written when a call is admitted; failed calls are billed

Providers bill timeouts and mid-stream failures, so a call that errors is settled at its
reserved estimate, never at zero. The usage event (model, estimated tokens and cost) is
written in the same transaction that reserves the spend, before the call starts, and a
successful call adds one append-only `usage_settlements` row with its real usage. So no
provider call is ever without a usage event, and END_GOAL #10's "usage events equal
provider calls" holds exactly in every run without kills. A worker killed after the
admission commits and before the request leaves has an event (billed at the estimate) for
a call that never happened: at most one per in-flight job per kill, and the error is
always over-billing, never a call nobody paid for. The job-queue harness reports both
directions (`unrecorded_calls`, which must be zero, and `events_without_call`).
