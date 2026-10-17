-- What a result keeps of the visit that got its page, besides the page
-- (spec §16.5, milestone M12, follow-up F36), and what a crawl took from
-- it until the crawl has recorded it (spec §13, follow-up F49).

ALTER TABLE results
    -- One document: `pacing` (the profile that applied, whether the
    -- request's own was ignored, how long the request waited), `behavior`
    -- (what the persona was asked to do on the page and what the browser
    -- says it did), `console` (the browser's log, when the request asked
    -- for it; bounded) and `side_effects` (events a capture fired in the
    -- page). A member is absent when there is nothing to say; null for a
    -- result stored before this column and for one that keeps none of it.
    ADD COLUMN visit jsonb,
    -- What a crawl took from the page of a result (where it ended, its
    -- links, its text), kept from the moment the result is stored until
    -- the crawl has recorded the page: a crawl that changes hands between
    -- the two still knows the page's links (follow-up F49). Null for
    -- every other result, and set to null once the page is recorded.
    ADD COLUMN reading jsonb;
