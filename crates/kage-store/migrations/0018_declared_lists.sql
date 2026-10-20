-- The provider-agnostic pool and declared lists (ADR 0013, M15).

-- What an operator declared about a proxy's exit besides its country: the
-- locale (BCP 47) and the IANA time zone its persona is made from, which a
-- geo lookup only verifies. Null for a provider that does not declare
-- (Webshare), and where nothing was declared.
ALTER TABLE proxies ADD COLUMN locale text;
ALTER TABLE proxies ADD COLUMN timezone text;

-- The one address a site's plain traffic (level 0) leaves from under the
-- pool (spec §25): chosen once per site, preferably a proxy that carries
-- no persona, and the site's from then on. A row whose proxy is retired
-- says "choose again"; one whose proxy is not usable says "wait".
CREATE TABLE site_exits (
    site       text PRIMARY KEY,
    proxy_id   uuid NOT NULL REFERENCES proxies (id),
    chosen_at  timestamptz NOT NULL
);

CREATE INDEX site_exits_proxy_idx ON site_exits (proxy_id);
