-- Camouflage levels and egress (spec §25, ADR 0012, M14).

-- Every request names its level, 0 to 3; the requests of 1.x have none.
ALTER TABLE requests ADD COLUMN level smallint CHECK (level BETWEEN 0 AND 3);

-- An attempt at level 0 is a plain client's: it has no persona and no
-- engine, and says instead which exit it left from (`host` or `pool`,
-- never an address). Every other attempt is a persona's.
ALTER TABLE attempts ALTER COLUMN persona_id DROP NOT NULL;
ALTER TABLE attempts ALTER COLUMN engine DROP NOT NULL;
ALTER TABLE attempts ADD COLUMN exit text CHECK (exit IN ('host', 'pool'));
ALTER TABLE attempts ADD CONSTRAINT attempts_persona_or_exit CHECK (
    (persona_id IS NOT NULL AND engine IS NOT NULL AND exit IS NULL)
    OR (persona_id IS NULL AND engine IS NULL AND exit IS NOT NULL)
);

-- Under `egress.mode: direct` the host's own connection stands in the pool
-- as one row per host persona, so that a persona keeps its one proxy: never
-- synced, probed through, replaced or counted as a leak.
ALTER TABLE proxies DROP CONSTRAINT proxies_provider_check;
ALTER TABLE proxies ADD CONSTRAINT proxies_provider_check
    CHECK (provider IN ('webshare', 'static', 'host'));

-- The bytes of a plain client belong to no persona, and the bytes of the
-- host's own connection are not the provider's: the budget of the proxy
-- plan counts `pool` only.
ALTER TABLE bandwidth_usage ADD COLUMN egress text NOT NULL DEFAULT 'pool'
    CHECK (egress IN ('pool', 'host'));
ALTER TABLE bandwidth_usage DROP CONSTRAINT bandwidth_usage_pkey;
ALTER TABLE bandwidth_usage ALTER COLUMN persona_id DROP NOT NULL;
ALTER TABLE bandwidth_usage ADD CONSTRAINT bandwidth_usage_hour_persona_domain_egress
    UNIQUE NULLS NOT DISTINCT (hour, persona_id, domain, egress);
