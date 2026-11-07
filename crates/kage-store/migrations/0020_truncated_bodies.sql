-- Whether the body of a response was cut at `max_body_bytes` (spec §9,
-- QA round one's extract-04, F122): the flag the capture always had, kept
-- with the attempt and with the result, so that a result read later says
-- what the answer said. False for every row stored before the column: those
-- bodies were not looked at.

ALTER TABLE attempts
    ADD COLUMN truncated boolean NOT NULL DEFAULT false;

ALTER TABLE results
    ADD COLUMN truncated boolean NOT NULL DEFAULT false;
