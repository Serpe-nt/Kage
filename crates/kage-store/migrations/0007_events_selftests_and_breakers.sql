-- Audit trail, health events, self-test runs and circuit breakers
-- (spec §7.4, §8.3, §19).

CREATE TABLE audit_events (
    id          uuid PRIMARY KEY,
    at          timestamptz NOT NULL,
    actor       text NOT NULL CHECK (actor IN ('api_key', 'cli', 'system')),
    -- Set to NULL when the key is deleted: the trail outlives its actors.
    api_key_id  uuid REFERENCES api_keys (id) ON DELETE SET NULL,
    action      text NOT NULL,
    target      text,
    outcome     text NOT NULL CHECK (outcome IN ('success', 'failure', 'denied')),
    details     jsonb NOT NULL,
    CHECK (actor = 'api_key' OR api_key_id IS NULL)
);

CREATE INDEX audit_events_at_idx ON audit_events (at);

-- Health of proxies, personas, policies and the system. Operator alerts are
-- stored here too.
CREATE TABLE health_events (
    id            uuid PRIMARY KEY,
    at            timestamptz NOT NULL,
    subject_kind  text NOT NULL
        CHECK (subject_kind IN ('proxy', 'persona', 'policy', 'sidecar', 'system')),
    subject_id    uuid,
    kind          text NOT NULL,
    severity      text NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    details       jsonb NOT NULL
);

CREATE INDEX health_events_at_idx ON health_events (at);
CREATE INDEX health_events_subject_idx ON health_events (subject_kind, subject_id, at);

CREATE TABLE selftest_runs (
    id           uuid PRIMARY KEY,
    persona_id   uuid NOT NULL REFERENCES personas (id) ON DELETE CASCADE,
    engine       text NOT NULL CHECK (engine IN ('chromium', 'firefox')),
    render       text NOT NULL CHECK (render IN ('http', 'browser')),
    started_at   timestamptz NOT NULL,
    finished_at  timestamptz NOT NULL,
    passed       boolean NOT NULL,
    score        real,
    findings     jsonb NOT NULL
);

CREATE INDEX selftest_runs_persona_idx ON selftest_runs (persona_id, render, started_at DESC);

CREATE TABLE domain_breakers (
    domain             text PRIMARY KEY,
    state              text NOT NULL DEFAULT 'closed' CHECK (state IN ('closed', 'open')),
    block_count_1h     integer NOT NULL DEFAULT 0 CHECK (block_count_1h >= 0),
    window_started_at  timestamptz,
    paused_until       timestamptz,
    updated_at         timestamptz NOT NULL
);
