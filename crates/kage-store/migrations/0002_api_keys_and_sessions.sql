-- API keys and sessions (spec §5, §16.2).

CREATE TABLE api_keys (
    id                   uuid PRIMARY KEY,
    name                 text NOT NULL,
    -- SHA-256 of the secret. The secret itself is shown once and never stored.
    secret_hash          bytea NOT NULL CHECK (octet_length(secret_hash) = 32),
    scopes               text[] NOT NULL
        CHECK (scopes <@ ARRAY['fetch', 'jobs', 'crawl', 'sessions', 'admin']::text[]),
    requests_per_minute  integer NOT NULL CHECK (requests_per_minute > 0),
    max_concurrent       integer NOT NULL CHECK (max_concurrent > 0),
    -- NULL means no daily budget.
    daily_page_budget    integer CHECK (daily_page_budget > 0),
    disabled_at          timestamptz,
    created_at           timestamptz NOT NULL,
    last_used_at         timestamptz
);

CREATE TABLE sessions (
    api_key_id    uuid NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE,
    -- Supplied by the caller or minted by the server; unique per key.
    id            text NOT NULL CHECK (id ~ '^[A-Za-z0-9_.-]{1,64}$'),
    persona_id    uuid NOT NULL REFERENCES personas (id),
    created_at    timestamptz NOT NULL,
    last_used_at  timestamptz NOT NULL,
    expires_at    timestamptz NOT NULL,
    PRIMARY KEY (api_key_id, id)
);

CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);
CREATE INDEX sessions_persona_idx ON sessions (persona_id);
