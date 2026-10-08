-- Proxy pool state (spec §7.3) and encrypted proxy user names (spec §19).

-- The user name is half of the credential, and the one value that ties
-- every proxy to one provider account, so it is stored as ciphertext like
-- the password. Nothing wrote proxy rows before this migration (the pool
-- arrives with it), so there is no plain text to convert. Should a row
-- exist anyway, adding the NOT NULL column fails and the migration stops
-- here on purpose, instead of leaving a user name in clear text: delete
-- the hand-made rows and migrate again.
ALTER TABLE proxies DROP COLUMN username;
ALTER TABLE proxies ADD COLUMN username bytea NOT NULL;

ALTER TABLE proxies
    -- Backoff cycles in a row without a passing check in between (the k of
    -- the backoff formula, and the count compared with M).
    ADD COLUMN backoff_count integer NOT NULL DEFAULT 0 CHECK (backoff_count >= 0),
    -- When the current run of failed checks began.
    ADD COLUMN failing_since timestamptz,
    -- The replacement Kage requested for this proxy. The monthly budget
    -- counts the rows whose request time falls in the month; a request the
    -- provider failed is cleared again and so does not count.
    ADD COLUMN replacement_requested_at timestamptz,
    ADD COLUMN replacement_reason text
        CHECK (replacement_reason IN ('unreachable', 'reputation', 'operator')),
    -- The provider's handle for the request, once it accepted it.
    ADD COLUMN replacement_ticket text,
    ADD CONSTRAINT proxies_replacement_complete
        CHECK ((replacement_requested_at IS NULL) = (replacement_reason IS NULL)),
    ADD CONSTRAINT proxies_replacement_ticket
        CHECK (replacement_ticket IS NULL OR replacement_requested_at IS NOT NULL);

-- A provider may hand the id of a proxy, or its address, to a later proxy.
-- Only one live row may hold a provider's id; retired rows keep theirs as
-- history.
ALTER TABLE proxies DROP CONSTRAINT proxies_provider_provider_ref_key;
CREATE UNIQUE INDEX proxies_provider_ref_live_idx
    ON proxies (provider, provider_ref) WHERE state <> 'retired';

CREATE INDEX proxies_replacement_requested_idx
    ON proxies (replacement_requested_at) WHERE replacement_requested_at IS NOT NULL;
