-- Crawls as they are worked (spec §13, milestone M10), and the patience of
-- a browser attempt (follow-up F44).

ALTER TABLE attempts
    -- How long the persona would have gone on scrolling the page at most,
    -- in milliseconds: drawn for the attempt from its seed, and kept so
    -- that the visit can be replayed whatever the configuration says by
    -- then. Null on the http path and for a flow.
    ADD COLUMN patience_ms bigint CHECK (patience_ms IS NULL OR patience_ms >= 0);

ALTER TABLE jobs
    -- What a crawl keeps about itself besides its frontier: its counters,
    -- its sitemaps, and the tab of each of its visitors. Null for every
    -- other job.
    ADD COLUMN crawl jsonb;

ALTER TABLE crawl_frontier
    -- Its position in the crawl: the order in which the crawl came to know
    -- its addresses. Entries name each other by it.
    ADD COLUMN seq                  bigint NOT NULL DEFAULT 0 CHECK (seq >= 0),
    -- The site it belongs to: its registrable domain.
    ADD COLUMN site                 text NOT NULL DEFAULT '',
    -- How the crawl came to know it.
    ADD COLUMN source               text NOT NULL DEFAULT 'link'
        CHECK (source IN ('seed', 'sitemap', 'link')),
    -- Why it is not read, when it is not.
    ADD COLUMN skip_reason          text,
    -- The page it was found on, and whether its link stood in what the
    -- site repeats on every page.
    ADD COLUMN found_on             bigint,
    ADD COLUMN furniture            boolean NOT NULL DEFAULT false,
    -- The entries the page links to, in the order a reader meets them,
    -- and those of its links that spell their target otherwise than the
    -- crawl first met it: pairs of a position and the address as written.
    ADD COLUMN links                jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN spellings            jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Where the page ended when a redirect led elsewhere, and whether that
    -- is off the crawl's sites.
    ADD COLUMN final_url            text,
    ADD COLUMN left_site            boolean NOT NULL DEFAULT false,
    -- The page it says the same as, word for word; the page it nearly
    -- says the same as, and in how many of 64 bits they differ.
    ADD COLUMN duplicate_of         bigint,
    ADD COLUMN near_duplicate_of    bigint,
    ADD COLUMN near_duplicate_bits  integer
        CHECK (near_duplicate_bits IS NULL OR near_duplicate_bits BETWEEN 0 AND 64),
    -- What a sitemap says about when the page changed.
    ADD COLUMN lastmod              timestamptz,
    -- The job item that carries its page.
    ADD COLUMN item_id              uuid;

-- No crawl ran before this migration; whatever a row says is put in order.
UPDATE crawl_frontier AS entry
SET seq = numbered.seq
FROM (
    SELECT id, row_number() OVER (PARTITION BY job_id ORDER BY depth, created_at, id) - 1 AS seq
    FROM crawl_frontier
) AS numbered
WHERE numbered.id = entry.id;
UPDATE crawl_frontier SET skip_reason = 'not_allowed' WHERE state = 'skipped';

ALTER TABLE crawl_frontier
    -- Said by `near_duplicate_of` from now on.
    DROP COLUMN near_duplicate,
    ADD CONSTRAINT crawl_frontier_job_seq_key UNIQUE (job_id, seq),
    ADD CONSTRAINT crawl_frontier_skip_reason_check
        CHECK ((state = 'skipped') = (skip_reason IS NOT NULL)),
    ADD CONSTRAINT crawl_frontier_near_duplicate_check
        CHECK ((near_duplicate_of IS NULL) = (near_duplicate_bits IS NULL));

-- The order of reading is the crawl's own (spec §13), not an order of
-- rows: the index by depth and age is of no use to it.
DROP INDEX crawl_frontier_queued_idx;
CREATE INDEX crawl_frontier_item_idx ON crawl_frontier (item_id) WHERE item_id IS NOT NULL;
