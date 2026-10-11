-- Jobs as they are worked, and the webhooks that report them (spec §16.6,
-- milestone M9).

ALTER TABLE jobs
    -- Items that never ran because the job was cancelled.
    ADD COLUMN items_cancelled    integer NOT NULL DEFAULT 0 CHECK (items_cancelled >= 0),
    -- A keyed hash of the request that came with the idempotency key, and
    -- until when the key names this job.
    ADD COLUMN idempotency_hash   bytea
        CHECK (idempotency_hash IS NULL OR octet_length(idempotency_hash) = 32),
    ADD COLUMN idempotency_until  timestamptz,
    ADD COLUMN origin             text NOT NULL DEFAULT 'api'
        CHECK (origin IN ('api', 'sync_overflow')),
    -- When the job stops waiting for what holds its items.
    ADD COLUMN deadline           timestamptz,
    -- While paused: when the waiting items may be tried again, and what
    -- they wait for.
    ADD COLUMN paused_until       timestamptz,
    ADD COLUMN paused_reason      text,
    -- The claim of the worker that runs the job. A job whose lease ran out
    -- is taken up again (SELECT ... FOR UPDATE SKIP LOCKED).
    ADD COLUMN lease_until        timestamptz,
    ADD COLUMN worker             uuid,
    -- The personas that visit each site for the job, so that a batch stays
    -- with its visitor across a pause and a restart. Never returned.
    ADD COLUMN visitors           jsonb NOT NULL DEFAULT '{}'::jsonb;

CREATE INDEX jobs_open_idx ON jobs (created_at, id)
    WHERE state IN ('queued', 'running', 'paused');
CREATE INDEX jobs_finished_idx ON jobs (finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX jobs_key_created_idx ON jobs (api_key_id, created_at);

ALTER TABLE job_items
    -- The site the item belongs to: a batch works one site's items in order.
    ADD COLUMN site        text,
    -- Not to be taken up before this moment.
    ADD COLUMN not_before  timestamptz,
    -- How often a worker took the item up.
    ADD COLUMN claims      integer NOT NULL DEFAULT 0 CHECK (claims >= 0),
    -- How the item's fetch ended, when it had one.
    ADD COLUMN outcome     text CHECK (outcome IS NULL OR outcome IN (
        'ok', 'challenge_passed', 'challenge_failed', 'captcha', 'rate_limited', 'forbidden',
        'soft_block', 'provider_error', 'timeout', 'ssrf_denied', 'robots_denied')),
    -- What the item reports besides its result (a flow step's action,
    -- duration and error).
    ADD COLUMN output      jsonb;

CREATE INDEX job_items_running_idx ON job_items (job_id) WHERE state = 'running';

-- An item names the request that carries it out before that request is
-- written: a worker that stops between the two finds the item again and
-- begins the request under the id it wrote down, instead of a second one.
-- The id is therefore no reference any more; a request that was deleted
-- (retention, a purge) leaves an item whose result is gone.
ALTER TABLE job_items DROP CONSTRAINT job_items_request_id_fkey;
CREATE INDEX job_items_request_idx ON job_items (request_id) WHERE request_id IS NOT NULL;

-- One row per event of a job on its way to the job's webhook. Pending
-- rows are claimed with SELECT ... FOR UPDATE SKIP LOCKED and survive a
-- restart.
CREATE TABLE webhook_deliveries (
    id               uuid PRIMARY KEY,
    job_id           uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    event            text NOT NULL
        CHECK (event IN ('job.completed', 'job.failed', 'job.progress')),
    -- scheme://host:port of the receiver: one delivery in flight per origin.
    origin           text NOT NULL,
    -- The body exactly as it is sent and signed.
    body             text NOT NULL,
    state            text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'delivered', 'failed', 'superseded')),
    attempts         integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at  timestamptz,
    -- Held by the sender while an attempt is under way.
    lease_until      timestamptz,
    created_at       timestamptz NOT NULL,
    finished_at      timestamptz,
    CHECK ((state = 'pending') = (next_attempt_at IS NOT NULL))
);

CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at, id)
    WHERE state = 'pending';
CREATE INDEX webhook_deliveries_job_idx ON webhook_deliveries (job_id, created_at);

-- The delivery log: one row per attempt.
CREATE TABLE webhook_attempts (
    delivery_id  uuid NOT NULL REFERENCES webhook_deliveries (id) ON DELETE CASCADE,
    n            integer NOT NULL CHECK (n >= 1),
    started_at   timestamptz NOT NULL,
    duration_ms  integer NOT NULL CHECK (duration_ms >= 0),
    status       integer CHECK (status BETWEEN 100 AND 599),
    -- Why the attempt failed when the receiver did not answer.
    error        text,
    PRIMARY KEY (delivery_id, n)
);

-- A purge by key and time (follow-up F33) and the retention of finished
-- jobs both look artifacts up by their request.
CREATE INDEX artifacts_request_idx ON artifacts (request_id) WHERE request_id IS NOT NULL;
