# 06 Content Repurposing — verified knowledge

Checked on 2026-11-20 against primary sources where they exist. Every fact carries a source. **UNVERIFIED**
marks what could not be confirmed. "The spec says" marks numbers taken from the v2.0 spec without confirmation.
Prices and limits are dated observations and will drift; re-check before any decision that depends on them.

## 1. Claude API (the generation engine)

**Current models and list prices** (per million tokens, first-party API)
([models overview](https://platform.claude.com/docs/en/about-claude/models/overview),
[pricing](https://platform.claude.com/docs/en/about-claude/pricing)):

| Model | ID | Input | Output | Cache read | Notes |
|---|---|---|---|---|---|
| Claude Opus 5.5 | `claude-opus-5-5` | $4 | $20 | $0.20 | default effort `medium`; thinking always on |
| Claude Sonnet 5.5 | `claude-sonnet-5-5` | $2 | $10 | $0.10 | default effort `high` |
| Claude Haiku 5.5 | `claude-haiku-5-5` | $0.10 | $0.50 | $0.01 | prompts over 100,000 tokens: $0.50 / $2.50 |

All three have a 1M-token context and 128K max output. Batch API is 50% off; `inference_geo: "us"` costs 1.1x.
Every model ID is a pinned snapshot (same source).

**There is no official Rust SDK.** Official SDKs are Python, TypeScript, C#, Go, Java, PHP and Ruby
([SDK list](https://platform.claude.com/docs/en/cli-sdks-libraries/overview)). The Rust crates on crates.io
(`anthropic-sdk-rust`, `anthropic-ai-sdk`, others) describe themselves as unofficial. The spec's plan, a shared
crate over raw HTTP with `reqwest`, is the right call; the crate owns the wire types.

**API version `2026-10-21` breaks two things a hand-written client is likely to send**
([versioning](https://platform.claude.com/docs/en/api/versioning)):
- `temperature`, `top_p` and `top_k` are removed; sending any of them returns 400 on every model. A
  "foundation-llm" crate must not expose them.
- Assistant-message prefill is not a steering tool on current models: the documented resume procedure for
  Claude 4.6 and later sends a user message instead
  ([streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)), and prefill is incompatible
  with JSON outputs ([structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)).

**Structured outputs are GA** (no beta header) through `output_config.format = {type: "json_schema", schema}`
on `/v1/messages`, or `strict: true` on a tool
([structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)). Facts that
shape the design:
- Not supported in the schema: `minLength`/`maxLength`, `minimum`/`maximum`, `oneOf`, recursion, array
  constraints beyond `minItems` 0 or 1; `additionalProperties` must be `false`. **Platform length limits
  therefore cannot be enforced by the schema; the Rust validators are the only enforcement.**
- Limits per request: 20 strict tools, 24 optional parameters in total, 16 parameters with union types;
  beyond that, "Schema is too complex for compilation" (400). Compilation timeout 180 s; compiled grammars are
  cached 24 h from last use, so the first request with a new schema is slower.
- Output may not match the schema on `stop_reason: "refusal"` or `"max_tokens"`. **Enum casing is not
  guaranteed** ("Conversation Topic 3" can come back for "Conversation topic 3"); compare case-insensitively.
- Required properties are emitted first, then optional ones, regardless of schema order.
- Changing `output_config.format` invalidates the prompt cache for that conversation.
- Incompatible with Citations (400) and prefill.

`schemars` 1.2.3 is current ([crates.io](https://crates.io/crates/schemars)); generated schemas must be
post-processed to strip the unsupported keywords above, because only the Python and TypeScript SDKs do that
transformation automatically.

**Faithfulness via Citations.** The Citations feature returns `cited_text` plus a character, page or
content-block location that the API "guaranteed to contain valid pointers to the provided documents", and
`cited_text` does not count toward output tokens
([citations](https://platform.claude.com/docs/en/build-with-claude/citations)). It cannot be combined with
`output_config.format`; the documented pattern for structured data with citations is one line per field,
parsed in code. For the spec's "every factual claim maps to a source span" check, this is a stronger primitive
than asking the model to quote, but it forces a two-call design (analysis with citations, generation with JSON).

**Streaming (the upstream half of SSE).** Event order is `message_start`, per-block
`content_block_start` / `content_block_delta` / `content_block_stop`, `message_delta` (usage is cumulative),
`message_stop`; `ping` events can appear anywhere; an `error` event (for example `overloaded_error`) can arrive
**after** the HTTP 200; unknown event types must be ignored
([streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)). On current models an
interrupted stream is resumed by a new user message that contains the partial text, not by prefill (same page).

**Rate and spend limits** ([rate limits](https://platform.claude.com/docs/en/api/rate-limits)): Start tier
allows 4,000 RPM / 2,000,000 ITPM / 400,000 OTPM for each of Opus 5.5, Sonnet 5.5 and Haiku 5.5; cached input
does not count toward ITPM. New organisations may start in a lower "Evaluation" tier. **Ramp limits** return
429 `ramp_limit_exceeded` after a sharp increase even under the tier limit, so a burst of concurrent generators
must ramp. Monthly spend caps: Start $500, Build $1,000, Scale $200,000; at the cap, requests return 429
`enforced_spend_limit_reached` **with no `retry-after`**, and retrying is pointless until the next month. The
job runner must treat that code as terminal, distinct from an ordinary 429.

**Unit economics, illustrative only.** The spec targets LLM cost under 10% of the plan price. For the $9 end of
the Creator plan with 30 sources, that is about $0.03 per source. With assumed token counts (analysis: 10,000 in,
1,500 out; five platform generations: 4,000 in, 800 out each), Haiku 5.5 costs about $0.006 per source and
Sonnet 5.5 about $0.115 per source (arithmetic on the list prices above). Sonnet-class generation at that plan
price misses the target by roughly 4x unless caching, batching or fewer variants close the gap. Real token
counts must be measured on the eval set.

## 2. Streaming to the editor (Axum SSE)

- `axum` 0.8.9 is current; `axum::response::sse::{Sse, Event, KeepAlive}`; the handler returns
  `Sse<impl Stream<Item = Result<Event, E>>>`; `KeepAlive` defaults to a 15 s interval and an empty comment
  ([docs.rs](https://docs.rs/axum/latest/axum/response/sse/struct.KeepAlive.html)).
- **Cloudflare in front of the droplet** closes a response after 125 s without data (Proxy Read Timeout,
  configurable only on Enterprise) and returns 524
  ([Cloudflare connection limits](https://developers.cloudflare.com/fundamentals/reference/connection-limits/)).
  A long generation must emit keep-alives well inside that window; the 15 s default does.
- Compression middleware that buffers will hold SSE frames; exclude `text/event-stream` from compression
  (community reports, e.g. [stackharbor](https://stackharbor.com/en/knowledge-base/cffix-websockets-sse-behind-cloudflare/);
  **UNVERIFIED** for `tower-http` specifically — test it).
- `EventSource` sends cookies on same-origin requests; cross-origin needs `withCredentials` and CORS
  ([MDN EventSource](https://developer.mozilla.org/en-US/docs/Web/API/EventSource/EventSource)). Over
  HTTP/1.1 browsers cap SSE at six connections per origin; HTTP/2 removes this
  ([MDN, using SSE](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events)).
  The workspace's same-origin layout (dashboard served by the Rust server) avoids the CORS case entirely.
- Design consequence: generation must not live inside the HTTP request. The worker writes progress rows; the SSE
  handler tails them with event IDs, so a reconnect with `Last-Event-ID` replays and a closed tab loses nothing.

## 3. Platform posting APIs: real restrictions and costs

| Platform | Posting access | Cost | Limits that matter |
|---|---|---|---|
| **X** | Pay-per-use credits; no subscription tiers ([X pricing](https://docs.x.com/x-api/getting-started/pricing)). Third-party reports say the free tier closed to new developers on 2026-02-06 and Basic subscribers were migrated on 2026-06-01 ([opentermsarchive](https://opentermsarchive.org/en/memos/x-replaces-its-dollar200-entry-level-api-plan-with-a-pay-per-use-model/); **UNVERIFIED** on docs.x.com) | $0.015 per post; **$0.200 per post that contains a URL**; reads $0.005 | 3M post reads per month cap |
| **LinkedIn (personal)** | "Share on LinkedIn" product grants `w_member_social` from the developer portal ([Microsoft Learn](https://learn.microsoft.com/en-us/linkedin/consumer/integrations/self-serve/share-on-linkedin)) | free | 150 requests per member per day, 100,000 per app per day (same page, last updated 2023-12, documents the older `ugcPosts` endpoint); posts are created as `PUBLISHED` |
| **LinkedIn (company page)** | `w_organization_social` via the Community Management API, which needs an application and approval; vendors report months ([LinkedIn migration guide](https://learn.microsoft.com/en-us/linkedin/marketing/community-management/community-management-api-migration-guide); timing **UNVERIFIED**) | free | page-admin only |
| **Instagram** | Professional accounts only; `instagram_business_content_publish` (or `instagram_content_publish`); media must be at a **public URL** when publishing ([Meta](https://developers.facebook.com/documentation/instagram-platform/content-publishing)) | free | 100 API posts per 24 h in one section, 50 in another (the page contradicts itself); carousels up to 20 items |
| **Threads** | `threads_basic` + `threads_content_publish`; without advanced access only the app's testers can be posted for ([Meta](https://developers.facebook.com/documentation/threads/posts)) | free | 500 characters; ≤5 links (else `THREADS_API__LINK_LIMIT_EXCEEDED` since 2025-12-22); 250 posts per 24 h; wait ~30 s between container and publish |
| **Buffer** | GraphQL API open since 2026-05-27 ([Buffer](https://buffer.com/resources/buffer-api-is-here/)); legacy REST retires 2027-02-01. Third-party reports say OAuth for third-party apps is not enabled yet ([zernio](https://zernio.com/blog/buffer-api); **UNVERIFIED**) | — | personal API keys only means a SaaS cannot connect its users' Buffer accounts |

Consequences: the spec's P1 "LinkedIn, X APIs or Buffer-style integrations" has a real price on X, an approval
wall for LinkedIn pages, Meta app review for Instagram and Threads, and no multi-tenant Buffer path today. The
spec's own mitigation, "start with copy/export", is the only option that ships without a third party's approval.

## 4. Character counting is per platform, not "Unicode-aware" in general

- **X** counts NFC-normalised **code points** with weights from twitter-text config v3: code points U+0000 to
  U+10FF (and three small punctuation ranges) weigh 1, everything else 2; every emoji weighs 2 including ZWJ
  sequences; every URL counts as 23 ([X counting characters](https://docs.x.com/fundamentals/counting-characters),
  [twitter-text v3.json](https://raw.githubusercontent.com/twitter/twitter-text/master/config/v3.json)).
  Devanagari (U+0900–U+097F) and Gurmukhi (U+0A00–U+0A7F) are in the weight-1 range, but per code point:
  a conjunct such as "क्षि" is four code points and only one or two grapheme clusters (depending on the
  Unicode version's conjunct rule). **The spec's plan to count with
  `unicode-segmentation` (graphemes) undercounts Hindi and Punjabi and will produce posts X rejects.** A Rust
  port exists: `twitter-text` 0.3.1 ([crates.io](https://crates.io/crates/twitter-text)); its fidelity to
  the reference conformance tests is **UNVERIFIED**.
- **Threads** counts emoji by their UTF-8 byte length against a 500 limit
  ([Meta](https://developers.facebook.com/documentation/threads/posts)).
- **LinkedIn**: the spec's "~3,000" is **UNVERIFIED** in the API docs found. The `/rest/posts` `commentary` field
  uses "little text" format, where reserved characters must be escaped or the request fails or the text is
  mangled ([Microsoft Learn](https://learn.microsoft.com/en-us/linkedin/marketing/community-management/shares/little-text-format)).
- **Instagram**: caption 2,200 characters, 30 hashtags, 20 @-tags, per third-party reading of Meta's reference
  ([adaptlypost](https://adaptlypost.com/blog/instagram-api-caption-2200-characters-20-tags); **UNVERIFIED** on Meta).

Each validator is a pure function with the platform's own algorithm and conformance fixtures, property-tested.

## 5. GEO (generative engine optimisation): what is actually known

- **Academic evidence.** Aggarwal et al., "GEO: Generative Engine Optimization", KDD 2024
  ([arXiv 2311.09735](https://arxiv.org/abs/2311.09735)). On a simulated engine (GPT-3.5-turbo over the top five
  Google results, GEO-bench, 1,000 test queries), adding quotations raised a position-adjusted word-count
  visibility metric about 41%, statistics about 33%, citing sources about 28%; keyword stuffing did not help. A
  200-query test on Perplexity.ai showed about 22% for quotations
  ([paper HTML](https://arxiv.org/html/2311.09735v3)). This is the basis for "quotable stats and answer blocks";
  it measures share of an answer on a 2023-era setup, not traffic or citations in today's engines.
- **Google AI Overviews / AI Mode.** Google: "There are no additional requirements to appear in AI Overviews
  or AI Mode", "no special schema.org structured data that you need to add", and no need for "new machine
  readable files, AI text files, or markup" ([Google Search Central](https://developers.google.com/search/docs/appearance/ai-features)).
- **FAQ rich results are gone.** Google stopped showing FAQ rich results on 2026-05-07; the markup itself stays
  valid ([Search Engine Journal](https://www.searchenginejournal.com/google-drops-faq-rich-results/574429/),
  [Search Engine Roundtable](https://www.seroundtable.com/google-drops-faq-rich-results-41298.html)). The spec's
  "FAQ schema JSON-LD" stretch goal can no longer promise a Google result.
- **llms.txt** is a community proposal, not a standard. Ahrefs found 97% of llms.txt files across 137K sites
  received zero requests in May 2026 ([Ahrefs](https://ahrefs.com/blog/llmstxt-study/)); OpenAI's crawler docs
  do not mention it (secondary report, [PPC Land](https://ppc.land/llms-txt-adoption-rises-8-8x-but-97-of-files-get-zero-ai-requests/)).
- **"Entity signals"**: no primary source found that defines or measures them for AI engines. **UNVERIFIED.**

Honest positioning for a GEO pack: it produces answer-first summaries, quotable statistics with their source
spans, and valid markup, and it measures whether those pieces get cited; it does not promise visibility.

## 6. Ingest: articles, YouTube, audio

- `dom_smoothie` 0.19.0 (MIT), a Readability port ([crates.io](https://crates.io/crates/dom_smoothie)).
- **URL ingest is an SSRF surface**: user-supplied URLs fetched by the worker must be refused for private,
  loopback and metadata addresses, with DNS answers and every redirect re-validated
  ([OWASP SSRF cheat sheet](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html)).
  Service Monitor's guard is the in-house reference pattern.
- **YouTube captions**: `captions.download` costs 200 quota units, needs `youtube.force-ssl` (or
  `youtubepartner`) and requires "permission to edit the video"
  ([YouTube Data API](https://developers.google.com/youtube/v3/docs/captions/download)); the default project quota
  is 10,000 units per day and `captions.list` costs 50
  ([quota calculator](https://developers.google.com/youtube/v3/determine_quota_cost)), so about 40 list-and-download
  pairs per day before a quota audit. This confirms the spec: only the user's own
  videos, through OAuth.
- **Hosted transcription** (for audio/video uploads): Deepgram Nova-3 pre-recorded $0.0043/min monolingual,
  $0.0052/min multilingual, diarisation included ([Deepgram](https://deepgram.com/pricing)); AssemblyAI-STT-3.7
  $0.21/h ([AssemblyAI](https://www.assemblyai.com/pricing.md)). A 60-minute video at the Deepgram rate is about
  $0.26, which alone exceeds the ~$0.03 per-source target above. Audio sources need their own credit price.
- **FFmpeg** is LGPL-2.1+, becoming GPL if built with GPL parts; distributing a container image that contains it
  carries source-offer duties ([FFmpeg legal](https://ffmpeg.org/legal.html)).

## 7. Crates the spec names (current versions)

From crates.io on 2026-11-20: `tokio` 1.54.1, `sqlx` 0.9.1, `reqwest` 0.13.5, `aho-corasick` 1.1.5,
`unicode-segmentation` 1.13.3, `tower_governor` 0.8.0, `tower-sessions` 0.15.0, `oauth2` 5.0.0,
`apalis` 1.2.0 / `apalis-postgres` 1.1.1. **`async-stripe`**: the latest stable is 0.41.0 while 1.0.0 is at
rc.10; choose one deliberately ([crates.io](https://crates.io/crates/async-stripe)).

## 8. Payments

- **Stripe is invite-only for businesses in India since May 2024**
  ([Stripe support](https://support.stripe.com/questions/stripe-accounts-are-invite-only-in-india)); from
  2026-01-01 RBI rules require video KYC for new Stripe India onboarding
  ([Stripe support](https://support.stripe.com/questions/video-kyc-for-india-onboarding)). "async-stripe
  (global)" assumes an account the operator may not be able to open.
- **Razorpay** webhooks: HMAC-SHA256 over the **raw body** with the webhook secret (not the API key), compared
  with `X-Razorpay-Signature`; `x-razorpay-event-id` identifies duplicates; test and live secrets differ
  ([Razorpay](https://razorpay.com/docs/webhooks/validate-test/)).
- The workspace currently bypasses payments for its products; see END_GOAL open questions.

## 9. Legal and policy constraints on a public demo (not legal advice)

- **Copyright and platform terms**: process only content the user owns or has rights to (spec); no
  third-party transcript scraping (YouTube API rule above).
- **EU AI Act Article 50** applies from 2026-08-02. 50(4): deployers publishing AI-generated text to inform the
  public on matters of public interest must disclose it, unless it had human review or editorial control and
  someone holds editorial responsibility. 50(2): providers of systems generating synthetic text must mark
  outputs machine-readably, except assistive editing that does not substantially alter the input
  ([Article 50](https://artificialintelligenceact.eu/article/50/)); the Digital Omnibus moved the 50(2) date to
  2026-12-02 per several firms, with scope disputed
  ([FPF](https://fpf.org/?p=250371), [Goodwin](https://www.goodwinlaw.com/en/insights/publications/2026/08/alerts-technology-dpc-eu-ai-act-transparency-obligations-now-in-force)).
  Whether a repurposing SaaS is a "provider" under 50(2) is a question for a lawyer. A public demo that keeps a
  human-approval step and labels drafts as AI-assisted stays on the conservative side.
- **Prompt injection**: a pasted article can contain instructions. The source is data; the generator has no
  tools and no write access, and its output is a draft.
