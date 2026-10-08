-- Requests, their attempts, their results and stored blobs (spec §5, §18).

CREATE TABLE requests (
    id           uuid PRIMARY KEY,
    -- NULL for Kage's own traffic (health checks, warm-up).
    api_key_id   uuid REFERENCES api_keys (id) ON DELETE SET NULL,
    job_id       uuid REFERENCES jobs (id) ON DELETE CASCADE,
    session_id   text,
    url          text NOT NULL,
    method       text NOT NULL,
    domain       text NOT NULL,
    render       text NOT NULL CHECK (render IN ('auto', 'http', 'browser')),
    spec         jsonb NOT NULL,
    created_at   timestamptz NOT NULL,
    finished_at  timestamptz,
    outcome      text CHECK (outcome IN (
        'ok', 'challenge_passed', 'challenge_failed', 'captcha', 'rate_limited', 'forbidden',
        'soft_block', 'provider_error', 'timeout', 'ssrf_denied', 'robots_denied'))
);

CREATE INDEX requests_domain_created_idx ON requests (domain, created_at);
CREATE INDEX requests_created_idx ON requests (created_at);
CREATE INDEX requests_job_idx ON requests (job_id) WHERE job_id IS NOT NULL;

CREATE TABLE attempts (
    id           uuid PRIMARY KEY,
    request_id   uuid NOT NULL REFERENCES requests (id) ON DELETE CASCADE,
    n            integer NOT NULL CHECK (n >= 1),
    persona_id   uuid NOT NULL REFERENCES personas (id),
    engine       text NOT NULL CHECK (engine IN ('chromium', 'firefox')),
    render       text NOT NULL CHECK (render IN ('http', 'browser')),
    started_at   timestamptz NOT NULL,
    finished_at  timestamptz NOT NULL,
    status       integer CHECK (status BETWEEN 100 AND 599),
    final_url    text,
    outcome      text NOT NULL CHECK (outcome IN (
        'ok', 'challenge_passed', 'challenge_failed', 'captcha', 'rate_limited', 'forbidden',
        'soft_block', 'provider_error', 'timeout', 'ssrf_denied', 'robots_denied')),
    block        jsonb,
    timing       jsonb NOT NULL,
    UNIQUE (request_id, n)
);

CREATE INDEX attempts_persona_started_idx ON attempts (persona_id, started_at);

CREATE TABLE results (
    id          uuid PRIMARY KEY,
    request_id  uuid NOT NULL UNIQUE REFERENCES requests (id) ON DELETE CASCADE,
    outcome     text NOT NULL CHECK (outcome IN (
        'ok', 'challenge_passed', 'challenge_failed', 'captcha', 'rate_limited', 'forbidden',
        'soft_block', 'provider_error', 'timeout', 'ssrf_denied', 'robots_denied')),
    status      integer CHECK (status BETWEEN 100 AND 599),
    headers     jsonb NOT NULL,
    final_url   text,
    redirects   text[] NOT NULL,
    extract     jsonb,
    xhr         jsonb,
    -- Pointers into the artifacts table; bodies are never inlined here.
    artifacts   jsonb NOT NULL,
    persona     jsonb,
    block       jsonb,
    robots      jsonb,
    cached      boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL
);

CREATE INDEX results_created_idx ON results (created_at);

-- One row per stored blob reference. The bytes live on disk under the
-- artifacts directory, keyed by (content_hash, encoding); several rows may
-- share one blob, which is deleted when its last row expires.
CREATE TABLE artifacts (
    id            uuid PRIMARY KEY,
    -- SHA-256 of the content before compression.
    content_hash  text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    kind          text NOT NULL
        CHECK (kind IN ('raw_body', 'dom_html', 'screenshot', 'pdf', 'xhr_body')),
    content_type  text NOT NULL,
    size_bytes    bigint NOT NULL CHECK (size_bytes >= 0),
    stored_bytes  bigint NOT NULL CHECK (stored_bytes >= 0),
    encoding      text NOT NULL CHECK (encoding IN ('identity', 'gzip')),
    request_id    uuid REFERENCES requests (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL,
    expires_at    timestamptz NOT NULL
);

CREATE INDEX artifacts_blob_idx ON artifacts (content_hash, encoding);
CREATE INDEX artifacts_expires_idx ON artifacts (expires_at);
