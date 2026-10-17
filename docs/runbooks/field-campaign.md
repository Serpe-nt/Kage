# The field campaign (M13)

How the owner measures and tunes Kage against real protected sites, and
writes down what was found. It is run by hand, by the owner, on sites the
owner chose and may fetch; never in CI, never from a test, never by an
agent. Everything here is ordinary use of Kage: there is no campaign mode.

## Before a session

- **The sites.** Only sites you may fetch: their terms allow it, their
  `robots.txt` is honoured where it should be (a policy's `robots:
  enforce`), and nothing behind a login. Kage signs in nowhere and solves
  no CAPTCHA; a site that asks for either is a site Kage does not read.
- **The pool.** `kage proxy list` shows every proxy `healthy`, `kage
  persona list` a persona on each, and `kage persona selftest <id>` passes
  for one persona of each engine. Retire what fails before you start
  ([personas.md](personas.md)).
- **The budget.** Every page costs the proxy plan's bandwidth; a session of
  a few hundred browser pages is a few hundred megabytes to a gigabyte.
  The **Kage · bandwidth** dashboard shows the share of the month used;
  stop well before 80 %, where KageBandwidthBudget fires.
- **The observability profile** ([observability.md](observability.md)):
  the campaign's numbers come from its dashboards and from the store.
- **A baseline.** Note the commit (`kage --version` and `git rev-parse
  HEAD`), and save `policies/` as it is: the report shows the difference.

## During a session

1. Give every site an explicit entry in `policies/domains.yaml` before its
   first request, with the shipped defaults written out: `render: auto`,
   `pacing: human`, `active_hours: enforce`, `retries: 2`,
   `breaker_threshold: 5`. A site without an entry gets the defaults, and
   the report cannot say what applied.
2. Ask little and slowly: a batch of 10 to 30 pages of one site at a time
   (`POST /v1/jobs`), never more than one batch per site at once. A crawl
   only of a site whose batches went well.
3. Watch **Kage · blocks and breakers**. A breaker that opens ends that
   site for the session: do not close it by hand to go on.
4. Change one thing at a time, and run the same batch again after each
   change: the render (`browser` instead of `auto`), the pacing (a slower
   profile), the engine mix of the site (`engines`), the active hours.
5. A block page that Kage did not recognise gets a signature and a
   sanitized fixture ([block-report.md](block-report.md), last section).
   Strip every cookie, token, address and personal detail from a fixture
   before it is committed.

## What to record

Per site and per change, from the store (the last session's requests):

```sql
-- Outcome rates per engine and render for one domain since a moment.
SELECT attempt.engine, attempt.render, attempt.outcome,
       count(*) AS attempts,
       round(100.0 * count(*) / sum(count(*)) OVER (PARTITION BY attempt.engine, attempt.render), 1) AS percent
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.domain = 'shop.example' AND attempt.started_at > '2026-11-02T09:00:00Z'
GROUP BY 1, 2, 3
ORDER BY 1, 2, 4 DESC;

-- Who blocked, by vendor and kind.
SELECT attempt.block ->> 'vendor' AS vendor, attempt.block ->> 'type' AS kind, count(*)
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.domain = 'shop.example' AND attempt.block IS NOT NULL
  AND attempt.started_at > '2026-11-02T09:00:00Z'
GROUP BY 1, 2;
```

and the pacing that applied (the result's `pacing`, or the policy's
profile), the bandwidth used (the dashboard), and every change made to
`policies/`, `policies/block-signatures.yaml` and `personas.mix`.

## The report

One file per session, `docs/field-reports/<date>.md`, committed with the
policy and signature changes it made, in a pull request of its own:

```markdown
# Field report <date>

Commit <sha>; pool of <n> proxies (<countries>), <n> personas (<engines>).

## <domain>

- Vendor detected: <vendor, kind> (or none)
- Before: <outcome rates per engine and render, from the query above>
- Change: <the exact diff of policies/domains.yaml for the site>
- After: <the same rates after the change>
- Pacing: <profile>, bandwidth: <MB>
- Decision: <keep, tune further, leave the site alone>
```

Every report holds the rates before and after and the exact configuration
diff (the plan's acceptance for M13). A site that keeps refusing a gentle
visitor is written down as such and left alone.
