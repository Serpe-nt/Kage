-- The cookie jar of the http path (spec §6.4, §9).
--
-- One row per cookie, keyed by the proxy the cookies were earned behind.
-- A persona lives behind exactly one proxy for the life of both, so until
-- personas exist (milestone M6) the proxy is the persona.
--
-- Cookies never survive their address: a new address that presents the old
-- cookies links the two for the site. The rows go when the proxy row goes,
-- the http path refuses to load the jar of a retired proxy, and retiring a
-- persona deletes them outright.
--
-- Values are stored in clear, like the browser profile directories that
-- hold the same cookies. Kage performs no logins in v1, so these are
-- consent, session and bot-management cookies, not credentials.

CREATE TABLE http_cookies (
    proxy_id      uuid NOT NULL REFERENCES proxies (id) ON DELETE CASCADE,
    -- The host that set the cookie, or its Domain attribute: lowercase, no
    -- leading dot.
    domain        text NOT NULL CHECK (domain <> '' AND domain = lower(domain)),
    -- No Domain attribute: sent to exactly this host, not to subdomains.
    host_only     boolean NOT NULL,
    path          text NOT NULL CHECK (path LIKE '/%'),
    name          text NOT NULL,
    value         text NOT NULL,
    secure        boolean NOT NULL,
    http_only     boolean NOT NULL,
    -- NULL when the cookie carried no SameSite attribute.
    same_site     text CHECK (same_site IN ('strict', 'lax', 'none')),
    -- NULL for a session cookie.
    expires_at    timestamptz,
    created_at    timestamptz NOT NULL,
    last_used_at  timestamptz NOT NULL,
    -- Position in creation order within the jar: cookies of equal path
    -- length are sent oldest first, and two can share a timestamp.
    ordinal       integer NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (proxy_id, domain, host_only, path, name)
);
