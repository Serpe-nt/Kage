-- Job items and the crawl frontier (spec §13, §16.6).

CREATE TABLE job_items (
    id           uuid PRIMARY KEY,
    job_id       uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    -- Position in the job; results are paginated in this order.
    seq          bigint NOT NULL CHECK (seq >= 0),
    state        text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    input        jsonb NOT NULL,
    request_id   uuid REFERENCES requests (id) ON DELETE SET NULL,
    error        text,
    created_at   timestamptz NOT NULL,
    started_at   timestamptz,
    finished_at  timestamptz,
    UNIQUE (job_id, seq)
);

-- Workers claim queued items in order (SELECT ... FOR UPDATE SKIP LOCKED).
CREATE INDEX job_items_queued_idx ON job_items (job_id, seq) WHERE state = 'queued';

CREATE TABLE crawl_frontier (
    id               uuid PRIMARY KEY,
    job_id           uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    -- The URL exactly as discovered: this is what gets requested.
    url              text NOT NULL,
    -- Its normalized form (spec §13): one entry per key per crawl.
    url_key          text NOT NULL,
    depth            integer NOT NULL CHECK (depth >= 0),
    state            text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'fetching', 'done', 'failed', 'skipped')),
    discovered_from  text,
    content_hash     text,
    -- 64-bit SimHash, stored as its two's-complement bit pattern.
    simhash          bigint,
    near_duplicate   boolean NOT NULL DEFAULT false,
    created_at       timestamptz NOT NULL,
    fetched_at       timestamptz,
    UNIQUE (job_id, url_key)
);

CREATE INDEX crawl_frontier_queued_idx ON crawl_frontier (job_id, depth, created_at)
    WHERE state = 'queued';
CREATE INDEX crawl_frontier_content_idx ON crawl_frontier (job_id, content_hash)
    WHERE content_hash IS NOT NULL;
