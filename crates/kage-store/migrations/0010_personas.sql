-- Personas as the persona manager makes them (spec §5, §6), the 24-hour
-- block window of the per-domain state, and the persona that owns a cookie
-- jar of the http path (spec §6.4).

-- Nothing wrote persona rows before this migration: personas arrive with
-- it. Should a row exist anyway, adding the columns without a default
-- fails and the migration stops here on purpose, instead of inventing a
-- seed for a persona that was generated from none: delete the hand-made
-- rows and migrate again.
ALTER TABLE personas
    -- The seed everything about the persona was drawn from; it never
    -- changes.
    ADD COLUMN seed                  text NOT NULL CHECK (seed ~ '^[0-9a-f]{64}$'),
    -- The User-Agent the persona's browser sends.
    ADD COLUMN user_agent            text NOT NULL CHECK (user_agent <> ''),
    ADD COLUMN hardware_concurrency  integer NOT NULL CHECK (hardware_concurrency > 0),
    -- The country of the persona's address: observed, or the provider's
    -- while no geo lookup has succeeded.
    ADD COLUMN country               text CHECK (country ~ '^[A-Z]{2}$'),
    -- Version of the tables the persona was drawn from.
    ADD COLUMN tables_version        text NOT NULL,
    ADD COLUMN cooling_reason        text
        CHECK (cooling_reason IN ('proxy_backoff', 'selftest_failed', 'geo_changed')),
    -- Warm-up visits made or skipped; warmed_at is set when the persona is
    -- ready for real work.
    ADD COLUMN warmup_visits         integer NOT NULL DEFAULT 0 CHECK (warmup_visits >= 0),
    ADD COLUMN warmed_at             timestamptz,
    ADD COLUMN retire_reason         text,
    -- The profile directory of a retired persona is archived, then deleted
    -- after its retention. profile_archive is NULL after profile_archived_at
    -- when there was no directory within reach to archive.
    ADD COLUMN profile_archived_at   timestamptz,
    ADD COLUMN profile_archive       text,
    ADD COLUMN profile_deleted_at    timestamptz,
    ADD CONSTRAINT personas_cooling_has_a_reason
        CHECK ((state = 'cooling') = (cooling_reason IS NOT NULL)),
    ADD CONSTRAINT personas_retired_has_a_time
        CHECK ((state = 'retired') = (retired_at IS NOT NULL)),
    ADD CONSTRAINT personas_archive_order
        CHECK (profile_archived_at IS NULL OR retired_at IS NOT NULL),
    ADD CONSTRAINT personas_archive_is_dated
        CHECK (profile_archive IS NULL OR profile_archived_at IS NOT NULL);

-- One persona per proxy among those that are not retired: a proxy's
-- persona can be retired and another generated behind the same address
-- (`kage persona regenerate`), and retired rows stay as history.
ALTER TABLE personas DROP CONSTRAINT personas_proxy_id_key;
CREATE UNIQUE INDEX personas_proxy_live_idx ON personas (proxy_id) WHERE state <> 'retired';
CREATE INDEX personas_proxy_idx ON personas (proxy_id);
-- What the retention job looks for.
CREATE INDEX personas_archive_due_idx ON personas (retired_at)
    WHERE state = 'retired' AND profile_archived_at IS NULL;
CREATE INDEX personas_delete_due_idx ON personas (profile_archived_at)
    WHERE profile_archived_at IS NOT NULL AND profile_deleted_at IS NULL;

-- When the window that block_count_24h counts began.
ALTER TABLE persona_domain_state ADD COLUMN block_window_started_at timestamptz;

-- The persona a cookie jar belongs to. The key stays the proxy: one live
-- persona per proxy. A jar is only ever loaded for the persona it was
-- written for, so a persona generated behind the same address never sees
-- its predecessor's cookies even if a row were left behind; retiring a
-- persona deletes its rows outright. NULL: written without a persona
-- (`kage selftest http --proxy`, which keeps no cookies, and rows from
-- before this migration).
ALTER TABLE http_cookies ADD COLUMN persona_id uuid REFERENCES personas (id) ON DELETE CASCADE;
CREATE INDEX http_cookies_persona_idx ON http_cookies (persona_id) WHERE persona_id IS NOT NULL;

CREATE INDEX selftest_runs_started_idx ON selftest_runs (started_at);
