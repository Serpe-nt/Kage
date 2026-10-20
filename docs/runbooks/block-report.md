# Reading a block report

A site turned a persona away. This page says how to read what Kage wrote
down about it and what to do, from one result to a site that is paused.
What happens on its own after a block (the cooldown, the closed browser,
the retry on another persona, the breaker) is in
[fetching.md](fetching.md#after-a-block).

Kage never solves a CAPTCHA, never signs in, and never routes around a
block: a block is the site's answer, and the way through it is to visit
the way a person does, or not at all.

## One result

A result that was blocked has an `outcome` that is a block, a `block`
object, and its `attempts` (`GET /v1/results/{id}`, or `kage fetch <url>
--level N --json` at the command line):

```json
{
  "outcome": "challenge_failed",
  "status": 403,
  "block": {
    "vendor": "cloudflare",
    "kind": "managed_challenge",
    "confidence": 0.95,
    "evidence": ["header:cf-mitigated=challenge", "body:/cdn-cgi/challenge-platform/"]
  },
  "level": 3,
  "attempts": [
    { "n": 1, "render": "browser", "outcome": "challenge_failed", "status": 403, "persona": "p_…", "engine": "chromium", "exit": null, "total_ms": 9120 },
    { "n": 2, "render": "browser", "outcome": "challenge_failed", "status": 403, "persona": "p_…", "engine": "firefox", "exit": null, "total_ms": 11480 }
  ]
}
```

| Field | What it says |
|---|---|
| `outcome` | `rate_limited`, `forbidden`, `captcha`, `challenge_failed` or `soft_block` are blocks; `challenge_passed` is a challenge the browser got through, and is a page. |
| `block.vendor` | Who blocked, by the signature that matched (`policies/block-signatures.yaml`): `cloudflare`, `akamai`, `datadome`, `human`, `kasada`, `imperva`, `aws_waf`, `f5_shape`, `recaptcha`, `hcaptcha`, or `generic` when nothing named a vendor (a bare 403 or 429). |
| `block.kind` | The kind of block within the vendor, the signature's `type` (`waf_block`, `rate_limit_1015`, `interstitial`, `captcha`, `press_and_hold`, …). |
| `block.confidence` | How sure the signature is, 0 to 1. The persona's cooldown is scaled by it: a weak attribution (a bare status) cools for the short cooldown. |
| `block.evidence` | What matched: a header, a cookie, a piece of the body, a status. Nothing the site sent beyond that. |
| `level` | The level the request was made at ([levels-and-egress.md](levels-and-egress.md)). Every attempt is made at it: nothing escalates, and a block on the http path is never retried in a browser. |
| `attempts[]` | One row per attempt, each on its own persona (a handle, `p_…`): the path the level implies (`render`: `http` at 0 and 1, `browser` at 2 and 3), its outcome and status, how long it took. A row of level 0 has `persona` and `engine` null and says the `exit` it left from (`pool` or `host`); a block there is the last row. The last row is the result's. |

How to read it:

- **One block, then a page.** The retry on another persona worked; the
  first persona cools for the site. Nothing to do.
- **Every attempt blocked, the same vendor and kind.** The site refuses
  what Kage looks like there. Look at the level and the site's policy
  before anything else: a page behind a JavaScript challenge cannot be had
  at level 0 or 1 (it needs level 2 or 3), a site that turns level 2 away
  watches how its pages are read (level 3), and a `pacing` that is too
  quick for it shows as `rate_limited` first.
- **A block at level 0.** The plain client says it is Kage, and the site
  answered it. It is the answer at once: no persona cools, no other
  address is tried, the breaker does not count it. Ask at level 2 or 3,
  or leave the site alone.
- **`generic` with a low confidence.** A 403 or 429 without a vendor's
  marker: possibly the site itself, possibly a real block. If a vendor's
  page is not recognised, add a signature (below).
- **`soft_block`.** The page came, and it is a wall: an empty page, a
  consent wall a person would see too, a page that says "unusual traffic".
  Read the stored document (`include: ["dom"]`) before you decide.

## A site that is blocked often

The dashboard **Kage · blocks and breakers** (observability profile,
[observability.md](observability.md)) has, per domain, the blocks of the
last hour against what the policy allows, the block rate, and who
blocked. Two alerts go with it:

- **KageBlockRateAboveThreshold**: a domain was blocked as often in the
  last hour as its `breaker_threshold` allows. The breaker opens at that
  count. Blocks at level 0 are not counted, by the alert or the breaker;
  the dashboard's panels show every level.
- **KageBreakerOpen**: the domain is paused for `breaker_pause`. Requests
  for it are answered `domain_paused` with `retry_at`; jobs wait.

Without the profile, the same from the store:

```sql
-- Blocks of the last day by domain, level, vendor and outcome.
SELECT request.domain, request.level, attempt.block ->> 'vendor' AS vendor, attempt.outcome, count(*) AS attempts
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE attempt.started_at > now() - interval '1 day'
  AND attempt.outcome IN ('rate_limited', 'forbidden', 'captcha', 'challenge_failed', 'soft_block')
GROUP BY 1, 2, 3, 4
ORDER BY attempts DESC;

-- The breakers that are not closed.
SELECT domain, state, block_count_1h, trips, paused_until FROM domain_breakers WHERE state <> 'closed';
```

What to do, in this order:

1. **Wait for the pause.** A breaker that opens is Kage being polite on
   your behalf. The trial after the pause decides by itself.
2. **Make Kage gentler for the site.** Ask for it at level 2 or 3, not 0
   or 1, and make that the site's floor in `policies/domains.yaml`
   (`min_level: 3`, reloaded when it changes) so that no caller reads it
   as less; there also a slower `pacing`, `active_hours: enforce`, fewer
   `retries`. A lower `breaker_threshold` pauses sooner.
3. **Look at the personas** that were blocked: `kage persona show <id>`
   lists each one's cooldowns. A persona blocked everywhere, or failing
   its self-test (`kage persona selftest <id>`), is retired
   (`kage persona retire <id>`); its proxy gets a new one
   ([personas.md](personas.md)). A proxy whose personas are all blocked
   may have a burnt address: replace it
   ([proxies.md](proxies.md)).
4. **Ask whether the site should be fetched at all.** A site that keeps
   refusing a gentle visitor is saying no. Set its policy's `pacing` to
   one that never fetches, or stop sending it.

## A block Kage does not recognise

A vendor's wall that comes back as `ok` or as `generic` gets a signature
in `policies/block-signatures.yaml` (the file says how a rule is
written). Keep a sanitized copy of the page as a fixture in
`tests/fixtures/blocks/` with no personal data in it; the classifier's
tests read every fixture there. A rule may not say `escalate`: since 2.0
nothing moves a request to another level, and a file with the key is
refused (the last good signatures stay in force).
