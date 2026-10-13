-- Extraction, robots.txt and the response cache as they are served (spec
-- §14, §15, milestone M11).

ALTER TABLE results
    -- When the page of a result that was served from the cache was fetched:
    -- the time of the visit that filled the entry. Null for a result that
    -- was fetched for its own request.
    ADD COLUMN cached_at timestamptz,
    ADD CONSTRAINT results_cached_at_check CHECK (cached = (cached_at IS NOT NULL));

ALTER TABLE robots_cache
    -- The site the origin belongs to: its registrable domain.
    ADD COLUMN site        text NOT NULL DEFAULT '',
    -- Whether the file could not be had (a server error, a timeout, a
    -- block). Nothing is known of the origin then, and where its policy
    -- enforces robots.txt nothing of it is fetched until `expires_at`.
    ADD COLUMN unreachable boolean NOT NULL DEFAULT false;

-- An entry is never deleted for its age: one that has expired says that
-- the file is to be asked for again, by the site's own scout.
DROP INDEX robots_cache_expires_idx;

-- The persona that asks for what no reader asks for (a site's robots.txt,
-- its sitemaps): one per site, so that one address is marked and no other.
-- It never reads the site. The row goes with the persona.
CREATE TABLE site_scouts (
    -- The site: a registrable domain.
    site        text PRIMARY KEY,
    persona_id  uuid NOT NULL REFERENCES personas (id) ON DELETE CASCADE,
    -- When the persona first asked.
    since       timestamptz NOT NULL
);

CREATE INDEX site_scouts_persona_idx ON site_scouts (persona_id);

ALTER TABLE response_cache
    -- The key an entry serves; null for a site whose policy shares its
    -- pages between keys. The key is part of `cache_key` too: this column
    -- is for whoever looks at the table.
    ADD COLUMN api_key_id uuid REFERENCES api_keys (id) ON DELETE CASCADE;
