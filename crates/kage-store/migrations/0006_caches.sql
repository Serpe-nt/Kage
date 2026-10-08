-- robots.txt cache and the opt-in response cache (spec §14, §15).

CREATE TABLE robots_cache (
    -- scheme://host[:port]
    origin      text PRIMARY KEY,
    fetched_at  timestamptz NOT NULL,
    expires_at  timestamptz NOT NULL,
    -- NULL when the fetch failed without a response.
    status      integer CHECK (status BETWEEN 100 AND 599),
    body        text
);

CREATE INDEX robots_cache_expires_idx ON robots_cache (expires_at);

CREATE TABLE response_cache (
    -- Hash of normalized URL, method, body hash, render, include set and
    -- persona country (spec §15).
    cache_key   text PRIMARY KEY,
    url_key     text NOT NULL,
    result_id   uuid NOT NULL REFERENCES results (id) ON DELETE CASCADE,
    cached_at   timestamptz NOT NULL,
    expires_at  timestamptz NOT NULL
);

CREATE INDEX response_cache_expires_idx ON response_cache (expires_at);
