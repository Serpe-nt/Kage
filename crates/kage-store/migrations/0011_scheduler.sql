-- The scheduler (spec §8, §11, decision D5): the half-open state of a
-- domain's circuit breaker, what an attempt was drawn from and why a
-- request ended without the site's verdict, and the bytes the forwarder
-- counted.

-- How often the breaker opened since the domain last answered well (each
-- time doubles the next pause), and when the one trial request after a
-- pause left.
ALTER TABLE domain_breakers
    ADD COLUMN trips             integer NOT NULL DEFAULT 0 CHECK (trips >= 0),
    ADD COLUMN trial_started_at  timestamptz;

ALTER TABLE attempts
    -- The seed of the attempt: with the persona's own seed it replays the
    -- behavior of a browser attempt and the pauses of an http attempt.
    ADD COLUMN seed    text CHECK (seed ~ '^[0-9a-f]{64}$'),
    -- Why the attempt ended as it did when the site did not say: for
    -- example proxy_failed, navigation_failed, sidecar_unreachable.
    ADD COLUMN reason  text;

-- Why a request ended without the site's verdict, or without any attempt:
-- for example domain_paused, egress_unsafe, bandwidth_exhausted, no_persona.
ALTER TABLE results ADD COLUMN reason text;

-- Bytes through the forwarder, per hour, persona and domain: both
-- directions of everything a persona's browser or http client exchanged
-- with its proxy. `domain` is the host being fetched while the bytes
-- passed, or '(idle)' outside an attempt (a browser's own traffic).
CREATE TABLE bandwidth_usage (
    hour        timestamptz NOT NULL,
    persona_id  uuid NOT NULL REFERENCES personas (id) ON DELETE CASCADE,
    domain      text NOT NULL,
    bytes_up    bigint NOT NULL DEFAULT 0 CHECK (bytes_up >= 0),
    bytes_down  bigint NOT NULL DEFAULT 0 CHECK (bytes_down >= 0),
    PRIMARY KEY (hour, persona_id, domain),
    CHECK (date_trunc('hour', hour AT TIME ZONE 'UTC') = hour AT TIME ZONE 'UTC')
);

CREATE INDEX bandwidth_usage_persona_idx ON bandwidth_usage (persona_id, hour);
