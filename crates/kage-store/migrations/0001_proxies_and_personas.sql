-- Proxies, personas and per-domain persona state (spec §5, §6, §7).
--
-- Conventions for every migration in this directory:
--   * ids are UUIDv7, minted by the application;
--   * timestamps are timestamptz in UTC, supplied by the application so
--     that tests can control the clock;
--   * enums are text with a CHECK constraint, so a new value is one ALTER
--     and not a type migration.

CREATE TABLE proxies (
    id                    uuid PRIMARY KEY,
    provider              text NOT NULL CHECK (provider IN ('webshare', 'static')),
    provider_ref          text NOT NULL,
    host                  text NOT NULL,
    port                  integer NOT NULL CHECK (port BETWEEN 1 AND 65535),
    username              text NOT NULL,
    -- Ciphertext (spec §19). The plaintext password is never stored.
    password              bytea NOT NULL,
    scheme                text NOT NULL DEFAULT 'http' CHECK (scheme IN ('http')),
    country               text CHECK (country ~ '^[A-Z]{2}$'),
    city                  text,
    asn                   text,
    state                 text NOT NULL DEFAULT 'new'
        CHECK (state IN ('new', 'healthy', 'degraded', 'backoff', 'replacing', 'retired')),
    consecutive_failures  integer NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    backoff_until         timestamptz,
    last_health           jsonb,
    created_at            timestamptz NOT NULL,
    retired_at            timestamptz,
    UNIQUE (provider, provider_ref)
);

CREATE INDEX proxies_state_idx ON proxies (state);

CREATE TABLE personas (
    id             uuid PRIMARY KEY,
    -- One persona per proxy, for the life of both.
    proxy_id       uuid NOT NULL UNIQUE REFERENCES proxies (id),
    engine         text NOT NULL CHECK (engine IN ('chromium', 'firefox')),
    claimed_os     text NOT NULL CHECK (claimed_os IN ('windows', 'macos', 'linux')),
    browser_major  integer NOT NULL CHECK (browser_major > 0),
    fingerprint    jsonb NOT NULL,
    -- NULL disables the http path for this persona.
    wreq_profile   text,
    locale         text NOT NULL,
    languages      text[] NOT NULL,
    timezone       text NOT NULL,
    geolocation    jsonb,
    screen         jsonb NOT NULL,
    profile_dir    text NOT NULL,
    state          text NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'cooling', 'retired')),
    created_at     timestamptz NOT NULL,
    retired_at     timestamptz
);

CREATE INDEX personas_state_idx ON personas (state);

CREATE TABLE persona_domain_state (
    persona_id       uuid NOT NULL REFERENCES personas (id) ON DELETE CASCADE,
    domain           text NOT NULL,
    cooldown_until   timestamptz,
    requests_today   integer NOT NULL DEFAULT 0 CHECK (requests_today >= 0),
    -- The calendar day, in the persona's time zone, that requests_today counts.
    counter_day      date,
    last_request_at  timestamptz,
    last_outcome     text CHECK (last_outcome IN (
        'ok', 'challenge_passed', 'challenge_failed', 'captcha', 'rate_limited', 'forbidden',
        'soft_block', 'provider_error', 'timeout', 'ssrf_denied', 'robots_denied')),
    block_count_24h  integer NOT NULL DEFAULT 0 CHECK (block_count_24h >= 0),
    PRIMARY KEY (persona_id, domain)
);

CREATE INDEX persona_domain_state_domain_idx ON persona_domain_state (domain);
