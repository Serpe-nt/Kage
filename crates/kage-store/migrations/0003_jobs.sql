-- Jobs (spec §16.6). Items and the crawl frontier follow in 0005, after the
-- requests they point at.

CREATE TABLE jobs (
    id                uuid PRIMARY KEY,
    api_key_id        uuid NOT NULL REFERENCES api_keys (id),
    kind              text NOT NULL CHECK (kind IN ('batch', 'flow', 'crawl')),
    state             text NOT NULL DEFAULT 'queued' CHECK (state IN (
        'queued', 'running', 'paused', 'succeeded', 'partial', 'failed', 'cancelled')),
    spec              jsonb NOT NULL,
    idempotency_key   text,
    webhook_url       text,
    -- Ciphertext of the webhook HMAC secret.
    webhook_secret    bytea,
    items_total       integer NOT NULL DEFAULT 0 CHECK (items_total >= 0),
    items_succeeded   integer NOT NULL DEFAULT 0 CHECK (items_succeeded >= 0),
    items_failed      integer NOT NULL DEFAULT 0 CHECK (items_failed >= 0),
    cancel_requested  boolean NOT NULL DEFAULT false,
    error             text,
    created_at        timestamptz NOT NULL,
    started_at        timestamptz,
    finished_at       timestamptz,
    -- NULL keys never collide, so jobs without a key are unconstrained.
    UNIQUE (api_key_id, idempotency_key)
);

CREATE INDEX jobs_state_created_idx ON jobs (state, created_at);
