-- The robots.txt answers 2.0.1 kept empty (F99, release 2.0.2).
--
-- Until F99 the scout's answer took its body from a browser's page only.
-- A file the plain client of level 0 asked for first had no page, so it
-- was kept as a file that says nothing: a `2xx` status, an empty body, not
-- unreachable. Under `robots: enforce` that let every page of the origin
-- through, at every level, until the row was due again (about a day).
--
-- This deletes exactly that shape, once, on the upgrade. An origin without
-- a row is asked for again by its site's scout when a request needs it,
-- and 2.0.2 keeps the file as the site sent it. Every other row stays: a
-- real file (a body that is not empty), "no file" (a `4xx`, no body), and
-- "could not be had" (`unreachable`). A site that does serve an empty
-- `robots.txt` with a `2xx` is asked once more and kept as it was.
--
-- Idempotent: a second run finds nothing of that shape left to delete.

DELETE FROM robots_cache
WHERE body = ''
  AND status BETWEEN 200 AND 299
  AND NOT unreachable;
