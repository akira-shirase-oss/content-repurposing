-- M1 ingest and analysis: a pasted URL becomes a source, a worker fetches, extracts and
-- analyses it, and its key ideas are stored with the exact byte spans they quote.

-- One pasted URL. `idempotency_key` makes a retried POST a no-op; the job columns are the
-- crash-safe queue: a worker claims a row by setting `lease_until`, and a lease that runs
-- out (the worker died) makes the row claimable again, up to `attempts` < 5.
CREATE TABLE sources (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    idempotency_key text NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    url text NOT NULL CHECK (length(url) BETWEEN 1 AND 2048),
    final_url text CHECK (length(final_url) <= 2048),
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'ready', 'failed')),
    failure text CHECK (failure ~ '^[a-z_]{1,64}$'),
    title text CHECK (length(title) <= 2048),
    body text CHECK (length(body) <= 1048576),
    truncated boolean NOT NULL DEFAULT false,
    refused_ideas integer NOT NULL DEFAULT 0 CHECK (refused_ideas >= 0),
    prompt_version text,
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    lease_until timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, idempotency_key),
    CHECK ((status = 'failed') = (failure IS NOT NULL)),
    CHECK (status <> 'ready' OR body IS NOT NULL)
);
CREATE INDEX sources_workspace_created ON sources (workspace_id, created_at DESC, id DESC);
CREATE INDEX sources_claimable ON sources (created_at, id) WHERE status IN ('pending', 'running');
-- The lease sweep reads only running rows: without this it walks every pending row per claim.
CREATE INDEX sources_leased ON sources (lease_until) WHERE status = 'running';

-- A key idea and the spans it quotes: `spans` is `[{"start","end","text"}]`, byte offsets
-- into `sources.body`, re-checked against the body on every read.
CREATE TABLE ideas (
    source_id uuid NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    ordinal integer NOT NULL CHECK (ordinal BETWEEN 0 AND 63),
    kind text NOT NULL CHECK (kind IN ('idea', 'quote', 'number')),
    text text NOT NULL CHECK (length(text) BETWEEN 1 AND 2000),
    spans jsonb NOT NULL CHECK (jsonb_typeof(spans) = 'array'),
    PRIMARY KEY (source_id, ordinal)
);

-- The spend ledger, in micro-dollars per UTC day: one row for the global cap
-- (workspace_id NULL is spelled as the nil UUID) and one per workspace. A call reserves
-- its estimate here before it starts and settles to its real cost after.
CREATE TABLE spend_days (
    workspace_id uuid NOT NULL,
    day date NOT NULL,
    reserved_micros bigint NOT NULL DEFAULT 0 CHECK (reserved_micros >= 0),
    spent_micros bigint NOT NULL DEFAULT 0 CHECK (spent_micros >= 0),
    PRIMARY KEY (workspace_id, day)
);

-- One row per model call: tokens and cost. Append-only.
CREATE TABLE usage_events (
    id uuid PRIMARY KEY,
    at timestamptz NOT NULL DEFAULT now(),
    workspace_id uuid NOT NULL,
    source_id uuid,
    model text NOT NULL CHECK (length(model) BETWEEN 1 AND 128),
    input_tokens bigint NOT NULL CHECK (input_tokens >= 0),
    output_tokens bigint NOT NULL CHECK (output_tokens >= 0),
    cost_micros bigint NOT NULL CHECK (cost_micros >= 0)
);
CREATE INDEX usage_events_workspace_at ON usage_events (workspace_id, at);

CREATE TRIGGER usage_events_append_only BEFORE UPDATE OR DELETE ON usage_events
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER usage_events_no_truncate BEFORE TRUNCATE ON usage_events
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
ALTER TABLE usage_events ENABLE ALWAYS TRIGGER usage_events_append_only;
ALTER TABLE usage_events ENABLE ALWAYS TRIGGER usage_events_no_truncate;

GRANT SELECT, INSERT, UPDATE, DELETE ON sources, ideas TO repurpose_app;
GRANT SELECT, INSERT, UPDATE ON spend_days TO repurpose_app;
-- The settled usage of an admitted call; an event without one was killed mid-call and
-- stands at its estimate.
CREATE TABLE usage_settlements (
    event_id uuid PRIMARY KEY REFERENCES usage_events (id),
    at timestamptz NOT NULL DEFAULT now(),
    input_tokens bigint NOT NULL CHECK (input_tokens >= 0),
    output_tokens bigint NOT NULL CHECK (output_tokens >= 0),
    cost_micros bigint NOT NULL CHECK (cost_micros >= 0)
);

CREATE TRIGGER usage_settlements_append_only BEFORE UPDATE OR DELETE ON usage_settlements
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER usage_settlements_no_truncate BEFORE TRUNCATE ON usage_settlements
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
ALTER TABLE usage_settlements ENABLE ALWAYS TRIGGER usage_settlements_append_only;
ALTER TABLE usage_settlements ENABLE ALWAYS TRIGGER usage_settlements_no_truncate;

GRANT SELECT, INSERT ON usage_events, usage_settlements TO repurpose_app;
