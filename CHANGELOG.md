# Changelog

All notable changes to Kage are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A
version's date is the day, in UTC, its tag was made.

## [Unreleased]

### Fixed

- **A crawl's link answered with no content (QA round three, crawl-02).**
  At level 2 a clicked link the site answered `204` or `205` came back
  `ok`, reason `download`, with no status, in Chromium and Camoufox, or,
  in Chromium after going back to the page, waited for a load until the
  request's budget and failed `timeout`, and the reader's tab was dropped.
  The sidecar ends a click's wait at once on such an answer and reports
  its status, the click arrived, a redirect before it included; the result
  is `ok` with its status and the answer's address as `final_url`, no
  title or artifact of the page the tab still shows (typed or clicked), and
  the reader's tab stays on the page the link stands on. A crawl took that
  page's address for the one it landed on and counted a duplicate.
- **A blank page that names a frame (QA round three, class-01).** The
  `soft-blank-page` rule's exception for a page whose content is in frames
  was a pattern on the markup, so a page that renders no text and names
  an `<iframe>` in a comment, in a tag manager's `<noscript>` snippet or
  as a hidden pixel was `ok`. It now counts the frames a reader is shown
  (`shown_frames`, a new condition of the signature file): not one in a
  comment, in `<noscript>`, hidden, or of a width and a height of 0 or 1
  (a frame a script sizes, small on one side only, counts).
- **A sitemap on another host of the site under `enforce` (QA round
  three, crawl-03).** A sitemap a `Sitemap:` line or an index named on
  another host of the crawl's site was asked for before that host's
  `robots.txt` was read, and although the file disallowed it. That host's
  file is now read first, by the site's scout, and a sitemap it disallows
  is listed `robots_denied` and not asked for; one that cannot be had holds
  the site, as the seed's does.
- **Links of an untyped JSON body (QA round three, crawl-01).** At level 0
  a body of no `Content-Type` was read for links when it was text, so the
  escaped quotes of a JSON body made an address the crawl asked for. A
  body of no type is HTML only when it begins as HTML does, as a
  browser's MIME sniffing takes it; a JSON, XML or plain-text body gives no
  links, typed or not.
- **A site's empty `4xx` over `https` in a browser (QA round three,
  browser-02).** An empty `404`, `451` or other `4xx` (not `403`, `429`)
  that a browser replaced with its own page inside a tunnel is the site's
  `ok` with its status, as at level 0 and with a body: not retried on
  another persona, nothing booked, no count toward the server-error pause;
  a flow's `goto` that stopped there fails its step `flow_step_failed`, not
  retried. It was `provider_error` `navigation_failed` (`connection_failed`
  in a flow), retried, in both engines. Over plain `http` it stays a failed
  navigation (F189).
- **Firefox's head of a page (QA round three, browser-01, browser-03).**
  Lines Firefox joins with a line feed (two `WWW-Authenticate` challenges,
  as an IIS site sends `Negotiate` and `NTLM`; `Proxy-Authenticate`) come
  back one line each, as in Chrome and at level 0; the value held a raw
  line feed, in the result and in the persona's `kept-heads.json`, which is
  read as the lines from now on. A revalidated page keeps its own
  `Content-Type`, `Content-Encoding`, `Content-Length`, `Content-Range`,
  `Content-Location`, `Content-MD5` and `ETag` where the site's `304` names
  others, as both browsers do: a `304` that named `text/plain` made the
  result of an HTML page `text/plain` (and its `extract` null) on Firefox
  from then on.
- **The body of a page Firefox revalidated (QA round three, browser-05).**
  `include: ["raw"]` gives the kept page's body, and keeps its artifact,
  on Firefox as on Chrome: Firefox's driver refuses a body for the wire's
  `304`, so the engine reads the page's entry of the browser's own cache
  in the persona's profile (read only, never through the page), its
  content coding undone. Since 2.0.4 such a result said `200` with
  `raw_body` null.
- **Chrome's header order (QA round three, browser-04).** A Chrome page's
  `headers` are in the order the site sent them, read from the head as
  Chrome's debugger quotes it from the wire; they came sorted by name.
  Over HTTP/2 and HTTP/3, where the debugger has no such text, they stay
  sorted.
- **A `304` the browser kept no page for (QA round three, class-02).** A
  site's `304` to a typed address the persona's browser has no page of is
  `ok` with its `304` in both engines, as at level 0: Chrome's engine
  reports the wire's `304` and its fields, which Chrome's debugger heard,
  where Chrome broke the load off. It was `provider_error`, `aborted`,
  with no status, on Chrome.
- **A file the browser saved carries no body (QA round three,
  crawl-04).** A typed address answered with a file (`ok`, reason
  `download`) has no `raw_body` and keeps no `raw_body` artifact in either
  engine, whatever `include` asks: Camoufox's result carried the file's
  bytes, Chrome's nothing. A scout still reads a gzipped sitemap from what
  Camoufox hands over.
- **A hidden link with `^` or `|` in its address (QA round three,
  extract-06).** At levels 2 and 3 a link a style sheet hides is left out
  of `extract.links` however the browser spells its address: Firefox and
  Chrome percent-encode `^` in a path, Chrome `|`, and such a link was
  kept.
- **The guard's edges (F181).** Under `egress.mode: direct` with
  `security.resolve_on_host`, a name of `egress.direct.private_names` is
  fetched: the guard's host lookup lets it resolve to a private, loopback,
  CGNAT or unique-local address as the forwarder does (never link-local,
  metadata or multicast); every such name was `422 ssrf_denied` at
  acceptance while `kage config check` said it may resolve privately (QA
  round two, ssrf-03). A redirect hop's one trailing dot is the name's
  own, as a target's is: the hop is asked for, judged and matched without
  it, so a hop to a listed name spelled `name.` is that name and no longer
  `ssrf_denied` at every level, and under the pool the proxy is no longer
  asked for `name.:port` (ssrf-05). One redirect more than
  `http.max_redirects` ends `provider_error`, `too_many_redirects`, not
  retried, at every level; a browser's chain was `ssrf_denied` (ssrf-07).
- **A level 0 hop the proxy refused (QA round three, level0-01).** A
  redirect hop of the http path whose tunnel the proxy refused (a `403` to
  its `CONNECT`) ends `provider_error`, `proxy_refused`, not retried and
  nothing against the site, as the same target asked for directly and a
  browser's hop do (F189): the hop the client failed on counts among the
  attempt's own targets, and a hop to another site under the exit it left
  by. It was `connection_failed`, asked three times, and booked as the
  site's server error, so the site's next page waited 30 s, then 60 s.
- **A proxy that drops the SYN (F197).** The forwarder's connection to a
  proxy takes at most 5 s (`PROXY_CONNECT_TIMEOUT`, within
  `forwarder.connect_timeout`, which still bounds the proxy's answer to a
  tunnel): such a proxy cost the whole connect timeout (20 s) before
  F159's immediate refusals began.
- **A credential Kage's own forwarder refused (F211).** A `407` of the
  forwarder for a credential that names a persona but is not its own is
  noted by that persona and the target, and logged at `info` with the
  persona; a page of it that failed meanwhile ends `provider_error` with
  the new reason `forwarder_refused`, retried, nothing against the site or
  the proxy. With a tunnel of the persona to the target still open, it
  counted as the site's failure.
- **One server per database (F174).** Kage is single-instance (ADR 0005):
  a server holds a Postgres advisory lock on its database for its life, on
  a connection of its own, and a second `kage serve` on the same database
  exits at once with "another kage server is running on this database".
  A server killed and started again starts at once (Postgres lets the lock
  go with the session); one that lost its connection takes the lock again,
  or stops when another took it meanwhile. Spec §16.6 no longer promises a
  take-over by another server: a job's lease moves to the server started
  after the one that stopped (QA round two, pool-03: a job taken over by a
  second server hung `running` for good).
- **A client that hangs up leaves the queue (F128).** A synchronous fetch
  whose caller goes away within the wall time is taken out of the
  scheduler's queue: nothing is asked of the site for it, and its request
  ends `provider_error` with the reason `cancelled`; a page that was being
  fetched is finished. `kage_api_requests_in_flight` comes down for such a
  request; it only ever rose (QA round one, sched-02, sched-03).
- **A page that waits for a persona says so (F130).** A fetch carried on
  as a job waits in the scheduler as a job's page (its priority, five
  minutes for its persona) and is then told when to come back, so its job
  is `paused` with the reason and the moment instead of `running`,
  `paused: null`. A job nobody may serve within its patience (the daily
  cap, a cooling, the night) is told when even while the site has no room
  for another visit; it waited in the queue for the visits to end (QA
  round one, sched-08).
- **Crawl counters (F139).** A link no request can be made for (longer than
  8192 bytes) is `skipped.invalid_url`: no item, no page of `max_pages`;
  it failed `invalid_url` and took a page without a request, and the page
  after it was never read. A page that waits for its site or its visitor is
  `waiting`, not `fetching`, and is not in `pages` until it is asked for
  again (QA round one, crawl-07, crawl-08).
- **Flow and webhook words (F140).** A `wait_for` of `ms` longer than what
  its step may take fails the step instead of succeeding after the
  shorter time (jobs-11). The spec says which actions take `human` (the
  seven a person does with the mouse and keys; jobs-09), and that
  `webhook.deliveries` has one row per event with its last attempt, every
  attempt being kept in the store (jobs-10).
- **A job paused for the pool's return looks again when a backoff ends
  (F156).** Work nobody is ready for is told to come back when the first
  backoff of a proxy that keeps a persona that could serve it ends, not
  every minute of a backoff that can last an hour.
- **A crawl's page a dead worker left running ends with a cancel (F157).**
  Pinned by a regression test: the resume path ends such a page and its
  request `cancelled`, as for a batch and a flow since F109.
- **A flow's later site is booked its own server errors (F162).** What a
  flow's end says of a site's health (a timeout, a server error) is booked
  to the later site that showed it, whose own pause then holds the next
  flow that goes there; the first site is not paused for it.
- **Three nits of F101 and F102 (F163).** `verdicts::saved_not_shown`,
  which only tests used, is gone; `forwarder::Denials` forgets a caller
  refused nothing for 15 minutes; a later `goto`'s robots check reads the
  policy the request holds since it began.
- **A flow's cancel stops it at the step under way (F175).** Kage hangs up
  on the sidecar, which finishes the step and begins no other; the flow's
  items are `cancelled` and the job `cancelled`. A flow used to run to its
  last step after a cancel and end `succeeded` (QA round two, pool-04). A
  server that stops still waits for a flow under way.
- **A flow's result carries the robots verdict of where it ended (F186).**
  Not its first site's: a page on a later site reports that site's file,
  with its `Crawl-delay`, and a click to an address the file disallows
  never says `allowed: true` (QA round two, flows-01).
- **A fetch redirected to another site's wall books the block to that
  site (F187),** as a flow does since F101: the persona cools there and
  that site's breaker counts it; the site that only redirected is left as
  it was (QA round two, flows-03).
- **A body cut at `max_body_bytes` says so, and half a page is not kept
  (F122).** The result and its attempt carry `truncated` at every level
  (a browser's document cut at the limit counts), the extraction says
  `truncated: "max_body_bytes"`, and such a result is not put into the
  response cache. Before, the cut page was `outcome: ok` with nothing that
  said so, extracted as if whole, called not thin and cached (QA round
  one, extract-04). Migration 0020 keeps the flag with attempts and
  results.
- **The parse of a page is bounded in time (F147).** html5ever's parse
  grows with the square of the nesting; the page is now fed in slices and
  a parse that has taken ten seconds stops there, with
  `extract.truncated: "parse_time"`. Before, a page of a few hundred
  kilobytes nested two hundred thousand deep held a thread for minutes.
- **The markdown of nested quotes and lists grows with the page (F173).**
  Quotes and lists nested past sixteen are written as the blocks they
  hold, and no block is copied once per level of nesting. Before, 4000
  nested blockquotes made 32 M characters in 22 s (QA round two,
  http-07).
- **Markdown says what the page says (F142).** `~` and an `&` that would
  read as an entity are escaped, a parenthesis in a link's address is
  escaped, and a `|` in code in a table's cell is escaped; before, a
  paragraph of `~~~` opened a fence that ran to the end, `)` ended a link
  early and `&amp;amp;` read as `&` (extract-10).
- **Artifacts honour a client's refusal of gzip (F142).** `GET
  /v1/artifacts/{id}` reads `Accept-Encoding` by RFC 9110: a weight of 0 in
  any spelling refuses, and `gzip` named wins over `*`; before,
  `gzip;q=0.000` and `gzip;q=0, *` got gzip (extract-09).
- **A `<main>` is the content of its page whatever it holds (F141, the
  owner's decision of 2026-11-05).** Where the rules find less than a few
  sentences, the page's `<main>` is its content: a table of prices, a
  list, a poem. Before, such a page had `word_count: 0` and
  `thin_content: true` (extract-05).
- **`extract.links` at levels 2 and 3 leaves out what the browser shows
  nobody (F123).** The browser is asked for its links with whether each is
  shown, and a link a style sheet hides is left out, as the crawl leaves it
  out (extract-06).
- **A cache hit whose body is gone is a miss (F124).** An entry whose
  stored body is gone from the artifacts volume is a visit that fills the
  entry again; before, it answered `cached: true` with `raw_body: null`
  for as long as the entry lived (extract-08). A cached level 0 hit asked
  only for `dom` now carries its `raw_body`, as the fresh answer does.
- **Bytes of no type that are no text get no `extract` (F180).** The
  sniffing that keeps `raw_body` null decides; before, such a body had an
  empty extraction with `thin_content: true` (QA round two, http-08).
- **A release image moves no tag until it passed every check.**
  `release.yaml` pushed each image with its tags (`X.Y.Z`, `X.Y`,
  `latest`) before Trivy scanned it, so the `kage` image of v2.0.3, which
  the scan refused, carried `2.0.3`, `2.0` and `latest` unsigned and
  unattested. Each image is now pushed by digest only, and a `tags` job
  tags both once both were scanned, signed and attested, and checks that
  every tag names the signed digest.
- **A doubled letter is typed as two presses (F202).** The behaviour
  engine held a key 35 to 160 ms while the next press could come 60 ms
  later, so the second press of a doubled letter came while the key was
  still down, and the X display dropped it: `hello` arrived as `helo`
  (about one same-key repeat in fourteen). A key is now up before it goes
  down again: such a press waits until 15 to 45 ms after the release.
  Holds are drawn as before and every other interval too; over 3000 plans
  the mean interval moved from 147.2 to 147.7 ms (spec §10.4).
- **A verdict needs a document to judge (F116, F86).** A successful
  `HEAD`, a `204` or `205` (which a browser breaks off as `aborted`), a
  flow's screenshot or PDF captured without the document, a page whose
  content is in frames and an empty 200 of no type are answers at every
  level, never a block: the classifier's body rules judge nothing where
  there is no document, `soft-blank-page` leaves a page of frames alone and
  `soft-empty-body` asks for HTML. Before, a HEAD was `soft_block`, a flow's
  screenshot of a good page `soft_block`, a 204 at level 2 `provider_error`
  and a page of frames cooled its persona for hours (api-08, jobs-02,
  policy-08, policy-09, extract-07).
- **A crawl takes links from HTML only (F118).** A JSON, XML or plain-text
  answer gives no links: before, level 0 read the escaped quotes of a JSON
  body as an attribute's and asked the site for an address no page named
  (crawl-01).
- **`enforce` covers the sitemaps a crawl would ask for (F119).** A sitemap
  the site's `robots.txt` disallows, the guess `/sitemap.xml` or a file a
  `Sitemap:` line names, is not asked for and is listed `robots_denied`
  (crawl-05).
- **A link answered with a file is one request (F120).** The site's
  answer, `ok` with the reason `download`: no other persona types it, the
  reader stays, and the next page is clicked from the tab, which still
  shows the page the link stands on. A typed address answered with a file
  is the same. Before, every persona of the site asked for the file in turn
  and the item failed (crawl-06).
- **A click on a file ends at once in Chromium (F120).** The sidecar ends a
  click, or a typed load, the moment the browser begins to save a file
  (the driver's `download` event, and in Chromium the main frame's answer
  with `Content-Disposition: attachment`, since Chrome holds a plain
  `http` download for the person and its driver hears of none). Before,
  Chrome's driver reported the file's answer like a page's and the click
  waited for a `load` that never comes until the request's budget ran out,
  60 s by default, on every release since entries click (engine-chromium
  runbook).
- **Every block signature is bounded (F125).** The six rules that matched a
  marker in the body alone need a status other than 200 or next to no
  text, so a long article that quotes a vendor's block page is no block;
  a classifier test holds every shipped rule to it, with fixtures of the
  quoted pages (policy-02).
- **What a result and a label say of a block or a visit (F143).** The
  `vendor` label of `kage_requests_total` and `kage_attempts_total` is the
  result's `block.vendor` (policy-07); `referer_strategy: search_engine`,
  which no results page can serve yet, is a typed visit that says `direct`
  (policy-10); a site's own 407 over plain http (no `Proxy-Authenticate`) is
  `ok` with its status at levels 0 and 1, not `proxy_failed` (policy-11).
- **A `304` to the robots.txt scout keeps the file (F209, F90's first
  half).** A scout answered `304 Not Modified` once the kept answer ran
  out keeps what was kept (the file, or "there is none") for another
  `robots.ttl`; before, the file was dropped and the site was
  `robots_unavailable` for `robots.error_ttl` (an hour). A 304 with
  nothing kept is "unreachable" as before (QA round C's level 0 over
  `https`, tls level0.sh check 6), and its log line says
  `revalidated: false`.
- **A persona that met a wall where a fetch was redirected is held from
  the site that redirects there (F219).** The wall is booked to the site
  that showed it (F187), and for a day after a fetch of a domain last
  ended on another site, a persona that cools on that site is held from
  the domain too, until that cooldown ends and at most for that day; the
  domain's next page went to the same persona, its visitor, and into the
  same wall. The domain's robots.txt scout is not held: it reads none of
  the domain's pages, and without the file an enforcing site cannot be
  read at all.
- **`KagePoolUnavailable` counts no proxy that carries nothing (F195).**
  A proxy marked `exit_shared_with` is `degraded` but carries no persona
  and no level 0; `kage_proxy_state` counts it as `backoff` while the mark
  lasts, so a pool whose answering proxies share an exit reads as no
  usable proxy.
- **No span event carries what a log line says (F196).** The trace
  exporter strips every span event to its level, its target and where it
  was written, so no seed, path, address or token in a line leaves the
  process in a trace, whatever the line holds (spec §20); the line itself
  is in the log with the span's ids. A line's own field named `level`,
  `target` or `code.*` is dropped as well, and an error's text no longer
  reaches the span's status description (a message-less `warn!(%error)`)
  or its `exception.message` (a `&dyn Error` field).
- **A refused handshake is a webhook's `tls`, not `connect` (F208).** A
  receiver whose certificate Kage refuses, or whose TLS ends the
  handshake, shows `last_error: "tls"` in the delivery log; a connection
  that could not be made or broke off is still `connect`.
- **The health probe's `407` (F210).** A `407` read inside the tunnel of
  an `https` echo is the echo's answer (an echo that did not answer), not
  the proxy refusing its login (`proxy_auth`); the proxy's refusal of the
  tunnel still is.
- **Character references in a `<meta>` charset at level 0 (F212).**
  `content="text/html; charset=&quot;shift_jis&quot;"` and
  `charset="&#x73;hift_jis"` declare Shift_JIS, as in Chrome, whose
  tokenizer decodes them.
- **A page's text is decoded once (F213).** The subresource emulation, the
  classifier and the reader of a level 0 or level 1 page each decoded its
  body again; the text is decoded at its first use and shared.
- **No panic on a span past the last date in the health checker (F194).**
  Every date the checker adds a span to is held at the last date there is
  instead of overflowing; unreachable since the interval is bounded to a
  day (#58), but the checker is a library.
- **The persona manager's test waits for the directory (F206).** The test
  read the directory right after the store's row; only the health probes
  read the directory, and a probe in that instant presents as one an
  instant before the persona existed, so production shows no gap.
- **CI's `setup-rust` cannot take the job's whole time (F207).** Each
  `apt-get` call is bounded and tried three times, a stalled cache
  download is given up after five minutes per segment, and every step
  that uses the action is bounded to 15 minutes; `apt-get update` once
  stalled the `rust` job for 45 minutes on a mirror (run 42081090157).
- **The QA harness runs from a copy without the executable bit (F198).**
  `tests/qa/*.sh` are executable in git, `stack.sh fast` runs itself
  through `bash`, and `just lint-scripts` (in `just lint` and CI's `node`
  job) refuses a shell script that is not executable in git.
- **A cancelled flow's persona waits for the step under way (QA round
  three, flows-01).** A flow's cancel hangs up on the sidecar, which
  finishes the step it is at on the persona's context (F175); the
  scheduler gave the persona out at once, and every request it took
  meanwhile, another key's, a session's, a flow job's or a crawl's, was
  refused `context_busy`, booked as a failed attempt and counted as a page
  of the persona's day on the site. The job still ends at once, but the
  persona takes no other work until its context runs nothing again, at
  most for the attempt's time and 20 s counted from the cancel. An
  attempt that meets a busy context anyway (a server that was killed left
  it) waits for it and asks again; one still refused at its bound spends
  no retry, is no page of the day and gives a breaker's trial back. A
  context still busy at a request's third such attempt holds its persona
  the same way, so that the persona's next requests do not each wait it
  out.
- **The wait for a cut flow's context holds it, and the http path's
  cookies wait for a busy context (review of QA round three's flows-01).**
  While Kage waited for what a cut flow left running, the context counted
  as nobody's: another persona's open could close it for room, which the
  busy sidecar refused, and Kage forgot a context that still filled the
  sidecar, so the next open failed `context_limit`. And the cookies the
  persona's http path earned were left out of a browser whose context was
  busy, with a warning; they now wait for it as the page does.
- **A job that waits for its proxies' backoff is told when the first one
  ends (QA round three, flows-02).** F156 did not hold: a persona whose
  proxy is in backoff is stored `cooling` (`proxy_backoff`) and had no
  moment to come back, so a job was still told "a minute on" through a
  backoff that can last an hour. Such a persona now carries its proxy's
  `backoff_until`, and `paused.until` is the first one's.
- **A job's page never waits for a visit that does not end (QA round
  three, lock-01).** A job's retry leaves out the persona whose attempt
  failed; when that persona was the domain's one visitor under
  `domain_concurrency: 1`, nobody the page could have was visiting and
  there was no room for another, and nothing planned it again: after a
  kill and a restart of the server a crawl's page (sometimes a batch's)
  stayed `running` with nothing in flight until a cancel. The page now
  waits for that visit to end (`scheduler.visit_idle` after its last page
  at most) and goes on then, or is told the moment when that is past its
  patience. A context another server opened with other parameters
  (`context_conflict`) is the persona still busy, as `context_busy` is: no
  retry spent, no page of the day; since asking again is refused the same
  at once, the next attempt goes to another persona.
- **The documents say what Kage does (F145).** The block report runbook
  names `block.type` (it said `block.kind`) and no pacing "that never
  fetches"; the observability runbook names `bandwidth.monthly_cap_gb`;
  every `docker compose` command in the runbooks names its file; `repo.md`
  lists the ten coverage-gated crates and the five sidecar packages;
  `first-start.md` gives today's download and disk sizes; D2, D4, D17
  and D38 of `docs/00-decisions.md`, and ADRs 0002 and 0006, say what
  superseded them; the changelog's dates are its tags' (1.0.0, 2.0.0,
  2.0.1 and 2.0.3 were not); three runbook sentences that said "before
  2.0.3" of fixes that shipped in 2.0.4 say so (QA round one, docs-01,
  policy-05, docs-03 to docs-09).
- **The end-to-end suite in a closed world (F221).** `CONTRIBUTING.md`
  says how a gate runs it with nothing leaving the machine: which images
  to pull before the egress rule, the rule with an `NFLOG` capture (a
  nested namespace logs nothing with `LOG`), and which dropped packets to
  expect.
- **What the runbooks left out (QA round three, bgcalls-01, bgcalls-02,
  ops-01, ops-03).** The browsers' own calls: Chrome's `www.google.com` at
  every request, its query to Google's Autofill server with the signature
  of every page with a form it reads (kept, as stock Chrome does it, and
  written down as `docs/security-review.md` T9), Camoufox's media plugin
  manager asking Mozilla and Google 20 to 27 s after a browser starts, and
  under `egress.mode: direct` all of it from the host's own address. An
  override file is read only when every
  command names it beside `compose.yaml`, which the runbooks now say where
  they prescribe one. And an upgrade: back up before it, `migrate` applies
  the new schema, and the way back to the older release is that backup
  (`fetching.md`, "Upgrading Kage").
- **Two characters on one key are typed whole (QA round three,
  browser-06).** F202 let a key come up before it went down again only
  where the next character was the same letter, so two different
  characters on one key (`5` and `%`, `7` and `/`, `q` and `@` or `e` and
  `€` with AltGr on the German layout) still came while the key was down
  and the display dropped the second: `25%` arrived as `25` and `q@` as
  `q`, in both engines, while the flow's `type` step said ok. The planner
  now asks the display's keyboard which key each character is on, on any
  of its levels, and such a press waits until 15 to 45 ms after the
  release; a text with no such pair is planned exactly as before
  (spec §10.4).
- **A letter the layout lacks is typed as a hand types it (QA round
  three, browser-07).** A character the persona's layout has no key for
  (`é`, `ç`, `ñ` on the German layout) was typed on a spare key mapped at
  the press: the press waited up to 60 ms for applications to read the
  change while its release kept its planned time, so such keys were held
  0 to 29 ms and their neighbours' holds collapsed, and Chrome named the
  key `"\u0000"`. In Chrome a letter a dead key makes is now typed as a
  German typist types it, `´` then `e`; every other such character (all of
  them in Camoufox, which does not compose dead keys on the display) gets
  a spare key of its own before the text's first key, chosen among those a
  page hears with no `code`, so its press is on time and held as long as
  any other (medians of such holds went from 0.46 of the other keys' to
  1.03–1.05 in both engines). The page still sees such a key with an
  empty `code`; that tell is written down in both engines' runbooks
  (spec §10.4).
- **A body is an object (QA round three api-01).** A JSON array was read
  as the object the document names, by the order of its fields: a fetch
  made from `["https://…","GET",{},null,0]`, a session opened from
  `["id"]`, a `wait` taken from `["load",null,0]`, a flow's step from
  `["click","#next"]`, and the objects inside a step (a screenshot's or a
  PDF's `options`, an extract's `xhr` and `extract`). Each is `422
  validation_failed` now, naming the member where there is one.
- **Every status in the document (QA round three api-02, api-09).** An
  operation with a query parameter names the `422` it answers for one it
  cannot read (`GET /v1/admin/audit`, `/personas`, `/personas/{id}`,
  `/proxies`), `GET` and `DELETE /v1/sessions/{id}` name
  `422 invalid_session_id`, and the seven operations that need the proxy
  pool (`POST /v1/sessions`, a persona's retire, regenerate and self-test,
  a proxy's probe and replacement, `GET /v1/admin/pool`) name their
  `503 unavailable`.
- **A site's scout is named by a host (QA round three api-03).**
  `POST /v1/admin/sites/{site}/scout` refuses a wildcard and a label over
  63 characters as the policy preview does (F136), `422 validation_failed`
  with `field: site`; `*.x.test` gave `x.test` another scout.
- **The limiter's refusals of admin calls are on the trail (QA round three
  api-04).** An admin call refused `429` for the key's own limits (at
  `/metrics` and `/docs` too) is written to the audit trail, `denied`, with
  its status, code and `limit`.
- **`budget_exhausted` is counted (QA round three api-05).**
  `kage_api_refusals_total{code="budget_exhausted"}` counts every fetch the
  key's daily page budget refuses; it stayed at zero.
- **A fetch sent while the server stops (QA round three api-06).** It is
  `503 unavailable`, as a job is, and nothing of it is kept; it was a
  stored `200 provider_error shutting_down` that cost a page of the budget.
- **Webhook origins keep their passwords (QA round three api-07).** A
  refused entry of `webhooks.allowed_origins` or `webhooks.private_origins`
  is named by its place (`webhooks.allowed_origins[1]`), never printed.
- **Durations and days the program can count with (QA round three
  api-08).** Every duration is at most ten years (`3650d`) and
  `artifacts.retention_days`, `jobs.retention_days` and
  `personas.upgrade_backup_days` at most 3650, refused at load and by
  `kage config check`: `KAGE__ARTIFACTS__RETENTION_DAYS=4294967295` made
  every fetch panic and `KAGE__SESSIONS__MAX_LIFETIME=200000000d` every
  session, while `/readyz` said ready. A date counted from a duration stops
  at the last one a timestamp has rather than panic, and a fetch whose
  task panicked is `500 internal`, no longer "the server is stopping".
- **Nested code is one code span (QA round three, extract-01).** A code
  span holds no code span, mark or link: nested `<code>` elements and what
  is inline in them are written as one span with their text. Each level
  was a span around the inner one with a fence one backtick longer, so
  the markdown grew with the square of the depth and its time with the
  cube: 4000 levels (61 KB) took 26 s for 16 M characters, and a 300 KB
  page held a core for more than half an hour.
- **Markdown reads as text where the page had text (QA round three,
  extract-02).** The guard of F142 now covers every line a hard break
  starts (`Features:<br>- Waterproof` is no list), a mark alone (`#`,
  `-`, `+`, a line of `=`, no longer an empty heading, an empty item or a
  setext underline), a `!` right before a link (no image) and the closing
  `#`s a heading's text ends with (`## Section \#`).
- **A `<dialog>` is never content (QA round three, extract-05).** Open or
  closed, as the role `dialog` was already; a closed one is also what the
  markup hides, in a `<main>` taken whole too.
- **An artifact's two representations have two entity tags (QA round
  three, extract-03).** `GET /v1/artifacts/{id}` sent the gzip and the
  unpacked bytes with one strong `ETag`; the gzip's is now the content's
  hash with `-gzip` after it (RFC 9110 §8.8.3).
- **No cut body an older version cached is served as whole (QA round
  three, ops-02).** The response cache's key is at version 3: an entry
  that 2.0.4 or earlier kept may hold a body cut at `max_body_bytes`,
  which nothing in it marks, and after an upgrade it was served as
  `cached: true, truncated: false` until it expired. Those entries are no
  longer found and expire as before. Results stored before migration 0020
  (F122) read `truncated: false` whatever their body was: those bodies
  were not looked at.
- **`robots.matched_rule` names the pattern as the file writes it (QA
  round three, level0-03).** Since F179 it was the rule as compared, an
  escape of an unreserved character decoded and others in upper case
  (`Disallow: /a/~user` for the file's `Disallow: /a/%7euser`); its
  pattern is the file's again, and the comparison is unchanged. Its key
  is always spelt `Allow:` or `Disallow:`, whatever the file's case or
  spacing (QA round four's pool0-03: the documents said the whole line).
- **An unreachable robots.txt is never `allowed: true` (QA round three,
  level0-04).** A page refused `robots_unavailable`, and a page of a site
  that reports its file while the file is kept as unreachable, say
  `robots: { fetched: false, allowed: false }`; they said `allowed: true`
  beside the refusal.
- **A followed downgrade is recorded (QA round three, level0-05).** A
  redirect of the http path from `https` to `http` is followed, as a
  browser follows it, and kage's log now says so at `info` with the hop's
  number and both hosts; spec §9 promised the record and nothing kept it.
- **A body cut inside a character ends at the last whole one at level 0
  (QA round three, extract-08).** The http path decoded the cut bytes
  with a replacement for the broken last character, so `raw_body` and
  `extract.text` ended in U+FFFD and `raw_body` could be longer than
  `max_body_bytes`; it now steps back to a character as the sidecar does
  at levels 2 and 3, in every encoding.
- **The health probe trusts `http.ca_file` (QA round three, level0-06).**
  The probe's clients, the persona's and the fallback's alike, add the
  operator's authorities to their roots as the persona's clients do, so an
  echo of the operator's own under a private authority answers the probe.
  Its handshake was refused, and the probe went on to the confirmation
  echo, the public one by default, which then saw every proxy's address;
  that step is now logged at `warn`.
- **A hang-up at the wall time leaves the fetch whole (QA round three,
  flows-03).** A synchronous fetch that outlasts `scheduler.sync_wall_time`
  is written as a job by a task of its own, which a client that hangs up
  in that moment no longer cuts short. A cut there left either a job
  leased to nobody, whose worker began the same request again when the
  lease ran out (the site got the page twice and the job ended `failed`,
  `internal`), or a request with no job and no end.
- **A server cut off from its database stops within 55 s (QA round
  three, lock-02).** A look at the server lock answers within 5 s or
  confirms nothing; when a look made 40 s or more after the last
  confirmation fails too (45 to 55 s after it), the server stops, cuts the
  pages under way at once instead of finishing them within
  `jobs.wind_down`, and exits with code 75. This is before Postgres
  gives a silent session's lock up (about a minute) and a second server
  could take it and claim the same items.
  The look waited on the dead session until the network healed, and the
  server served on, `/readyz` ready, for 4 to 51 s beside the server that
  held the lock.
- **A scout whose robots.txt took long reads none of the site's pages
  (found by round F's gate).** A site's scout chosen inside a request was
  kept from its pages only while the pipeline's memory of it lasted, half
  a minute; a file that took 31 s to have left the scout free, and its
  open visit read the page. The request now records the scout its gate
  chose, from the store when the memory has let it go.
- **`http.max_body_bytes` bounds a browser's page too (QA round three,
  extract-07, extract-04).** At levels 2 and 3 a page was kept whole up to the
  sidecar's own 10 MB whatever the operator set, and a request's
  `max_body_bytes` above the operator's was honoured there though cut at
  level 0; a flow kept 10 MB whatever was asked. The browser path is now
  sent the request's limit, else the operator's, never more than the
  operator's, and the sidecar's flow request takes `max_body_bytes` as its
  navigation does (sidecar contract 1.9.0: an optional field).
- **A site held by its own pause is not said to want a persona (QA round
  three, level0-02).** A request or a job's item held past its patience by
  the site's pause after server errors was told `no_persona`, also at
  level 0, which has no persona: an operator sent to `kage persona list`
  found nothing wrong. It is now told `server_errors`, and level 0's wait
  for a `Crawl-delay` `crawl_delay`; `no_persona` keeps its meaning.
- **A later site a click led to is booked its own timeouts (QA round
  three, flows-04).** F162 reached a `goto` only: a flow whose click sent
  the tab to another site that never answered paused the flow's first
  site, which had answered every page, while the silent site went on
  being asked. The sidecar now names the address the tab was sent to
  that had not answered (`unanswered_url` of a page result, sidecar
  contract 1.9.0), and its timeout is that site's.
- **A proxy's `5xx` to a tunnel is the target's failed connection (QA
  round four, guard-03).** The forwarder knew a `502`, `503` or `504` to a
  `CONNECT` meant the exit could not reach the target, but noted it
  nowhere the executor reads, so the client's own failed tunnel stood as
  `proxy_failed` at every level: the proxy was blamed, the pool told, and
  the page tried on other personas as a failed proxy. It is noted now, and
  such an attempt is `connection_failed` (`navigation_failed` for a
  browser's page) with a server error's courtesy, as spec §8.3 says and
  as a tunnel that opened and broke is booked: it counts in the site's
  row of server errors, and a scout's `robots.txt` behind it is kept as
  unreachable for `robots.error_ttl`. The forwarder logs it at `info`
  with the proxy and the target. Another answer to a `CONNECT` that is
  neither a success, a `4xx` nor a `5xx` stays the proxy's failure.
- **A later site a redirect led to is booked its own timeouts (QA round
  four, sched-01).** A flow's `goto` answered with a redirect to a site
  that never answered ended with the first site's page and its 200, and
  the first site was paused for the timeout while the silent site went
  on being asked; through a click, the site that only redirected was
  paused. The sidecar now names the redirect's last hop as the address
  that did not answer, after a `goto` or a click, and the timeout is
  that site's. A shown page that answered with a server error itself is
  booked its own error, wherever it sent the tab after.
- **A robots.txt no scout could bring is never `allowed: true` either
  (QA round four, pool0-01).** level0-04's fix reached only a file kept as
  unreachable. A page refused `robots_unavailable` because the scout's
  attempt failed before the site had a say (the proxy refused the
  `CONNECT`, or could not reach the site; nothing is kept then) or its
  scout was not ready, and a page refused `robots_no_scout`, still said
  `robots: { fetched: false, allowed: true }` beside the refusal, at
  every level; so did a flow that ended on a site whose file is kept as
  unreachable. They say `allowed: false`.
- **A `type` step whose budget runs out stops typing.** The step failed
  at its `timeout_ms` (30 s by default, about 200 characters), but its
  typing went on after the flow had returned: into the page of the
  context's next request, over a retry's own keys, and a release could be
  lost and leave a key down. Typing now stops at its next key once the
  budget is over, every key down comes up on the key code it went down on,
  the keys lent for the text are given back, and the flow returns once
  that is done. A flow's `press` of a character the layout makes with a
  dead key, or has no key for, is typed as `type` types it (spec §10.5).
- **A timed-out step's input reaches no other page (QA round four,
  input-01).** A human `select` whose step ran out of time pressed its
  arrows on after the flow returned, for as many options as it had to
  cross, into the persona's next request's tab (another key's), and its
  `Enter` submitted that page's form, in both engines. Every step of a
  flow now runs with an input grant of its own and every request with one
  around it, revoked when the step's grace ends and when the request
  returns; every move, button, wheel notch and key of the persona checks
  the grant of the work that asks for it and never acts for a revoked
  one, and what a revoked step still held down is let up. A `select`
  also stops before its next key once its budget is over. Firefox's print
  shortcut for a `pdf` step goes through the same gate; an act outside any
  request is refused; a press that reaches the display after its grant
  was revoked is let up at once; and a `human: false` step's driver action
  gets only what is left of the step's time after the wait for its
  element.
- **A site's robots scout is never a session's persona there (QA round
  four, pool0-04).** The pipeline kept the scout out of every fetch of
  its site but a session's: a session for the site could be pinned to
  it, and then read the site as a person from the address its level 0
  leaves by (`kage/<version>` and a browser's pages from one address).
  A session opened for a domain is not pinned to its site's scout; a
  fetch or a job in a session whose persona is the scout of a site it
  reaches (its page, or a `goto` of its flow), or has become it since
  the session was opened, is `409 session_persona_conflict` with the
  constraint `robots.scout`, and nothing is fetched; a job's item is
  refused so when the scout was chosen after the job was accepted, and
  the pipeline itself keeps a site's scout out of a pinned attempt as
  out of any other.
- **A robots.txt nobody could ask for is asked for again a minute later
  (QA round four, pool0-02).** When the scout's attempt failed before the
  site had a say (the proxy refused the `CONNECT`, the sidecar), every
  fetch of the origin sent the scout again, and each refusal named a later
  `retry_at`. The fetches of the next minute are refused
  `robots_unavailable` with the same `retry_at`, and nobody asks, as
  spec §14 says.
- **A fetch carried on as a job is not a job of the key's bound (QA round
  four, api-05).** `jobs.max_open_per_key` counted a fetch carried on as a
  job (`origin: sync_overflow`) though nothing held such fetches to it: a
  key with a bound of 1 held 6, and a client that never made a job was told
  `jobs_limited` for jobs it did not ask for. The bound is the jobs a key
  asks for; a carried fetch is never refused by it and no longer counted
  in it, and the key's limits on fetches (`max_concurrent`,
  `requests_per_minute`, `daily_page_budget`) hold it (spec §16.2, the
  security review's L2).
- **A warm-up site carries no credentials (QA round four, api-01).**
  `personas.warmup.sites[].url` took an `https` URL with a password, which
  `kage config check` printed and every persona would have sent; over
  `http` the refusal repeated it. It is refused, and every refusal of a
  site names it by its place (`personas.warmup.sites[2]`), never repeating
  it.
- **Every `429` in the API document says it carries `Retry-After` (QA
  round four, api-02).** `POST /v1/fetch` and `POST /v1/jobs` document a
  `429` of their own, which did not declare the header every `429`
  carries; a client generated from the document now knows it.
- **A refused configuration value names its key and where it came from
  (QA round four, api-03).** The message said `"global.robots.ttl" in
  (alloc::string::String, figment::value::value::Value)` for an
  environment variable and "YAML source string" for the file. It now says
  `robots.ttl: ... (from the environment variable KAGE__ROBOTS__TTL)` or
  the file's path, and a policy file's refusal names its key without
  figment's profile.
- **A purge before 4713 BC is the caller's value (QA round four, api-04).**
  `POST /v1/admin/purge` with a `before` earlier than Postgres keeps
  failed inside the store, `500 internal`; it is `422 validation_failed`
  with `field: before`.
- **A crawl's refusal never repeats a seed's password (QA round four,
  api-06).** A seed with credentials was refused, and the problem document
  repeated it whole in `seed`; that member is left out for such a seed,
  `item` naming it, as a fetch, a batch, a flow and a webhook refuse one.
- **A CORS origin is written as a browser sends it (QA round four,
  api-07).** An origin in upper case or with its scheme's own port (`:443`)
  was taken without a word and matched nothing a browser sends. It is
  refused at load with the form that would, as F136 refuses a wildcard.
  **Upgrade note:** a server whose `server.cors_allowed_origins` holds
  such an entry no longer starts; write it as the message says.
- **The forwarder's refusal of a credential names its caller as every line
  does (QA round four, guard-01).** F211's line wrote the persona as
  `Some(PersonaId(..))` and a plain client as `None`; it writes `persona`
  with the id, `proxy` or `plain` for a plain client. The http client's
  debug lines write their `host` as the host, not `Some("..")`.
- **G3 reaches a private webhook receiver (QA round four, guard-04).** A
  webhook on a port the browsers refuse (`:25`, `:025`) was accepted and
  posted to when its origin was listed in `webhooks.private_origins`,
  which skipped the guard and with it the port check. It is `422
  ssrf_denied` whatever list its origin is on, and an origin of either
  list on such a port is refused at load. **Upgrade note:** such an origin
  stops a server from starting.
- **A domain a reload adds is labelled while room is left, and the
  server says when it is not (QA round four, obs-01).** A domain a policy
  reload named after the `domain` label bound filled was counted as
  `other`, with no breaker threshold, until a restart, and nothing said
  so. A reload now names its domains at once while room is left, logs a
  warning naming how many got none and the first, and the spec, the
  example configuration and the observability runbook say what `other` is
  and that the next start gives them their labels.
- **The `no_persona` row of the fetching runbook says when to ask again
  (QA round four, sched-02).** It said "a minute on" when every proxy is in
  backoff; since F156 `retry_at` is the first backoff's end, else a minute
  on when nobody knows.

### Changed

- **Durations are at most ten years (QA round three api-08).** A duration
  over `3650d` (`87600h`) in the configuration or in a policy file
  (`robots.ttl: 5000d`, `cache.max_ttl: 4000d`) is refused. Check yours
  before upgrading: a configuration with one no longer starts (`kage config
  check` names the key), and a policy file with one is refused at start,
  or on a reload, where the last good policies stay in force.
  `artifacts.retention_days`, `jobs.retention_days` and
  `personas.upgrade_backup_days` are at most 3650 likewise.
- **A job's page whose fetch panicked fails (QA round three api-08).**
  Such an item ends `failed` with the error `internal` (the log has the
  cause); it was taken for a stop of the server and queued again at once,
  so it panicked again with every turn.

### Added

- **Two reasons: `server_errors` and `crawl_delay`.** A synchronous
  fetch's `reason`, a job's `paused.reason` and
  `kage_scheduler_refusals_total{reason}` name the site's own pause after
  server errors (its 5xx and timeouts in a row, at any level; nothing of
  Kage's is wrong) and level 0's wait for the `Crawl-delay` of the site's
  enforced `robots.txt`, each with when to ask again. They were
  `no_persona` (level0-02); the runbook's reasons table says what to do
  about each: wait, or nothing.

- **A daily scan of `main`'s images (F220).** `image-scan.yaml` builds the
  three images from `main` every morning and runs `just scan-images` with
  `.trivyignore`; a fixable HIGH or CRITICAL finding that is new, or an
  accepted one past its review date, fails it before a tag is pushed
  rather than at release (`docs/runbooks/repo.md`, "Daily image scan").
- **The server's lock is a metric (QA round three lock-02).**
  `kage_server_lock_held` (1 while the latest look found this server
  holding its database's lock) and
  `kage_server_lock_confirmed_timestamp_seconds` (when it last did) show
  a server that serves on a lock it cannot confirm; nothing but the log
  said so. Watch the age, `time() -
  kage_server_lock_confirmed_timestamp_seconds` (spec §20,
  `docs/runbooks/observability.md`).

### Security

- **The ports the browsers refuse (F57, the security review's G3).** The
  SSRF guard refuses the 82 ports Chromium and Firefox refuse to connect
  to (the Fetch standard's bad ports: 21, 22, 25, 110, 143, 6000, 6667, …)
  on every road: a URL naming one is `422 ssrf_denied` at every level, a
  redirect hop to one is `ssrf_denied`, and the forwarder refuses a tunnel
  to one. Level 0 and level 1 could send an HTTP request through an exit
  to a mail, shell or chat server's port.
- **The forwarder relays what it checked (F57, G5).** The request target
  that goes to the persona's proxy is the one the forwarder parsed and
  judged: the `host:port` of a tunnel and the serialized URL of a plain
  request, the host in its normal form; it relayed the client's own text,
  which a proxy that parses otherwise could read as another host.
- **A key's open jobs are bounded (F57, the security review's L2).** A key
  may have `jobs.max_open_per_key` (20) jobs queued, running or paused and
  `jobs.max_queued_items_per_key` of their items waiting, the new job's
  included, a crawl's `max_pages` counted from its acceptance; past either,
  `POST /v1/jobs` is `429 jobs_limited` with the counts, the bounds and a
  `Retry-After`, nothing stored, counted in
  `kage_api_refusals_total{code="jobs_limited"}`. The same `Idempotency-Key`
  sent twice at once is still one job and a replay. One key could queue
  jobs without end, up to the global limits only. Upgrading: left unset,
  `jobs.max_queued_items_per_key` is twice `jobs.max_items` and at least
  20 000, so a configuration that raised `jobs.max_items` still starts; one
  that sets it below `jobs.max_items` is refused.
- **The `domain` label is bounded (F57, L3).** The first
  `server.metrics_max_domains` (500) distinct domains since the start have a
  `domain` label of their own in every metric that carries one, the domains
  the policy files name first; every later domain is counted under `other`. A key that asked for many hosts grew
  the series of `kage_requests_total` and `kage_attempts_total` without end.
- **The sidecars no longer mount the stored results (F57, V1).** Their files
  are on a volume of their own, `sidecar-files`, mounted at
  `/var/lib/kage/artifacts/sidecar` in both sidecars and in kage; no sidecar
  mounts the `artifacts` volume, so a browser that was taken over cannot
  read or delete other requests' stored results. `just hardening` refuses a
  sidecar with any other volume. Compose creates the volume at the next
  `docker compose up`; a file a sidecar wrote at that moment and kage had
  not yet taken is lost with its page's screenshot or PDF.
- **RUSTSEC-2026-0459 is accepted (`libsqlite3-sys` 0.37.0).** The
  advisory names C SQLite's CVE-2026-119375; the crate is only an entry
  of `Cargo.lock` (sqlx's SQLite driver, which Kage never enables), so no
  binary builds or links it. Ignored until 2026-12-31 or the first sqlx on
  `libsqlite3-sys` 0.39 (`docs/security-review.md`).

## [2.0.4] - 2026-11-07

### Fixed

- **Firefox's result headers are the lines the site sent (F190).** Its
  driver split every value at its commas, so `Date` and `Last-Modified`
  came as two headers each (`Tue` and `03 Nov 2026 ... GMT`) and a list as
  one header per item; the pieces are one line again, as Chrome reports
  them (QA round two, left-03). A comma the site wrote without a space
  after it comes back with one: the driver no longer tells the two apart
  (spec §16.5).
- **A page a browser revalidated is reported as the page it shows, in both
  engines (F178).** Chrome reported `200` with the 304's few headers, no
  `Content-Type` among them; Firefox reported `304`. Both now report the
  kept page's status and its head brought up to date by the 304's fields:
  Chrome as its debugger announced it, Firefox from the heads the engine
  keeps in the persona's profile (`kept-heads.json`, at most 300 pages, 30
  days, no `Set-Cookie`). A Firefox page whose head is not kept is the
  wire's `304` with its own fields, never a guess (tls-02; spec §16.5).
- **Chrome says the status of an empty error answer it replaced with its
  own page (F204)**, from its debugger, as Firefox does since F189: an
  `https` site's empty `403` is the site's `forbidden` on Chrome too, and
  over plain `http` nobody's block in both engines (spec §8.3, §10.2).
- **A page that sends the tab on before its `load` to an address nothing
  answers ends at once on Firefox (F200).** A script or a form that sent
  the tab to an address the forwarder refused left Firefox on its own
  page, which fires no `load`, and the fetch ended only at its
  `timeout_ms` (60 s in the guard's end-to-end check, where every other
  road ends in under 6 s); it now ends there in a fraction of a second,
  `navigation_failed` where the tab was sent, so `ssrf_denied` (§19).
- **A result's `title` is the page the tab shows at the capture (F201).**
  It was the look at the page before the dwell, so a page that moved
  itself on while the persona read it kept its old title beside the new
  page's address and document; the page is looked at again after the
  dwell (spec §10.4).
- **The Firefox runbook says what a page sees of WebRTC: no candidate at
  all (F183)**, not host candidates as `.local` names; the parity suite
  now holds both engines to it (ssrf-04).
- **What a refused hop leaves in the browser is said (F138).** A hop the
  forwarder passed and the check after the fact refused (a URL with a user
  name, a public address by number) was loaded by the browser, and what
  its page stored (a cookie, `localStorage`) stays in the persona's
  profile with the origin that wrote it, as any site's state does;
  nothing of it is returned or kept by Kage. Spec §19 says so (ssrf-06).
- **A tab a page opens under Chromium's `inject: window` asks as the
  browser really is (F203, documented, not fixed).** Its first request
  leaves before the driver reports the tab, and the pinned driver offers
  no way to reach the tab sooner; spec §10.5 and the Chromium runbook say
  what was measured. The default `inject: none` is not affected.

### Security

- **Two `zlib1g` findings of the `kage` image's runtime base are accepted
  until 2026-11-21.** Trivy refused the `kage` image of the 2.0.3 release
  for CVE-2026-76844 and CVE-2026-85091 in the distroless base's `zlib1g`,
  fixed in Debian (`zlib1g` `+deb13u1`) but in no distroless build yet.
  CVE-2026-85091 is a heap buffer overflow in `gz_vacate()`; Trivy's title
  for CVE-2026-76844 names webpack-dev-middleware, while its Debian data
  lists that finding on `zlib1g` as fixed by the same update. The `kage`
  binary never loads `libz.so.1`, and the image has no shell or package
  manager, so nothing reaches the library; `.trivyignore` accepts both
  until the base carries the fix or 2026-11-21, whichever comes first.

## [2.0.3] - 2026-11-07

The tag `v2.0.3` stays where it is, with no GitHub release and no
released `kage` image: the release's Trivy step refused that image for
CVE-2026-76844 and CVE-2026-85091 in the runtime base's `zlib1g`
(2.0.4 accepts them: the binary never loads `libz.so.1`). Its
`kage-browser-chromium` image was published by digest. The tag
`ghcr.io/serpe-nt/kage:2.0.3` points at an image pushed before the
scan refused it, unsigned and unattested: do not use it; 2.0.4
replaces it.

### Fixed

- **A proxy that comes back on an address a dead proxy once had keeps its
  persona (F177).** The shared-exit check compares a probe with the
  latest probes of the other live proxies that read an address; a proxy
  that is down no longer lends the address its record kept from before
  the outage. Before, a restarted proxy that Docker or the provider gave a
  dead proxy's old address was marked `exit_shared_with` that proxy and
  its persona retired for good (QA round two, timers-04).
- **An operator's probes add no backoff cycle (F129).** `kage proxy
  health` and the admin probe look at a proxy in backoff as it is: a pass
  ends the backoff, a failure is recorded and leaves its end and its count
  alone. Before, every failed look was one more cycle, so a few probes in
  a row added minutes to an outage and spent the three cycles toward a
  replacement (sched-06).
- **Level 0 waits out a suspected outage of the pool (F193).** A site's
  plain exit whose latest probe failed is held by the outage guard as a
  persona's proxy is since F169: the request is answered
  `plain_exit_unavailable` with `retry_at`, a job waits, and the forwarder
  relays nothing through the held exit; before, a level 0 job still met
  dead exits during an outage (the review of #50).
- **After a breaker's pause level 0 goes on, and `kage_breaker_open` is
  read from the store (F126).** A plain client is refused `domain_paused`
  with the pause's end as `retry_at` while the pause runs, and goes on
  when it ends, never waiting for the trial a persona makes; before, a
  site read only at level 0 stayed refused with no `retry_at` until some
  browser request came (policy-03). The gauge is read from the stored
  breakers at every scrape: it survives a restart and is 0 when the pause
  ends, so `KageBreakerOpen` follows the pause (secrecy-03).
- **An attempt through a proxy nobody answers for fails within seconds
  (F159).** The forwarder looks up a proxy's name within five seconds and
  refuses a proxy it could not reach at once for fifteen seconds, so a
  browser's second try of the tunnel costs nothing; in the QA world an
  attempt through a stopped proxy took 40 s.
- **The log names a proxy by its id (F153).** "proxy added to the pool",
  "proxy retired" and the replacement lines carry the proxy's host and port
  at DEBUG only, and an alert's details in the log have the host, port and
  exit address `[redacted]` at INFO and above; the nightly's public log
  printed the pool's addresses.
- **Grafana's trace search by service works (F176).** The profile's Jaeger
  is pinned to 2.20.0, the last release with the v1 search Grafana's Jaeger
  data source sends (2.21.0 removed it and no Grafana release searches
  through v3 yet), and the data source's v3 toggle is off; with 2.22.0
  Explore's search answered `404` (timers-01).
- **No seed in a trace, and a missing scope is counted (F182).** The
  attempt's log line, which leaves with its span as an event, no longer
  carries the attempt's seed (timers-03; the seed stays in
  `attempts.seed`). `kage_api_refusals_total{code="scope_missing"}` counts
  the `403` it names (timers-02).
- **Observability's small items (F144).** The sidecars' event loop series
  carry `engine`, so the merged exposition holds each once (secrecy-06); a
  sidecar's lines of a traced request carry `span_id` (secrecy-07) and are
  in Kage's shape: `timestamp`, `level` in upper case, `fields.message`,
  `request`, `persona` (secrecy-08); spec §20 lists the sidecars' families
  and labels (secrecy-09); `RUST_LOG` and `LOG_LEVEL` reach the services of
  the shipped compose file (secrecy-11); a redirect target a site sent is
  logged by its origin only, never its query (secrecy-12).
- **The sidecars' counters exist from zero (F150).**
  `sidecar_context_opens_total`, `sidecar_context_closes_total`,
  `sidecar_browser_recycles_total`, `sidecar_selftests_total` and
  `sidecar_flows_total` are exported at zero for every result and reason
  from the start, as Kage's own are since F110, so `increase()` counts the
  first event.

- **The engine images take Debian's security updates at build.** On
  2026-11-05 Trivy refused both engine images for CVE-2026-109518 in
  `apt` 3.0.3 (fixed in 3.0.4) while the pinned base image, and the
  newest build of its tag, still carried it. The runtime stage of
  `sidecar.Dockerfile` now runs `apt-get upgrade` after its
  `apt-get update` for both engines, so a rebuild carries the
  distribution's security updates for the base layer as it already took
  the current point release of the packages it installs; nothing
  fingerprint-critical moves.

- **After a block the retry leaves from another /24 and another
  autonomous system whoever is ready first** (F168; QA round two's
  pool-01). Each persona waits its own pause before a retry, at its own
  pace, and the planner compared only the personas whose pause was over,
  so the fastest persona took the retry whatever its address: with five
  proxies in three /24s and four autonomous systems, nine of nine blocked
  requests went on from the blocked persona's /24, or from its system
  while personas of others were untried. The rule of spec §8.2 and the
  score now run over every persona that could take the retry once its
  pause is over; a tie is drawn from the retry's own seed, and the retry
  waits for the pause of the one drawn, never longer. A persona that is
  busy, cooling, at its cap or asleep is not waited for, and the
  neighbour of the blocked persona takes the retry when nobody else is
  left; a visitor of the site in the blocked /24 no longer takes it while
  another can.
- **A suspected outage of the pool pauses jobs instead of failing them
  on dead proxies, and `KagePoolUnavailable` fires for it** (F169, F151;
  QA round two's pool-02). In a pool of four or more the outage guard of
  spec §7.3 freezes the failure counters while more than half of the
  pool fails its latest probe, so no proxy reached backoff, every
  persona stayed ready, and a batch sent during an outage of every proxy
  was `failed` `proxy_failed` within half a minute, its sync fetch
  `proxy_failed` with no `retry_at`; the gauge `kage_proxy_state` kept
  such a pool `healthy` and the alert could not fire. While the guard
  holds, a proxy whose latest probe failed is now held as one in backoff
  until a probe of it passes: its persona is not ready, a job is
  `paused` with `no_persona` and resumes by itself (F107), a synchronous
  fetch is answered `no_persona` with `retry_at`, the proxies whose probe
  passed go on carrying, and the gauge counts the held proxies as
  `backoff`, republished after every probe while the guard holds or is
  lifted.
- **A flow's later site keeps to its `domain_concurrency` while another
  persona visits it** (F188; QA round two's left-01). F101's hold on a
  flow's later site counted pages in flight, not visits, so while
  another persona's visit of a site with one seat was open, a flow's
  `goto` read it from a second address at once (a batch item of the same
  site waited, as it should). A persona that is not visiting such a site
  now waits for a seat as a page of the site does, until enough of the
  other visits are over; a visitor of the site may take the flow at
  once, and a synchronous flow goes past the seats only for a visitor's
  break that does not end within `visitor_wait`.
- **With `security.resolve_on_host`, every name is resolved on every road
  (F57, the security review's G1).** QA round two found that a public
  page's redirect, meta refresh, script or self-submitting form, or a
  flow's click, sent a browser to a name of the operator's network, the
  forwarder relayed it by its URL alone, and the private page came back
  `ok` with its document kept, in both engines at levels 2 and 3, while
  the same name asked for directly was `422 ssrf_denied` (ssrf-01). Under
  the setting the forwarder now looks up every name it relays through a
  proxy, a browser's own requests and a scout's reads included, and
  refuses one with an address in a denied range, or with none, before the
  proxy is asked; the refusal is the guard's, so the attempt ends
  `ssrf_denied`, never retried, nothing kept. Each name is looked up once
  and its answer kept 30 seconds; the proxy still resolves where it
  connects (G2). After the navigation the chain of a page and every page
  of a flow are resolved with the http path's own address check, so a hop
  that got past the forwarder is refused all the same. `kage config check`
  warns about a declared proxy list without the setting.
- **A browser's redirect to a scheme, and Firefox's error page, are
  `ssrf_denied` (F167).** A redirect to `file:`, `data:`, `ftp:` or
  `javascript:` ended `provider_error`, in Firefox with its own error page
  kept and tried twice (ssrf-06), and under `egress.mode: direct` a
  Firefox meta refresh to a refused name ended `soft_block` with that page
  kept and a block booked to the site (ssrf-02). A server's redirect may
  now only send a browser to `http` or `https`, any other `Location` is a
  refused hop; the Chromium engine reads from Chrome's debugger the
  redirect Chrome refuses by itself and never hands the driver, and a
  page is Firefox's error page by its document's URI as well as by its
  address. One outcome in both engines and both modes: `ssrf_denied`,
  nothing of the browser's page kept, nothing booked, no retry.
- **A level 0 redirect to another site leaves by that site's own exit
  (F166).** A level 0 fetch, or a level 0 `robots.txt`, that redirected
  to another site asked for the hop from the first site's plain exit, so
  the second site got `kage/<version>` from an address that may read it
  as a person (http-02). Each hop now leaves from its own site's plain
  exit (the site's own, else its scout's); when that exit cannot carry
  traffic the hop is not asked for at all and the attempt ends
  `provider_error` with the new reason `hop_exit_unavailable`.
- **A tab a flow's page opens is the flow's, behind its page and closed
  with it (F184).** After a flow clicked a link that opens a new tab
  (`target=_blank`), the persona's later clicks and typed text went into
  that tab, which the browser shows in front, on both engines, while every
  step reported `ok: true` on the flow's own page; with the link on
  another site the text reached that site, and the tab ran on across
  other keys' requests for up to 17 minutes (QA round two, flows-02). The
  flow's page now stays the flow's page: the sidecar puts it in front of
  such a tab again as soon as the browser reports the tab, before every
  step that acts and between a `type` or `select`'s click and its keys,
  and a step whose input could not reach the flow's page fails. The step
  names the tab's address in `opened_tabs` (sidecar contract 1.8.0, and
  the public job item's `step`), nothing of the tab is captured, every
  such tab is closed when the flow ends, and a tab a page opens outside a
  flow is closed at once (spec §10.5).
- **A self-test that read none of its pages exits non-zero on every road
  (F185).** `kage selftest browser --persona <id>` and `kage persona
  selftest <id>` exited 0 on a run that read none of its pages
  (`passed: false`, `verdict: inconclusive`), so a script that checked the
  exit status took a run that tested nothing for a pass, though `kage
  selftest browser --proxy` exited 1 since F105 (QA round two, flows-04).
  The persona's self-test commands now exit with what `passed` says: a part
  that could not read its pages or its echo is `inconclusive`, its table
  ends with a line that says what was not tested, the JSON document has a
  `verdict`, and the command exits non-zero. The hint for a missing echo
  no longer names an `--echo` that `kage persona selftest` does not have
  (spec §7.4).
- **A proxy's refusal of a target is never the site's `forbidden`
  (F189).** On Firefox a proxy that refused a plain `http://` target with
  an empty `403` made the attempt the site's `forbidden`, with Firefox's
  own error page as `dom_html`, after the whole `timeout_ms`; the persona
  cooled 15 minutes and the breaker counted a block the site never gave
  (QA round two, left-02). Chrome said `navigation_failed`. Both engines
  now end such a navigation as soon as the browser shows its own page,
  keep nothing of it, and report `navigation_failed` with the status the
  browser saw (Firefox) or none (Chrome): an empty error answer over plain
  http may be the proxy's as well as the site's, so it is nobody's block
  (spec §10.2). Over `https` only the site answers, so the status Firefox
  saw stays the site's verdict (a WAF's empty `403` is `forbidden`). A tunnel the proxy refuses with a `4xx` other than `407`
  (a provider's `403` to the `CONNECT`), which both engines reported as a
  failed proxy, now ends `provider_error` with the new reason
  `proxy_refused`, not retried: nothing against the site and nothing
  against the proxy (no setting aside, no probe), and the forwarder logs
  the refusal at `info` with the proxy and the target's host (spec §8.3).
  A `5xx` to the tunnel stays the target's failed connection.
- **A page that moves itself away under the persona's hand ends where it
  went (F192).** A page that sends the tab on at once (a meta refresh of
  `0`, a script) while the hand reads it to scroll could make a level 2
  or 3 navigation with `behavior: human` fail as `sidecar_error`: on
  Firefox the read failed with "Execution context was destroyed" and the
  error escaped the navigation as a 500 (seen in `tests/e2e/direct.sh`'s
  meta refresh check on #53's CI; a race, in production too). Such a read,
  on a page that asked for a new document, now ends the behavior there:
  the tab is given up to 3 s to load where it went, and the capture says
  where that is (a refused name is `ssrf_denied`). Any other failed read
  fails as before (spec §10.6).
- The end-to-end suite passes at any hour and across midnight (F154). Its
  worlds' personas are Germans and slept by Berlin's night under the
  shipped `active_hours: prefer`, so a job or a flow started at night
  waited for the morning (`tests/e2e/guard.sh`'s flow stayed `queued`
  until 05:42 UTC on a run at 02:00 UTC); every world now starts from the
  shipped policies with the defaults' `active_hours` at `ignore`
  (`tests/e2e/policies.sh`). The field runner's test gave its sites a
  window of 00:00 to 23:59 UTC and failed or hung near midnight: it closed
  the window before site 2, ran site 1 again "the same day" after
  midnight, or read the wall's cooldown as "the day goes on" and waited
  for 23:59; its sites now keep their days where it is noon. Each script
  of `just test-e2e` is bounded (`e2e_bound`, 20 minutes) and fails
  instead of holding a CI job until it is cancelled. Shown with the
  stack's clock moved by libfaketime, at 02:00 UTC and across 00:00 UTC
  (`docs/runbooks/repo.md`).
- The parity test of the mouse and keyboard measured a button's hold
  between when the page's listeners ran, and a page still busy after its
  load runs them late, by different amounts for the press and the
  release: a button held for 102 ms read as 5 ms on a loaded machine
  (F81). It is measured on the events' own `timeStamp`, which read 102 to
  108 ms in every run. The Chromium parity test of a paced flow counted
  the unpaced hand's mouse moves before 2200 ms from the recorder's start,
  which a loaded machine fails by drawing the page late and by telling it
  of a path in as few events as it drew frames; it is counted from the
  page's second frame, and the path is the one the hand reports.
- **The hand clicks a link at the very top of the window of a page at its
  end** (F164). The hand goes to an element once its middle is 12 pixels
  inside the window; one whose middle lay in the top 12 pixels was
  flicked further up with the wheel turned down, and a page at its end
  does not move for that, so after three flicks the hand gave up
  (`element_not_reachable`) and a crawl typed the address of a link that
  was there to click. It happened to personas with a tall window at the
  end of the crawl's reading room (1481 pixels high, the link's middle 5
  pixels from the top), in two of three gate runs of one session and once
  on GitHub. Such an element now comes down as the wheel turns up, and on
  a page that scrolls no further either way the hand goes to the part of
  an element that shows when at least half of it does. A miss logs where
  the element was and what the last flicks did.
- **A NUL character in a request is refused naming the member, never
  `500 internal` with the body in Postgres' log** (F112; QA round one's
  api-02, secrecy-10). Every body is checked as a document before it is
  read as a type: a NUL in a string or a name is `422 validation_failed`
  with the member as `field` (`url`, `actions[1].text`, `urls[1]`), on
  `POST /v1/fetch`, `/v1/jobs`, `/v1/sessions`, `/v1/admin/keys` and the
  rest; nothing reaches the store, so nothing of it reaches a log.
- **The self-test of a persona reads its body as every other endpoint
  does** (F113; api-03): `POST /v1/admin/personas/{id}/selftest` answers a
  body that does not come with `408`, a body too large with `413
  payload_too_large` as a problem, a body of another media type with
  `415`, and one that is not JSON with `400`. No body at all is still the
  default test.
- **The OpenAPI document names every status an operation answers**
  (F114; api-05, docs-02): `401`, `403`, `429` and `500` on every
  operation that takes a key, `400`, `408`, `415` and `422` where a body
  is taken, `413`, `414` and `431` everywhere, as shared responses
  (`#/components/responses/...`) that an operation's own description of a
  status overrides. `GET /v1/artifacts/{id}` answers `*/*` with a binary
  string, so a generated client reads bytes instead of failing on JSON;
  the TypeScript client was generated again.
- **A refusal while a body is read is `422` and names its member, its
  step and its item** (F115; api-06, api-07, jobs-08): a null or a number
  for an enumerated member (`robots`, `request_kind`, `wait.until`,
  `screenshot.format`) is no longer `400` "Failed to parse the request
  body as JSON"; `field` is the path as written (`items[1].timeout_ms`,
  `defaults.timeout_ms`, `webhook.url`, `actions[2].selector`), with
  `step` for a flow's step and `item` for a batch's item.
- **An operator's key reads and cancels any key's job, on the audit
  trail** (F117; auth-02, decided by the planner): a key with `admin`
  beside `jobs` reads and cancels another key's job as an operator's
  power, and each such read and every cancel, the owner's own included,
  writes an audit event (`job.show`, `job.items`, `job.cancel`) with the
  caller's key and the job's as `owner_key`. A key without `admin` still
  reads `404 job_not_found`. Spec §16.3 says so.
- **`/readyz` answers `503 stopping` for as long as the server stops**
  (F121; jobs-04, api-16): the listener stayed open only until the signal,
  so for the whole wind-down every request, a load balancer's included,
  met a reset connection. It now stays open while the pages that are
  being read are waited for and the requests with a key in flight end (at
  most `server.shutdown_grace` from the signal), and closes after.
- **The `method` label of the API's metrics is bounded** (F131;
  secrecy-04): a method HTTP defines, or `other`; a client without a key
  could add a series for every method it made up, 4 kB labels included.
- **Every nested configuration key can be set alone** (F132; cli-01):
  `KAGE__PERSONAS__WARMUP__VISITS__MIN=2`, or the same member alone in
  `kage.yaml`, was `missing field max` for twenty keys of that shape; the
  defaults are now the first layer of the configuration.
- **A password in a URL is refused or never shown, on every road**
  (F133; cli-03, cli-06, cli-08): `kage fetch` refuses a URL with
  credentials before anything is stored (it was kept in `requests`, the
  audit trail and the error line); the sidecars' URLs, a self-test page
  and a CORS origin refuse credentials at load without repeating them; a
  parameter of `database.url` the driver does not read is refused at load,
  naming the parameter and never its value, which the driver logged.
- **The API's small conformance items** (F134; api-09 to api-15,
  auth-01): an answer from the cache says `created_at` as a read of it
  later does; a path parameter that cannot be read (`%ff`) is `400
  invalid_request` as a problem; a head over `server.max_headers` or
  `server.max_header_bytes` is `431 headers_too_large` (a new code) as a
  problem with its request id, and a target over `server.max_uri_bytes`
  `414`, up to twice the limits, past which the HTTP layer answers alone
  (documented); a `Content-Length` over `server.max_body_bytes` is `413`
  on every endpoint, one that takes no body included; a member given twice
  is refused in a job as in a fetch; a `+json` media type is `415`; a
  `daily_page_budget` over 100 000 is `422`, not `500`, at the API and at
  `kage key create`.
- **The audit trail's gaps** (F135; auth-04, auth-06): a refusal at
  `/metrics` and `/docs` for a missing `admin` scope is on the trail
  (`admin.metrics.read`, `admin.docs.read`, `denied`), and a command of
  the CLI whose audit event cannot be written shows what it did and then
  ends non-zero, saying so.
- **What the loaders take and cannot mean is refused** (F136; auth-05,
  cli-12, extract-11, policy-06): a wildcard in
  `server.cors_allowed_origins` or in `egress.direct.private_names` (it
  matched nothing); a policy's `cache.max_ttl` or `cache.ttl` longer than
  the configuration's `cache.max_ttl`, at start, at a reload and in
  `kage config check`; and `GET /v1/policies/domains/{domain}` for a
  wildcard, an empty label or a label over 63 characters (`422`). The
  preview shows the cache lifetimes that hold.
- **The CLI and compose edges** (F137; cli-04, cli-05, cli-07, cli-09,
  cli-10, cli-11, cli-13): a list file named relative to `deploy/` without
  `./` (`KAGE_STATIC_LIST_FILE=list.txt`) is mounted as the documents say,
  where compose took it for the name of a volume and refused the project
  (the compose file mounts it in the long form); `scripts/dev-secrets.sh` no longer glues a
  new key onto a last line without its newline; `config check` refuses
  the values that made `serve` panic or run out of memory
  (`personas.retention_days`, `personas.archive_after`,
  `proxies.health.interval`, `database.max_connections`), and the archive
  sweep no longer panics on an overflowing date; `egress.direct.engines`
  outside `personas.mix` is refused; `kage fetch --output` is checked
  before a site is asked and the result is shown before the file is
  written; `--timeout` keeps the API's bounds; `null` from the
  environment switches an optional key off as in the file.
- **The items of a job show the result of an item that ended without its
  page** (F155), as `GET /v1/results/{id}` has since F109: `provider_error`
  with what ended it, where the list said `null`.
- **The doc comment of `GET /v1/results/{id}` says what is kept with a
  result** (F158): `pacing`, `behavior`, `console` and `side_effects` are
  stored with the result since F36 and read back, as spec §16.5 says; the
  comment that called them null was out of date.

- **A persona's display is its own X server's** (F80). Each engine's pool
  of display numbers knew only its own process: two pools on one host
  (two test workers, two sidecars sharing `/tmp`) gave out `:100` at the
  same moment, and the engine whose Xvfb lost either failed the start
  ("Xvfb exited with 1") or found the other's socket, called the display
  up and prepared the other's server; on a GitHub runner its `setxkbmap`
  there hung for its 10 s and the context failed `browser_launch_failed`.
  Measured locally, two such starts took the same number in 10 races of
  10. A display is now up only once Xvfb's lock names this engine's own
  server and the server answers a client, and a number another server
  holds is passed over for the next free one (20 races of 20 on two
  numbers, at most 123 ms). `setxkbmap` keeps its 10 s: under seven times
  more work than CPUs it took at most 165 ms.
- Two tests that read a slow runner as a failure: the carried fetch's
  test (`kage-api`) wanted its first page answered within the 300 ms wall
  time it sets for the second, and the test of a flow that waits for a
  site's one visitor (`kage-scheduler`, F188) measured the visit from the
  attempt's start, which comes after the moment the visit counts from.
- `tests/e2e/guard.sh` read its sites under the shipped `human` pacing, so
  a persona broke after a burst of 4 to 14 pages for a minute or more, and
  the Chromium persona's ninth page waited past the fetch's 180 s wall
  time and was answered 202. Its three sites are read at the pace of a
  test (`pacing: fast`), as the other scripts' sites are.
- **A certificate level 0 refuses, and a site's own `407` over `https`,
  are the site's answer** (F170; QA round two's tls-01, tls-03). wreq
  reports a refused handshake as a connection error, so a site whose
  certificate the plain client refused (expired, the wrong name, an
  authority it does not trust) was `proxy_failed` ("cannot connect through
  the proxy"): the proxy was blamed and probed, the site was asked again
  with every retry, and a `robots.txt` behind it was unavailable for a
  minute instead of an hour. kage-http now tells a TLS answer from a
  failed connection by the TLS library's own error: `connection_failed`
  with a server error's courtesy, and the self-test says the handshake
  failed. The forwarder notes every tunnel it opens, and an attempt the
  client named a failed proxy whose own tunnel was open is the site's:
  Firefox's proxy error for a site's `407` inside TLS is
  `navigation_failed`, as Chrome names the same page (spec §9, §14).
- **A `robots.txt` with a NUL byte, or saved as UTF-16, is read and kept
  once** (F171; QA round two's http-01). Its text went to Postgres as it
  came, which refused the NUL, so every level 0 fetch of the origin was
  `500 internal` and asked the site for the file again, and a level 2
  fetch `robots_unavailable`. The file is now read as UTF-8 or in the
  encoding its byte order mark names, with a NUL left out, so a file saved
  as UTF-16 with or without its mark reads as the site wrote it (spec §14).
- **Level 0 reads a page's encoding as the browsers do** (F172, F160; QA
  round two's http-06, http-05). A `<meta charset>` written inside an
  inline script was taken for the page's, so a windows-1252 page read as
  UTF-8, with replacement characters in its text and links no page names;
  a `<meta>` past the first 1024 bytes of a long head was not seen; an
  undeclared page in Shift_JIS, GBK, EUC-KR or another legacy encoding was
  read as windows-1252, and an undeclared UTF-8 page as UTF-8 where both
  browsers show mojibake. The scan for a `<meta>` now reads the markup as
  Chrome's does, token by token (scripts, styles, titles, comments are
  text), and on while the head lasts; an undeclared page goes to
  Firefox's own detector (chardetng 1.0.0, pinned), never allowed to
  answer UTF-8, with the site's top-level domain as its hint (spec §9).
- **A sitemap in a declared legacy encoding names the addresses it names**
  (F172; QA round two's http-04). The sitemap reader took every file for
  UTF-8, so `/caf\xE9` in an ISO-8859-1 file became `/caf%EF%BF%BD`; it
  now reads the file in the encoding of its byte order mark, its
  `Content-Type` or its XML declaration, else UTF-8 (spec §9, §13).
- **A link's query is written in its page's encoding, and the subresource
  emulation reads the page it fetched as decoded** (F161). At level 0 a
  link `?q=für` on a windows-1252 page asked for `?q=f%C3%BCr` where a
  browser asks for `?q=f%FCr`; the query is now encoded in the document's
  encoding as the URL standard has it, by the crawl's links, the
  extraction's and the assets of level 1, and the assets are read from the
  decoded page instead of its bytes as UTF-8 (spec §9).
- **Level 1's subresource emulation resolves an asset's name before it
  asks for it under `security.resolve_on_host`** (F148), as the page's own
  redirect hops are since F98: an asset whose name points into a denied
  range is refused and never asked for (spec §9, §19).

- **A robots.txt rule matches a path written with escaped unreserved
  characters** (F179; QA round two's http-03). Rules were compared with
  percent-escapes as written, as Google's matcher does, so under
  `robots: enforce` `/%70rivate` was read although `Disallow: /private`
  refuses it, and a rule written `/%62az` did not protect `/baz`. An escape
  of an unreserved character (a letter, a digit, `-`, `.`, `_`, `~`) is
  now that character in the rule and in the address, as RFC 9309 §2.2.2
  says; an escape of anything else (`%2F`) stays one (spec §14).

### Added

- **An operator trust anchor for the http path: `http.ca_file`** (F191). A
  PEM file of certificate authorities, none by default, that the persona's
  http clients, the plain client of level 0 and the proxy provider's API
  client trust besides the public ones, so that level 0 reaches a site
  under a private authority. It is for an authority of the operator's own,
  never a public one's root someone else holds the key of
  (`docs/runbooks/levels-and-egress.md`). `kage config check` and the
  server's start refuse a file that cannot be read or holds no
  certificate. QA round two's TLS world tests level 0 over `https` with it
  (`tests/qa/infra/tls/level0.sh`).

## [2.0.2] - 2026-11-04

### Added

- **The field campaign's runner** (`tests/field/`, `just field <index>`):
  drives one site's batch of a day through the API of a running stack and
  records it from the store, for a round a session runs on the owner's
  typed approval of the list (`docs/runbooks/field-campaign.md`, "A
  session runs it"). It deals each country's sites to its personas by a
  seeded draw, writes the round's policy entries into a private
  directory, pins every batch to the site's persona through a session (so
  a block is never retried by another persona and no persona reads two
  sites), runs one crawl at level 3 with a deny list for sign-in, account,
  basket, checkout and search pages, cancels it at a block that cools the
  persona past the day, at the end of the site's window or at a stop of
  the pool or the bandwidth, and writes a private record and an
  anonymised section that is refused when it would name a site, an
  address, an id or a token. Its library is tested with node's runner in
  `just test-scripts`; `tests/e2e/field.sh` runs it against the fixture
  site in `just test-e2e`. `tests/field/compose.field.yaml` is the
  campaign stack's overlay: the private policies, no automatic
  replacement, `Claude-User` among `robots.agents`, and the warm-up
  without Wikipedia's `Special:Random`.
- **The field campaign's first report**, `docs/field-reports/2026-10-29.md`:
  twenty sites read at level 3 by one persona each, the vendor found, the
  outcome rates per site, engine and pacing, the consent dialogs against
  the reader's clicks, the warm-up, and what the data says of the plan's
  open items. It names no site.
- **The `patient` pacing profile** in `policies/pacing-profiles.yaml`:
  about twice `human`'s gaps and rests, shorter bursts, longer breaks, one
  visitor at a time; a profile to choose, not the default.
- **`akamai-reference-error`**, a block signature for a denial the Akamai
  edge serves under a site's own branding, known by its
  `x-reference-error` header, with a synthetic fixture.

### Fixed

- **The scheduler keeps a site to its seats, waits out an outage of the
  pool, blames a dead proxy on the proxy, and a cancel reaches what waits
  (F106 to F109).** QA round one found four ways the scheduler and the
  job worker broke their own rules. Synchronous fetches that waited
  together recruited every persona of the pool in turn, past
  `domain_concurrency` and long before `visitor_wait` (sched-01): a
  synchronous request now goes past the seats only for a visitor's break
  that does not end within `visitor_wait`, and waits for a visitor in its
  gap however many wait. A batch sent while every proxy was in backoff
  failed its items with `no_persona` within two seconds (sched-04): work
  that a persona could take once one is ready is refused `no_persona`
  with `retry_at` a minute on, so a job is `paused` and resumes when the
  proxies return. A dead proxy was the site's server error: a browser
  reports a plain `http://` page whose proxy is down as
  `navigation_failed`, and the site was paused 30 s doubling for everyone
  (sched-05); the forwarder now notes, by caller and target, each
  connection whose proxy could not be reached, did not answer or refused
  its credential, an attempt that got no answer for its own page meanwhile
  is `proxy_failed` and counts nothing against the site (a proxy's `502`
  for an unreachable target, and a third party's host, still do), the
  persona leaves its visit and for five minutes is set aside for one that
  could start before then, and its proxy is probed at once. A
  cancel did not reach a fetch carried on as a job, nor a job queued
  behind `jobs.max_running`, and the request of an item that ended
  without its page answered `202 running` for ever (sched-07, jobs-07,
  jobs-03, auth-03): the carried fetch is taken out of the queue at once,
  a cancelled job nobody works is claimed past the limit and ended, every
  item that ends without its page ends its request, and
  `GET /v1/results/{id}` answers `200` with what ended a request whose
  job ended without it.

- **Level 0 reads robots.txt and sitemaps, decodes a page as a browser
  does, and a host has one name** (F99, F100, F104; QA round one's api-01,
  crawl-02, crawl-03, ssrf-05, extract-03, crawl-04, policy-01, ssrf-04).
  A `robots.txt` first asked for at level 0 was kept empty for a day, so
  `robots: enforce` let every page of the origin through at every level,
  and a level 0 crawl read no sitemap (`sitemap_unreadable`): the scout's
  answer and the crawl's sitemap reader looked only at a browser's page,
  and the plain client's body was dropped. Both now read the body the site
  sent at every level (`AttemptOutput::sent_body`), and the plain client
  keeps an enforced file's `Crawl-delay` between two pages of the host as
  a persona does. The http path read every body as UTF-8, so a page in
  ISO-8859-1 or Shift_JIS came back as replacement characters with
  `outcome: ok`, and a crawl asked for addresses no page names: it now
  reads a body in the encoding a browser takes for it, by the Encoding
  Standard (its byte order mark, the `charset` of `Content-Type`, its
  `<meta>` by the HTML standard's prescan, then UTF-8 or windows-1252;
  `kage_http::charset`, with `encoding_rs` 0.8.42, already in the
  lockfile, as a direct dependency), for the classifier, the extraction,
  a crawl's links and text, and `raw_body`, which is now that text where
  it was null. A host with one trailing dot was another site to the
  breaker, the cooldowns, the pace and the plain exit, and one with two
  (`http://localhost../`, `http://127.0.0.1../`) passed the SSRF guard on
  every road: `kage_core::urlnorm::normal_host` is the host's one normal
  form, one dot dropped where a request arrives and in every key, and the
  guard refuses a name with an empty label. The empty `robots_cache` rows
  a deployment that ran 2.0.1 with `enforce` and level 0 may still hold
  are cleared by the upgrade (below).

- **The upgrade clears the empty `robots.txt` files 2.0.1 kept (F99).**
  Migration `0019_robots_cache_empty_files` deletes, once, every
  `robots_cache` row of the shape the bug wrote (a `2xx` status, an empty
  body, not unreachable), so the site's scout asks for the file again when
  a request needs it; a real file, a `404` and an unreachable answer stay.
  An operator no longer clears the table by hand or waits a day.

- **Flows meet every site's policy, one `ssrf_denied` for a refused
  target, a page cannot stop a sidecar, and the self-test of a proxy reads
  its pages (F101, F102, F103, F105).** QA round one found four promises
  of the browser path broken. Every `goto` of a flow now meets the policy
  of its own site as a fetch of that address would: its `min_level` at
  acceptance, its enforced `robots.txt`, its breaker, pacing and hours and
  the persona's state there; a `human: false` step is judged by the site of
  its page, a wall a flow meets at a later site is booked to that site, and
  a later site's breaker trial ends with the flow (spec §10.5). A page that
  moves itself, is redirected, or is left by a flow's click or way back for
  a denied address or scheme, in either engine, and a name `egress.mode: direct`
  refuses for its address, at every level, end `ssrf_denied`, never retried
  and nothing kept, instead of `ok` on Chrome's error page or a retried
  `provider_error` (spec §19). Every read of a page is bounded (a page
  that stops answering ends as a `timeout` of its own, never as an empty
  page that cools the persona, and a screenshot or PDF the page never
  paints leaves no artifact and the same typed error), the sidecar's
  server bounds a navigation and a flow within Kage's own wait and fails
  its health check while one does not end, and a watchdog ends a sidecar whose event
  loop stopped answering (`SIDECAR_STALL_MS`, 30 s) so that compose starts
  it again: a page whose depth crashes Chrome's renderer and wedged the
  Chromium sidecar for good now costs one restart (spec §10.6; the cure of
  the crash is F146's). `kage selftest browser --proxy` hands its browser
  a forwarder of the command's own, so it reads its pages on the shipped
  compose and the provider's credentials never leave kage, and a run that
  read none of its pages says `inconclusive` and exits non-zero (spec
  §7.4, §7.5). A flow's later sites are named in the host's normal form
  as a request's is (F104): `pay.example.` is the site `pay.example`, its
  breaker, file and walls included.

- **`KageWebhookFailing`, `KageBlockRateAboveThreshold` and
  `KagePoolUnavailable` fire on what they are for.** QA round one found the
  first two one event late and the third never: a Prometheus counter is
  born with its first sample, which `increase()` never counts, so the
  first webhook failure and the first block of a domain were lost to the
  alerts and to every panel that counts events (secrecy-01); and every
  expiry of a proxy's backoff showed that proxy `degraded` for a scrape,
  which broke the pool alert's condition and started its ten minutes
  again, so that it never fired in eighteen minutes with every proxy down
  (secrecy-02). Every counter whose label values are known before any
  event now exists at zero from the start of the process (webhook
  deliveries, jobs and items, the crawl's pages, skips and duplicates, the
  API's and the scheduler's refusals, bandwidth, cache, `robots.txt`,
  replacements), so its first event is a change; the block-rate alert and
  the block panels read `kage_attempts_total`, whose domain arrives with
  the event, against its value an hour earlier, so the first block counts
  and a restart counts as zero. `kage_proxy_state` counts a proxy whose
  backoff ran out as `backoff` until a probe passes, and is set at every
  change of state, not only at the pool's maintenance pass. The rule tests
  start their series where the exposition starts, with the two findings
  as cases; the observability end-to-end test shows a first webhook failure
  firing its alert and the pool reading unavailable across a backoff's
  expiry (F110, F111).
- **A redirect hop of the http path at level 0 is resolved on the host
  under `security.resolve_on_host`, as the documents said it was.** The
  plain client of level 0 was built without the factory's resolver, so
  with the setting on a public page's 302 to a name that resolves into a
  private network was followed through the proxy and the hop's body
  returned to the caller, while the same name asked for directly was `422
  ssrf_denied` (QA round one, ssrf-01). Both clients of the http path now
  run one address check before every request they send: a hop with any
  address in a denied range, or with none, ends the fetch as `ssrf_denied`
  with nothing of the hop requested, returned or kept, and a name the host
  cannot look up counts as one with no address, as it does at acceptance
  (the persona's client gave such a hop an `http_client` failure before).
  The persona's client under `kage serve` already checked its hops. With
  the setting off nothing changes: the proxies resolve (F98).
- **A page nested thousands of elements deep no longer ends the process.**
  QA round one (finding extract-01) fetched a page of 5000 nested `<div>`
  with `include: ["extract"]`, and the kage process died of a stack
  overflow: every fetch, job claim and webhook delivery in flight with it,
  `/readyz` down until the container restarted, and the request without a
  result. A crawl at level 0 whose seed linked to such a page restarted the
  server three times and then said `succeeded`; a batch item said `ok` with
  `extract: null`. The walk that reads a page's links and its text for a
  crawl and for `links` of an extraction called itself once per level of
  nesting, so a page chose how deep the worker's 2 MiB stack was used. It
  is a loop over the tree now, with a stack of its own, like the parser
  and every other walk, measurement and rendering of the extractor
  already were; nothing on a page's path calls itself per level, and the
  worker's stack is unchanged. The result is whole at any depth: the
  finding's page gives its title, its hundred words and its links. Tests at
  each layer read pages of 5000 and 20 000 nested elements on a thread
  with a worker's stack, and the fixture site serves a page whose script
  nests ten thousand elements, which the end-to-end test reads through
  Firefox at level 2, as the finding did (F97). Chromium's sidecar does not
  answer the navigation of such a page in time, even at 2000 elements, and
  the fetch ends `sidecar_error`: a finding of its own, not fixed here.
- **A persona's tab holds no page of another request (F96, QA finding
  jobs-01).** A flow's `back` walked into the page the request before had
  left in the persona's tab, another key's as much as the same key's, and
  both engines reported the step `ok: true`; on a persona's first flow
  Chromium went back to `about:blank` with `ok: true`. Every navigation,
  flow and self-test now starts in a fresh tab of the persona's window,
  which holds no page, no history and no session storage of what came
  before (cookies and local storage are the profile's and stay, D6), and
  a way back never lands on the blank page a tab begins with or on the
  browser's error page: `back` at a flow's start fails as §10.5 says, on
  both engines, on the first request and on every later one, and a step
  that reads the tab before the flow's own first navigation reads a blank
  one. Only a crawl's pages share a tab, which Kage names with the crawl's
  job (contract 1.7.0, `NavigateRequest.tab`): its next page is still a
  click on the page before (§13, F28). No API consumer can name a tab.
- **The browser self-test loads a detection page once more when its
  navigation failed.** The first nightly live run (2026-10-30) lost its
  verdict on one page: the Firefox self-test through the second of its two
  proxies got `page.goto: Timeout 45000ms exceeded` on bot.sannysoft.com,
  the page counted as `unreachable`, and the live check refused a verdict
  that would rest on fewer checks, while the other three self-tests of the
  run scored 100 of 100. The sidecar now loads a page whose navigation
  ended in a timeout, a proxy or a network error a second time after a
  fixed pause of five seconds, through the same context, proxy and
  persona, and judges it by that load; a page that fails twice stays
  `unreachable` as before, and a page that arrived is never loaded again,
  whatever its checks say. The page report says how many loads a page took
  (`attempts`, contract 1.6.0, additive), the stored run and the persona's
  self-test event carry it (`retried`), `kage selftest browser` and `kage
  persona selftest` print it, and the live check prints the attempt a page
  was loaded on; its rule that a verdict rests on all checks is unchanged.
  Kage waits for a self-test as long as its pages may take with their
  second loads, not as long as every sidecar call may. The runbooks say
  that a detection page may see an address twice in one run (F95).
- **A page whose head outgrows a mebibyte is no longer judged blank.** The
  block signatures read only the first `settings.max_body_bytes` of a
  captured document (1 MiB); a page whose scripts grow its head past that
  before the visit ends left the classifier nothing but head to read, and
  the `soft-blank-page` rule judged a page full of text a soft block (seen
  in the field campaign). The shipped `policies/block-signatures.yaml`
  reads 16 MiB, more than a sidecar captures. A rule that counts text on a
  cut body abstaining is follow-up F88.
- The README's CI badge said failing while every gate passed. The
  report-only `mutants` job ran into its 30-minute timeout on every run; a
  job stopped by its timeout is cancelled even with `continue-on-error`,
  so every run on `main` ended cancelled, which the badge shows as
  failing. The job now stops its own run after 26 minutes and ends green,
  with the same artifact (`docs/runbooks/repo.md`, the jobs table).

### Changed

- `docs/runbooks/field-campaign.md`: a session the owner starts may run a
  round on the owner's typed approval of the list; a committed report
  names a site only as "Site N: <country> <category>, <vendor>".
- The end-to-end world (`deploy/compose.e2e.yaml`) answers to a second
  registrable name, `wall.field.test`, for the field runner's test.
- The workspace's `authors` and the OpenAPI document's contact name the
  maintainer, `serpe-nt`; `SECURITY.md` promises an answer within two
  weeks (it said one) and names its second section "What is not in
  scope".
- `.github/allowed_signers` names the tagger by the maintainer's GitHub
  noreply address, the address every commit and tag is made under from
  now on; the tags up to v2.0.1 verify against the list at their own
  commit (`docs/runbooks/repo.md`, Releases).

### Security

- `fast-uri` 3.1.9 and 4.2.3 in the sidecar and 3.1.9 in the API test
  client, by overrides, for four advisories: GHSA-w74x-v6f9-j3xg and
  GHSA-wjr9-j795-jxcr (moderate), GHSA-333x-w4r6-6crp and
  GHSA-xhq7-xmqj-769h (low). 2.0.0 and 2.0.1 ship 3.1.8 and 4.2.1 in the
  sidecar, and their changelogs do not name them. There `fast-uri` is the
  URI resolver of the schema compilers under fastify: it resolves the
  `$id` and `$ref` of the sidecar's own schemas, never a URL a caller
  sends or a page holds. The API client is test tooling and is in no image.
- `minimatch` 11.0.1 and `glob` 14.0.2 in the sidecar, by overrides, for
  GHSA-g3hv-75wg-m249 (moderate: a pattern an attacker supplies makes the
  matcher's preprocessing grow and its second pass quadratic, at
  optimization level 2) and GHSA-3g3m-2fjr-6pw4 (moderate: a walk of glob
  that was aborted kept reading directories after it had rejected). 2.0.0
  and 2.0.1 ship 10.2.6 and 13.0.6. Neither is on a request's path:
  minimatch is the file matcher of eslint and typescript-eslint, in no
  image, and glob is a dependency camoufox-js declares and its shipped
  code never imports, so the Firefox image carries the package and
  nothing loads it. The API test client
  keeps `minimatch` 5.1.9 under `@redocly/openapi-core` 1.34.20, which
  pins it and requires the module as a function, a shape minimatch 9 and
  later do not export, so its alert stays open: the one call is Redocly's
  header matching for a `$ref` fetched over HTTP, which the client never
  does, at the default optimization level, where the advisory's path is
  not reached.
- `utoipa-swagger-ui` 10.0.2 and `zip` 9.1.3, for five advisories against
  `zip` 8.6.0: GHSA-m665-v7pm-7grj (critical), GHSA-9hj8-73fh-9963,
  GHSA-hjqx-j4rw-g7xv and GHSA-q7v9-x93c-xqv3 (high), GHSA-8hxf-qr8p-62x4
  (moderate). `zip` never touched user data: it is a build dependency, and
  its one use is the build script of `utoipa-swagger-ui` unpacking the
  vendored Swagger UI bundle, checksummed in `Cargo.lock`, at compile time.
  It is not in the binary. The accepted exception for RUSTSEC-2026-0367,
  -0368 and -0369 is gone with it, and the Swagger UI bundle is unchanged.

## [2.0.1] - 2026-10-29

### Security

- `zstd` 0.14.3 and `zstd-safe` 8.2.0 under the response decoders of the
  HTTP path (level 0, and the proxy probes), which read what a site sends.
  In 2.0.0 and every 1.x, a site that answered with a corrupt legacy zstd
  frame could make a decoded body carry up to 32 KiB of stale memory of the
  Kage process (RUSTSEC-2026-0403: the decoders stream through the affected
  API, and legacy frames are compiled in); RUSTSEC-2026-0391 and -0398 are
  about APIs the decoders do not use. `zstd-sys` 2.1.2 (RUSTSEC-2026-0410,
  WebAssembly only). The older `zstd` 0.13.3 that `wreq-util` 0.2.0 uses to
  decompress TLS certificates has no fixed release; it makes a fresh read
  decoder for each certificate, which none of the three affects, and the
  advisories are accepted for it until 2026-12-31 or a `wreq-util` on
  `zstd` 0.14 (`docs/security-review.md`).

## [2.0.0] - 2026-10-27

Camouflage levels and egress (spec §25, ADR 0012, decision D50), a
provider-agnostic pool with declared lists (spec §7.1, ADR 0013, decision
D51), and the public face of decision D52. **Drain the job queues before
upgrading**, and **give every entry of a static list its country** (below):
an item of 1.x still queued names no level and fails at take-up with
`level_required`; nothing is decided for the caller.
`docs/runbooks/levels-and-egress.md`, "Upgrading from 1.x", is the whole
upgrade.

### Breaking

- **Every request names `level`**, 0 to 3, and a request without one is
  refused with `422 level_required`: `POST /v1/fetch`, every item of a
  batch (its own or `defaults.level`), a crawl's `defaults.level`, a
  flow's `level` (2 or 3), and `kage fetch --level`. There is no default.
  0 is a plain HTTP client that says it is Kage (`User-Agent:
  kage/<version>` unless the request names one), with no persona; 1 a
  persona's http client (what `render: http` was; refused with
  `level_unavailable` while no browser version of the installation has a
  verified profile, which is the case for the shipped Chrome 155 and
  Camoufox 156); 2 a persona's browser that loads the page, sends no input
  and leaves; 3 a persona's browser as a person (what 1.x did in a
  browser). Below 3 Kage is not pretending to be a person.
- **`render` is gone**: from requests (an unknown field now), from `kage
  fetch` (`--render` is refused; `--level` is required), from the domain
  policy file (a file that still has `render`, including the 1.x default
  `render: auto`, is refused with a message that names `min_level`: remove
  the key), from the cache key, and from the labels of
  `kage_requests_total`, `kage_attempts_total` and
  `kage_attempt_duration_seconds`, which carry `level` (`"0"` to `"3"`).
  The shipped dashboards and alert tests follow, and the block-rate alert
  counts levels 1 to 3 only: a block at level 0 is no breaker's.
- **Nothing escalates.** A block on the http path is never retried in a
  browser: the level is the request's ceiling. Results no longer have
  `escalated`; a block signature with `escalate` is refused.
- **Error codes renamed**: `render_needs_http` → `level_needs_http`,
  `render_kind_conflict` → `level_kind_conflict`, `render_include_conflict`
  → `level_include_conflict`. New: `level_required`, `level_below_policy`
  (a site's `min_level`), `level_unavailable`, `session_needs_persona` (a
  session at level 0).
- The response cache's key is at version 2 (the level replaces the
  render): no entry of 1.x is served after the upgrade.
- An attempt's `persona` and `engine` are null at level 0, which says its
  `exit` (`host` or `pool`) instead; `attempts.persona_id` and
  `attempts.engine` became nullable in the store (migration 0017).
- The policy preview says `min_level` in place of `first_render`.
- **A static list's country is a declaration** (ADR 0013): the persona of
  an entry follows the country, locale and zone the entry declares, and a
  geo lookup only verifies them (`proxy_geo_mismatch` after two that
  disagree, the persona's identity kept). In 1.x the lookup's country won.
  At the first 2.0 start a persona whose lookup had placed it in another
  country or language than its entry declares is made again
  (`declaration_changed`), and **an entry without a country loses its
  persona** (`undeclared`) and serves as a plain exit for level 0 only.
  Add a country to every entry that should carry a persona.
- A declared country the browser tables do not know (they know AT, BE,
  CA, CH, DE, ES, FR, GB, IE, IT, LU, NL, PL, PT, RO, SE, US) is refused at
  load unless the entry also declares a locale in a language they know;
  1.x gave such a persona `en-US, en`. The same rule holds for
  `egress.direct.country`.
- **The probes of a static list read the public echo**
  `https://checkip.amazonaws.com/` and the host's own address
  `https://api.ipify.org/`, where 1.x read Webshare's echo whatever the
  vendor; `proxies.health.echo_url` and `host_ip_url` are unset by default
  and resolve by provider (Webshare's own echo for Webshare). Set them to
  keep 1.x's endpoints, or to your own echo.
- Entries of a static list that share a host and port (a gateway's
  sessions) need an `id` each; their default ids collide and the list is
  refused.
- The `ProxyProvider` trait has `capabilities()`, and `replace`,
  `replace_status`, `replaced`, `account` and `stats` answer
  `Unsupported` by default; the static list now answers `Unsupported` for
  all of them (in 1.x it returned an empty log, an account and no stats).

### Added

- `level` on every result, and the still visit of level 2: the dwell is
  the site policy's `min_dwell` drawn upward (median one and a half times,
  95 % under three times), with no scrolling, no wandering pointer and no
  patience. Sidecar contract 1.5.0 (additive): `behavior.mode` (`human`,
  `still`) and a flow's `pace.wander`.
- Level 0, the plain client: wreq with its own TLS and HTTP/2, a minimal
  header set in a fixed order (recorded as
  `tests/fixtures/fingerprints/plain.json` and compared in the gate), the
  caller's headers and body as given (`Cookie` and `Authorization` too:
  sent to the request's origin only, never stored or logged), cookies a
  redirect chain sets kept for that chain only. It waits for the site's
  own limits (concurrency, requests per minute, the breaker's pause, the
  pause after server errors) and nothing of a persona's; a block is the
  answer at once and cools nobody. Under the pool it leaves through the
  site's scout's address and no other (`plain_exit_unavailable` while that
  proxy cannot carry traffic).
- `min_level` in the domain policy (0 by default); the shipped file sets 3
  for the detection pages (`bot.sannysoft.com`,
  `abrahamjuliot.github.io`, `browserscan.net`, `pixelscan.net`,
  `browserleaks.com`, `fingerprint.com`, `demo.fingerprint.com`).
- `egress.mode: direct`: Kage fetches from the host's own connection, with
  host personas (`egress.direct.personas`, 1 to 4) placed by the host's own
  geo lookup or by `egress.direct.country` and `egress.direct.locale`, on
  the engines `egress.direct.engines` names; the forwarder resolves and
  checks every target itself (`egress.direct.private_names` for names the
  operator allows to be private). Nothing leaves through the pool then,
  and under `egress.mode: proxies` nothing leaves from the host's address:
  `kage config check` and the start refuse both crossings, and the check
  reports `egress` with its warnings. `egress.plain_user_agent` sets the
  plain client's name.
- The bandwidth is counted by exit (`kage_bandwidth_bytes_total{egress}`,
  `bandwidth_usage.egress`); only the pool's counts against the
  provider's budget. `kage_jobs_accepted_total` and `kage_job_items_total`
  carry `level`.
- **Declared lists** (ADR 0013, M15): an entry declares `country`, and may
  declare `locale` (BCP 47, overriding the country's language) and
  `timezone` (IANA), with `city` informational; the YAML form also takes a
  proxy as `url: http://user:pass@host:port`, and the line form a sixth
  field, `host:port:username:password[:country[:locale]]`. A list given as
  `KAGE__PROXIES__STATIC_LIST_FILE` is read again at every sync; an edit
  that does not parse keeps the last good list (`static_list_invalid`). A
  corrected country or language makes a new persona; a corrected zone moves
  the one there is. `egress.direct.timezone` declares the host personas'
  zone by the same rule.
- **Plain exits**: a declared entry without a country carries no persona
  and is level 0's preferred exit under the pool, one per site, kept
  (`site_exits`, migration 0018; the declared `locale` and `timezone` on
  `proxies` too).
- **Provider capabilities**: `kage proxy list`, `kage proxy health`,
  `GET /v1/admin/pool` (`provider`) and `kage config check` say what the
  provider does not offer; a proxy due for a replacement its provider
  cannot make stays in backoff with one `replacement_unsupported` event.
  The admin proxy view and `kage proxy list --json` show `locale`,
  `timezone`, `declared`, `carries_persona` and `plain_exit`.
- **The provider conformance suite**
  (`crates/kage-proxy/tests/provider_conformance.rs`) and the provider
  guide in `CONTRIBUTING.md`.
- `kage proxy health` ends with how many proxies are live and how many
  dead (`4 checked: 3 live, 1 dead`; `live` and `dead` in its JSON), then
  the pool's line on a line of its own (`pool: …`). `kage config check` reports the
  health endpoints in force, the list's plain exits, and the
  `proxies.health` keys unused under `egress.mode: direct`.
- A proxy whose probe sees an older proxy's exit address carries nothing
  until it leaves by its own (`proxy_exit_shared`).
- The probe reads Kage's own echo (`kage echo serve`, its `peer`) and an
  address written with its port, so the operator's echo can be
  `proxies.health.echo_url`.
- **The public face** (decision D52): the README rewritten for 2.0 (what
  Kage is, who it is for, responsible use for the levels, the quick start,
  the levels and the egress modes in plain words, the three ways to run
  the pool, what a result holds, what is measured and the honest limits);
  `SECURITY.md` (the private advisory form, the latest release line only,
  what is and is not in scope); `CODE_OF_CONDUCT.md` (the Contributor
  Covenant 2.1); issue forms for a bug, a feature and a proxy provider;
  a pull-request template that is the definition of done; `CODEOWNERS`
  naming the release's files and the public ones; `CONTRIBUTING.md` with
  every `just` target and the definition of done.
- **Badges from the repository's own data**: CI's `coverage-badge` job
  writes the Rust workspace's line coverage, as the gate counts it, to the
  `gh-pages` branch on every push to `main` (one file, one commit, with the
  job's own token and `contents: write` alone), which shields.io draws;
  `just lint-docs` holds the toolchain and licence badges to
  `rust-toolchain.toml` and `Cargo.toml`; `just test-scripts` runs the
  gate scripts' own tests (`docs/runbooks/repo.md`, "Badges").
- `deploy/compose.yaml` takes where requests leave from and a declared list
  from `deploy/.env`: `KAGE_EGRESS_MODE` (`proxies` or `direct`) and
  `KAGE_STATIC_LIST_FILE`, a file the `kage` service mounts read-only.
- `nightly-live.yaml` runs by hand the `suite` it is given (`load`, the
  default, needing no secret; `live`; `both`), so that a release's load
  test no longer starts the live job.
- `tests/live/host-persona.sh` (`KAGE_LIVE=1 KAGE_LIVE_HOST=1`, never in
  the nightly run): the host persona of each engine under
  `egress.mode: direct`, placed by the lookup of the machine's own address
  and judged on the two detection pages with the engines' baselines.

### Fixed

- The proxy pool wrote a proxy and the health event that records it in
  two transactions (a new proxy and `proxy_added`, a probe's state change
  and its `proxy_state_changed` and `proxy_geo_observed`, a retirement, a
  replacement requested or finished), so a reader in between saw the
  proxy or its new state without the event; a slow runner's CLI test read
  a `healthy` proxy and no `proxy_added` (F65). The row and its events are
  now one write.
- Under `egress.mode: direct` an operator's `kage proxy health` and the
  admin probe still probed the idle pool; both refuse now ("the pool is
  idle", F68).
- The hand found a link it had to scroll to while the page still glided
  from the wheel, and aimed where the link was, not where it came to rest:
  a page that glides for longer than the hand's pause after a flick (a
  slow or loaded host) moves the link from under the pointer, and after
  two tries the hand gives up and a crawl types the address it meant to
  click (seen once in four gate runs of M14, F66). After the wheel the
  hand now goes to an element only once the page rests, and a link it
  does not come to rest on is logged with the reason.
- A proxy that never answered a probe got a persona when its backoff
  ended, because `degraded` is usable: a generation spent and a profile
  made for an address nothing was ever seen through (F71). A persona is
  made only for a proxy that has answered at least once; the host's own
  rows, which are never probed, always count as answered.
- `kage selftest browser --language` refused a Firefox locale without a
  region with a line break and a run of spaces in the middle of its
  message (F75).
- The OpenAPI document had an attempt's `status`, `persona`, `engine`,
  `exit` and `reason` as optional, though every attempt carries them,
  null where they do not apply (level 0 has no persona, a persona's
  attempt no exit). They are required and nullable now, so a client
  generated from the document may rely on them (F69); only `seed`, which
  an admin key alone sees, may be absent.
- Under `egress.mode: direct`, `kage persona selftest` and `kage selftest
  browser --persona` refused every host persona: the command's own
  forwarder took the pool's routes, which carry no host row, so the
  browser's self-test could reach no page. A command's forwarder now takes
  the server's routes, and the direct end-to-end test runs the host
  persona's self-test through it.
- An empty `KAGE__…_FILE` variable was read as a file named by nothing and
  stopped the start; it now counts as not set, like an empty secret
  variable, which is how compose passes an optional one.
- Since 1.0.0 every warm-up visit was judged a soft block: the visit
  captured no document, and the classifier read the page it was served as
  an empty body (F85). Each visit was logged as a warning, cooled its
  persona for the warm-up site and counted toward that site's breaker,
  and its proxy got no credit for the traffic; the visit itself, and the
  persona's warm-up, still counted. A warm-up visit now captures the
  document for the classifier alone, as every page a browser reads, and
  is judged on it; nothing of it is stored or logged.

### Security

- `rustls-webpki` 0.103.16 (RUSTSEC-2026-0382: certificate verification
  could accept distrusted intermediates), `webpki-roots` and
  `webpki-root-certs` 1.0.10 (RUSTSEC-2026-0381 and -0383: the bundled roots
  trusted distrusted emSign intermediates). They decide which certificates
  Kage's own clients accept (the roots wreq and the self-test client trust,
  rustls' verification of the database's and the echo's); nothing a target
  sees moves, and the recorded fingerprints are unchanged.
- `ctutils` 0.4.4 (RUSTSEC-2026-0385: mask generation and `u128`
  comparisons that were to be constant-time could branch), under the
  hashes, HMACs and the AEAD that seal credentials and sign tokens and
  webhooks.
- `tokio` 1.53.3 (RUSTSEC-2026-0386: `AsyncFd::new` and its siblings are
  unsound). Kage calls none of them.
- The sidecar: `zod` 4.6.6 (GHSA-mrhx-c7xc-m3w5: validating an oversized
  constrained collection could exhaust memory; the contract document is
  unchanged) and `fastify` 5.12.6 (GHSA-389g-gv7q-r4x8: a header
  validation bypass through case-sensitive `propertyNames`).

## [1.0.2] - 2026-10-19

### Fixed

- **Every Chromium persona of 1.0.0 and 1.0.1 showed a page three things a
  stock Chrome 155 does not**, all from the driver's default switches
  (patchright's, Playwright's `chromiumSwitches`), measured against a
  Chrome nobody configured: a bar over the page on every start ("You are
  using an unsupported command-line flag:
  `--disable-blink-features=AutomationControlled`"), which made the
  viewport 56 px shorter than the window's; third-party storage not
  partitioned (a cross-site frame read its site's first-party
  `localStorage` and IndexedDB); and no HTTPS upgrade (an `http` address of
  a public host went to http directly, where Chrome asks it on 443 first
  and falls back by itself). The engine now names every switch Chrome is
  started with and leaves the rest to Chrome (ADR 0014): it keeps 12 of the
  driver's 25 for the driver, the hand or the container, leaves 13 out
  (`ThirdPartyStoragePartitioning`, `HttpsUpgrades`, `PaintHolding`,
  `Translate` and the other disabled features, the background throttling
  switches, `--force-color-profile=srgb` among them), and adds
  `--test-type`, which suppresses the bar and changed nothing a page reads.
  Each persona moves at its next context open. The wire is unchanged (the
  capture against the echo equals the committed one).
- The Chromium parity test of a paced flow failed on a slow host (0.6 of a
  CPU, as on GitHub's runners): the unpaced click came after the paced
  floor because the page drew its first frames 0.9 to 1.3 s after `load`,
  and the hand waits for them. The input recorder notes those frames, and
  the click is held to the floor from there.

### Changed

- Known tells kept on purpose and written down
  (`docs/runbooks/engine-chromium.md`, security review T5 to T7): the
  built-in AI APIs answer `unavailable` (`OptimizationHints` stays off, so
  a page cannot make a persona download a model through its proxy), Google's
  properties see no variations seed and no `X-Client-Data`
  (`--disable-background-networking` stays), Cast discovery stays off, and
  WebGL from llvmpipe is reached past Chrome's own blocklist. A result of a
  navigation that Chrome upgraded and that fell back to http shows Chrome's
  internal `307` from the https address as the one hop of its chain.
- The live browser check measures Chromium's idle traffic in two windows
  of its own, with Kage's switches and without
  `--disable-background-networking`, each with the bytes per persona-hour
  and the hosts asked for (about 89 kB and 139 kB, measured on
  2026-10-19), and the Chromium baseline
  (`tests/fixtures/selftest/chromium-155-live.json`) is recorded anew with
  the new switches: 100 of 100, every check as on 2026-10-09, CreepJS's
  "like headless" 38 % (44 % before).

## [1.0.1] - 2026-10-19

### Changed

- The Firefox engine runs Camoufox **156.0.1-beta.39** (Firefox 156.0.1),
  pinned by URL and checksum with its `omni.ja` by a second checksum, in
  place of 152.0.4-beta.30 and its Juggler backport (ADR 0011, which
  supersedes ADR 0010). It removes four things a page could read of every
  1.0.0 Firefox persona and of no stock Firefox: a page's
  `new Worker("chrome://juggler/…")` worked, a cross-site frame read the
  persona's first-party cookies and storage, `window.open()` without
  activation opened a window, and pages had 15 extra `set…` functions.
  Reading a page still grants it no user activation, now by the build
  itself. `engines.firefox.browser_major` is 156; Firefox 156 is
  browser-only on the http path (JA4
  `t13d1517h2_8daaf6152771_3cbfd9057e0d`, as a stock Firefox 156.0.1), and
  its capture replaces the 152 one. The Firefox image grows by about
  1.2 GB (the build bundles 2.1 GB of fonts).
- The launch plan speaks the persona's language at startup: its Firefox
  UI locale (`intl.locale.requested`, a table per country checked against
  the build's language packs) and the plan's other preferences reach the
  build through `CAMOU_PREFS` before the first page, so that `Intl` and the
  browser's own messages are the persona's (on 152 the messages were
  English). The engine leaves out of the configuration the keys the build
  declares `removed` and never applies `navigator.doNotTrack` or
  `navigator.globalPrivacyControl`, which the generator draws for nearly
  every device and 156 would have shown without the `Sec-GPC` header.
  WebGPU is on for Windows claims and for macOS claims with an Apple
  graphics card, and the memory cache and compression dictionaries are on,
  as in stock Firefox 156.

### Added

- `POST /v1/personas/upgrade` (sidecar contract 1.4.0, additive): a stored
  persona moved to the browser major its engine runs, the same bytes for
  the same document. At a persona's first open under a newer major the
  manager has it moved, stores it with its new `browser_major`,
  `user_agent` and `wreq_profile`, and records `persona_browser_upgraded`
  with what the persona was before; its sessions, cookies, http jar and
  per-domain state continue. Personas of 1.0.0 move to Firefox 156 this
  way, at their next open.
- Before a profile's first start under a newer Firefox major the engine
  copies it to `.upgrade/<persona id>/` on the personas volume; kage
  deletes a copy after `personas.upgrade_backup_days` (7). The rollback
  within that window is `docs/runbooks/upgrading-an-engine.md`, "Going
  back", and a test runs its database statement.
- Probe checks, hard for both engines: `coherence.intl_language` (the
  language of `Intl` against `navigator.language`),
  `headers.navigation.gpc` and `headers.navigation.dnt` (what a script
  reads against what the navigation sent), `automation.driver_worker` (a
  `chrome://juggler` worker) and `automation.extra_globals` (a `set…`
  function the browser does not have). The parity suite states what a
  page reads of a persona's document, of a moved persona, of third-party
  storage and of an unrequested popup, per engine. On Chromium the storage
  statement records a tell it found: a cross-site frame reads its site's
  first-party `localStorage`, because patchright's default switches turn
  third-party storage partitioning off (security review T2). Chrome is
  unchanged here; the audit of its launch switches is the next milestone.
- `scripts/audit-camoufox-prefs.mjs`: the preference audit of a Camoufox
  build against a stock Firefox of the same version, for the next bump;
  its table for 156 is `docs/evidence/camoufox-156-prefs-audit.tsv`.

### Removed

- `scripts/patch-camoufox-juggler.mjs`, its two checksums and the
  startup-cache clearing of the engine (ADR 0010). The `omni.sha256` notes
  in existing profiles are no longer read.

## [1.0.0] - 2026-10-18

The first release. Kage fetches web pages through a pool of static
residential proxies, one long-lived persona per address, in a real
browser (Chrome patched by patchright, or Camoufox, each in a hardened
sidecar) or on the http path with a browser's own TLS and HTTP/2
fingerprint; one page at a time or as batches, scripted flows and crawls,
with extraction, `robots.txt` per site, a response cache, webhooks,
sessions and an admin API, and with metrics, traces and an optional
observability stack. What follows is the record of the milestones M0 to
M12 that made it, newest first within each kind of change.

### Added

- The load test of the release (`tests/e2e/load.sh`, `tests/load/batch.js`):
  200 queued fetches, one batch job of three visitors, of the fixture site,
  driven by k6 from its pinned image with its usage report off. Under
  `fast` (design A, in `just test-e2e`) it must end in 45 to 240 s;
  under the test-only `load` profile, `human`'s shape at a fifth of its
  times at the shipped caps (design B, `just test-load`, before a release
  and in the nightly), in 540 to 720 s with the cap of 20 starts a minute
  reached and never exceeded. Both: every item `ok` with 200 at its first
  attempt in a browser and not from the cache, every page asked for once,
  by at most three addresses and without a forwarding header, no persona
  on two pages at once, the job never `paused`; B also at least 100 ms
  from the end of a page to the persona's next.
- M12, the follow-ups carried into the release:
  - `kage fetch` takes `--extract`, `--cache-ttl <duration>` and
    `--robots policy|report|enforce|ignore` and shows what they return:
    what the site's `robots.txt` says of the address, whether the answer
    came from the response cache and when its page was fetched, and the
    title, the words and the links of what was extracted (`robots`,
    `cached`, `cached_at`, `cache_ttl_s` and `extract` with `--json`).
    A wish the site's policy does not allow is refused before anything
    starts, as the API refuses it. The command line's cache entries are
    its own, and a fetch that may fill the cache keeps its document, so
    that `--output` writes the stored page for an answer from the cache
    (F50).
  - A test holds the bound on how much of a `robots.txt` is ever read
    (`MAX_ROBOTS_BYTES`, 4 MiB) to its value: the one mutant that
    survived on M11's lines of `kage-core` (F50).
  - A stored result keeps what its answer said of the visit besides the
    page: `pacing`, `behavior`, `console` and `side_effects` are the same
    in the answer of `POST /v1/fetch`, in `GET /v1/results/{id}` and in
    an item's result of a job, where they were null before (F36;
    migration 0016, `results.visit`). An answer from the response cache
    shows what the persona did on the page, the browser's log and what a
    capture fired as the visit that got the page had them, and its own
    pacing with `waited_ms: 0`. A result keeps at most the first 1000
    entries of a browser's log, each text cut at 4096 bytes.
  - A page of a crawl that changed hands between its stored result and
    its item keeps its links (F49). What the crawl takes from a page
    (where it ended, its links with whether a reader can see each, its
    text) is stored with the result (`results.reading`, migration 0016)
    and forgotten once the crawl has recorded the page; a worker that
    takes the job up reads on along those links instead of recording the
    page as read without any. The rule that turns a page into what the
    crawl takes moved into the pipeline, so that a page read now and one
    taken up later are read alike.
  - A server that stops finishes the pages that are being read (F39).
    Until now a fetch that was carried on as a job ran in a task nobody
    waited for, the job worker waited 20 s for pages that take one to two
    minutes, and Docker killed the container after 10 s: the attempt was
    cut without a record and the page fetched again after the restart,
    so the site saw it twice. Now the scheduler grants nothing more from
    the moment of the signal, the server waits for every attempt that is
    under way for at most `jobs.wind_down` (new, 150 s, at most 15 m),
    and what outlasts the wait is cut and written down as an attempt
    with the reason `interrupted`, which cools nobody and counts toward
    the request's retries. `kage` has a `stop_grace_period` of 200 s in
    `deploy/compose.yaml`. A request that waited for a persona when the
    signal came is answered `shutting_down` at once instead of waiting
    out the grace.
  - The `link` and `search_engine` entries that load a page to start
    from click as the entry from the open page does (F48). Their result
    said `ttfb_ms` 0 and counted the hand's way to the link into the
    times; a link over two lines was missed in Chrome (the hand aimed at
    the rectangle around both lines); a link under something else was
    clicked through it in Firefox; a link for another tab was clicked; a
    click answered by a page that draws itself waited 15 s; and a file
    the browser saved was `arrived` with the start page's document under
    the file's address. Now the times count from the click, only a link
    a reader can see that leads on in this tab is clicked, on the largest
    of its boxes and only when the pointer rests on it, a page that
    answers itself ends the wait after 1.5 s, and a file is no page
    (`aborted`). A client-routed site still costs a click and a typed
    load per page: documented in spec §10.4, not solved.
  - Traces (spec §20): every request is a trace, `api.request` →
    `pipeline.fetch` → `scheduler.wait` → `attempt` → `sidecar.navigate` or
    `sidecar.flow`, and every page of a job one from `job.item`; webhook
    attempts are `webhook.deliver`. With `telemetry.otlp_endpoint` set the
    spans go to that collector as OTLP over HTTP with protobuf, through
    wreq and never a proxy; without it nothing leaves the process. Every
    JSON log line written inside a span carries `trace_id` and `span_id`,
    and a request to a sidecar its `traceparent`. No span holds a URL's
    path or query, an address, a seed, a token or a fingerprint.
    `deny.toml` bans `reqwest`, which `opentelemetry-otlp` brings with its
    default features.
  - The sidecars' half of the traces: a navigation and a flow are spans of
    their own, the children of the span Kage names in the `traceparent` of
    its request, sent as OTLP over HTTP with protobuf when
    `SIDECAR_OTLP_ENDPOINT` is set (`SIDECAR_SERVICE_NAME`, default
    `kage-sidecar-<engine>`). The five `@opentelemetry/*` packages are
    loaded only then, and patch nothing. Every line of a request of Kage's
    carries its `trace_id`, and a line written during a navigation or a
    flow its `span_id`. `@opentelemetry/sdk-node`, planned since M0, is not
    used.
  - The `observability` profile (`deploy/compose.observability.yaml`): an
    OpenTelemetry Collector, Jaeger, Prometheus and Grafana at pinned
    digests, hardened like the rest of the stack, with Grafana alone
    published, on loopback. Seven dashboards (pool and personas, pacing
    and visits, blocks and breakers, jobs and crawls, webhooks, bandwidth
    against the plan's cap, cache and robots.txt), written by
    `scripts/grafana-dashboards.mjs`; seven alert rules (Kage down, no
    usable proxy, 80 % of the bandwidth budget, a breaker open, a domain's
    blocks at its policy's threshold, a persona retired, a webhook failing
    for good) with `promtool` tests. `just lint` checks the dashboards and
    the rules; `tests/e2e/observability.sh` reads one trace from the API
    to the browser back from Jaeger and greps every log line and span for
    the stack's secrets. Runbooks: `observability.md`, `block-report.md`,
    `field-campaign.md` (the owner's M13 campaign).
  - Two identical requests under way at the same moment are one visit
    (F52): of two fetches or batch items with one cache key in one
    process, the second waits for the first and is answered from the
    cache with what it stored; when the first kept nothing, the second
    makes its own visit. `kage_cache_requests_total{result="joined"}`
    counts the waits. The same crawl asked for again inside the lifetime
    still visits every page again.
  - A site's scout has an end to its rule and the operator a way to give
    a site another one (F51). A persona that read a site is kept from
    scouting it for `robots.scout_quarantine` (new, 30 days, 1 to 3650
    days), no longer for good, so a site whose every persona had read it
    once gets a scout again. Among the site's own personas the scout is
    first one whose engine keeps another reader of the site, so the
    preference for a Firefox no longer takes a site's only Firefox while
    a Chrome can ask. `POST /v1/admin/sites/{site}/scout` gives a site
    whose scout is not ready (and which, enforcing its file, is closed
    until the scout is back) another one, chosen by the same rule and
    never the one it had; the audit trail names the site, the new scout
    and the one before.
  - The pin of `hickory-resolver`, which no crate ever used, is gone
    (F37), and `just deny` now fails on a pin in
    `[workspace.dependencies]` that no crate of the workspace uses
    (`scripts/check-workspace-pins.mjs`; what is declared on purpose is
    listed there with its reason). The workspace entry of `html5ever`,
    which no crate of ours names, is gone too: it comes through `scraper`
    at 0.39.0, held by `Cargo.lock`.

- M11, extraction, `robots.txt` and the response cache:
  - `include: ["extract"]` returns what a page says: `title`,
    `description`, `canonical`, `lang`, `published_at`, `modified_at`,
    `meta`, `opengraph`, `twitter`, `jsonld`, `links`, `headings`, and
    its main content as `text` and `markdown` with `word_count` and
    `thin_content`. It is read from the document the visit captured,
    never by a second request, in a fetch, a batch's items, a flow's
    captures and a crawl's pages. `extract: { markdown, links }` leaves a
    part out.
  - The main content is Readability's rules (Firefox's reader view)
    ported onto the tree a crawl reads its links from, so a page is
    parsed once, with three rules of Kage's own (furniture is never
    content and no candidate means no content; bylines are metadata;
    siblings of the same shape are content). Mean F1 against text marked
    by hand: 97.0 on ten written pages of ten kinds, 97.7 on three more
    that were never tuned on; the furniture rule alone reaches 76.2, the
    best crate measured (`dom_smoothie`) 89.4. The markdown is written by
    the same walk as the text: fenced code with its language, GFM tables,
    absolute links, nested lists. Golden files per fixture page,
    regenerated by `just goldens`.
  - `robots.txt`. A reader never asks for the file, so asking marks the
    address that does: a site's file is asked for only where its policy
    says `report` or `enforce`, by the site's scout (one persona per
    site, which reads none of its pages and also asks for its sitemaps),
    once a day at most. Every result carries
    `robots: { fetched, allowed, matched_rule, crawl_delay }`. Under
    `enforce` what the file disallows is `robots_denied` and never asked
    for, a file that cannot be had closes the origin until it is asked
    for again, and `Crawl-delay` is a floor under each persona's gap on
    the host. A crawl never makes a disallowed address a page
    (`skipped.robots_denied`) and takes its sitemaps from the file.
    Parser and matcher are Kage's own, by Google's rules.
  - The response cache. `cache_ttl_s` (or the policy's `cache.ttl`): a
    request whose key holds a result that is young enough is answered
    with it, with `cached: true` and `cached_at`, without a visit and
    without a page of the daily budget. The key is everything that
    changes what a result holds, the include set and the API key among
    it; a policy may share a site's pages between keys (`cache.shared`).
    A crawl fills the cache and never reads it; a flow and a session stay
    out of it.
  - Configuration: `extract.thin_words`, `robots.ttl`,
    `robots.error_ttl`, `robots.max_bytes`, `robots.agents`,
    `cache.max_ttl`; the policy's `cache` section. Error codes
    `robots_enforced`, `robots_ignored`, `cache_ttl_too_long`. Metrics
    `kage_extract_duration_seconds`, `kage_robots_requests_total`,
    `kage_robots_denied_total`, `kage_cache_requests_total`. Migration
    0015.
  - The API test client reads what a result gained (`extractOf`,
    `cachedAt`, `robotsDenial`); `tests/e2e/extract.sh` runs all of the
    above by real personas of both engines and checks it against the
    fixture site's own request log: the essay asked for once per fetch
    and not at all while the cache answered, `robots.txt` asked for once
    by an address that read no page, the page it disallows never asked
    for, the sitemap it names read in place of the guess.

- M10, the crawler:
  - `POST /v1/jobs` takes `type: "crawl"` (the `crawl` scope besides
    `jobs`): `seeds`, `allow` and `deny` patterns on the normalized
    address, `max_depth`, `max_pages`, `same_site_only`, `use_sitemaps`,
    `defaults` for every page, `visitors`, `session_id` and `webhook`. A
    crawl is accepted whole or not at all, with `Idempotency-Key`, cancel,
    pause and deadline as a batch has them. It has no items when it is
    accepted: one is added for every page it asks for, with the page's
    depth, where its link was found, how it was reached and what it
    repeats, and the job shows how far the crawl has come and why it
    stopped reading.
  - A crawl reads like a reader, not breadth-first: the next page is a
    link in the text of the page that is open, else in the text of a page
    up to three entries back in the tab (a reader goes back to the
    listing for the next article), else a link of a menu within the same
    reach, and an address is typed only when the tab leads nowhere new. In a browser the persona clicks
    its way (sidecar contract 1.3.0: an entry by a link of a page it has
    been reading, with the keyboard's way back; F28); on the http path
    the page that holds the link is the `Referer`. Measured with the
    frontier itself (`cargo run -p kage-crawl --example reader_order`):
    breadth-first types 96 % of the addresses of a shop and 79 % of a
    blog's, this order 8 % and one address of 344; on the fixture's
    listing of ten articles it types 1 of 18 where taking the open page's
    menu first typed 11.
  - One persona reads one site (its registrable domain) through the
    scheduler like every other fetch: pacing, visits, waking hours,
    breaker and cooldowns are the same. `visitors` asks for more, up to
    the pacing's `domain_concurrency`.
  - Links no reader can see are never followed (`capture.links` of
    contract 1.3.0 says of every link whether it is rendered, has a box,
    is on the page and is not transparent; on the http path what the
    markup itself hides) and are counted as `hidden`.
  - Duplicates: one entry per normalized URL, the end of a redirect
    included; a page with the content hash of an earlier one is flagged
    and its links are not followed from there; a page whose 64-bit SimHash
    is at most 6 bits from an earlier one's is flagged as a near-duplicate
    and read on from. What is compared is the text without what the site
    repeats on every page.
  - Sitemaps are off by default: no reader asks for `/sitemap.xml`. With
    `use_sitemaps` a scout reads them first: a persona that is not the
    site's reader and never becomes it, a Firefox persona when one is free
    (Chrome saves a gzipped sitemap as a file). Where nobody but the
    reader could ask, nobody asks.
  - `kage-crawl`: the frontier (the reader's order, depth by the shortest
    way with lifting, the page cap, duplicates), the page reader (links
    with whether the markup hides them and whether they stand in the
    site's furniture, the main text), SimHash, sitemaps (index, gzip,
    `lastmod`), and the scope of a crawl. URL normalization in `kage-core`
    is property-tested, with twelve more tracking parameters.
  - A crawl section in the configuration (`crawl`), the frontier in
    `crawl_frontier` (migration 0014), a page claimed for its item with
    `SELECT ... FOR UPDATE SKIP LOCKED`, and a crawl that changes hands
    goes on where it stands. Metrics `kage_crawl_pages_total{reached}`,
    `kage_crawl_skipped_total{reason}` and
    `kage_crawl_duplicates_total{kind}`.
  - The TypeScript test client drives a crawl (`createCrawl`,
    `crawlProgress`, `readCrawlPages`); `tests/e2e/crawl.sh` reads the
    fixture site with real personas of both engines.
- A persona rests between leaving a page and asking for the next one of
  the same sitting (F43): `rest_ms` of a pacing profile (`human`: floor
  700 ms, median 2.2 s, p95 7 s; `fast`: none), applied by the planner.
  Before, a page that was read for longer than the gap was followed by the
  next one 1 to 3 ms after it was left, in both browsers.
- A fetch's `behavior` says the patience the persona had with the page
  (`patience_ms`), how far down it came (`scroll_reached`) and what ended
  its scrolling (`scroll_ended_by`: `depth`, `patience` or `page`); the
  patience is kept with the attempt (`attempts.patience_ms`).

- M9, jobs and webhooks:
  - `POST /v1/jobs` with `batch` and `flow` jobs, accepted whole or not at
    all, with `Idempotency-Key` for a day; `GET /v1/jobs/{id}`,
    `GET /v1/jobs/{id}/items` (paginated, each item with its result) and
    `POST /v1/jobs/{id}/cancel` (cooperative: the page under way is
    finished, nothing more is begun). A worker claims jobs with
    `SELECT ... FOR UPDATE SKIP LOCKED` under a lease, and a job whose
    worker stopped is taken up again where its items stand.
  - A batch has one visitor per site: one persona reads the site's pages
    in order and another takes over only when it cannot go on. `visitors`
    asks for more, up to the pacing's `domain_concurrency`. An item that
    cannot be attempted now waits instead of failing, and a job whose
    every remaining item waits is `paused` until the time the scheduler
    named, for at most `jobs.max_age`.
  - A flow is one browser attempt of one persona, always paced like a
    person (sidecar contract 1.2.0: a dwell on every page it arrives on, a
    thinking time before every action). Its steps are items, each capture
    a result of its own. Every `goto` passes the SSRF guard at acceptance
    and everything a flow visited is checked afterwards. `human: false`
    steps need the site policy's new `driver_input`.
  - Webhooks: `job.completed`, `job.failed` and `job.progress` as thin
    events signed with HMAC-SHA-256 over `<timestamp>.<body>`
    (`X-Kage-Signature`), to receivers the operator listed
    (`webhooks.allowed_origins`, none by default; `webhooks.private_origins`
    for those on the operator's own network), tried again five times on a
    jittered schedule, with a delivery log on the job.
  - A synchronous fetch that outlasts its wall time is carried on as a job
    of one item under its own request id (F35): the `202` has a `job_id`,
    and the result is reachable after a restart of the server.
  - `POST /v1/admin/purge` deletes what a key fetched before its retention
    ends, on the audit log (F33); finished jobs are deleted after
    `jobs.retention_days`.
  - Metrics for jobs, their items and webhook deliveries (spec §20).
  - `tests/e2e/jobs.sh`: a batch of 50 pages, a flow in each engine, a
    cancel, the webhook's signature at a receiver, a fetch carried across
    a restart, and the purge, on the compose stack; the TypeScript test
    client drives the jobs.

- M8, the public API v1 and the admin API:
  - `POST /v1/fetch` over the pipeline of M7: one page fetched while the
    caller waits, answered `200` with its result whatever the site did (a
    block is the result's `outcome`), and `202` with the request's id when
    it outlasts `scheduler.sync_wall_time`; `GET /v1/results/{id}` returns
    a result again and `GET /v1/artifacts/{id}` streams what was kept of a
    page (the document, the raw body, a screenshot, a PDF, the bodies of
    captured XHR responses).
  - OpenAPI 3.1 generated from the Rust types, served at `/openapi.json`,
    committed as `crates/kage-api/openapi.json`, printed by `kage openapi`,
    with Swagger UI at `/docs` for the admin scope. Errors are RFC 9457
    problem documents with a stable `code`; every answer has
    `X-Request-Id`.
  - API keys `kage_<keyid>_<secret>`: 32 random bytes of which only the
    SHA-256 is stored, five independent scopes, and per key requests per
    minute, requests in flight and a daily page budget, beside a limit of
    requests per minute for the whole API. `kage key create | list | revoke
    | rotate` and the same through `/v1/admin/keys`. Every admin call and
    every change at the command line is in `audit_events`.
  - What a request may ask of a persona's behaviour: `behavior.scroll_depth`
    (0 is no scrolling), `behavior.dwell_ms` (the median of what the
    persona draws from, never below the policy's new `min_dwell`) and
    `behavior.referer_strategy`; a policy's new `behavior_locked` ignores
    them. A `timeout_ms` too short for a visit is refused, not clamped.
    The result's `behavior` and `pacing` say what applied.
  - `request_kind: fetch | xhr` on the http path (follow-up F6): a request
    sent as a page's script sends one, with the header order, `Sec-Fetch-*`
    values, `Accept` and `Priority` Chrome 155 and Firefox 152 were
    captured sending, the page that makes it as `Referer`, and an `Origin`
    where a browser sends one. What a browser would preflight is refused.
  - Sessions through the API: opened by `POST /v1/sessions` and by nothing
    else, pinned to one persona that hints and a domain's policy choose,
    never moved; a fetch whose constraints exclude its session's persona
    is `409 session_persona_conflict` and nothing is fetched.
  - The admin endpoints: personas (list, show, retire, regenerate), proxies
    (list, probe, replace), the pool with the bandwidth budget, a policy
    reload, the audit trail. A persona's self-tests run in the server,
    through the scheduler's gate and the forwarder, in the browser context
    real work has open (follow-up F29).
  - What a consumer sees: a persona as a handle of the key's own, never a
    persona's id, a proxy, an address or a seed; the cookies a site set
    without their values (`security.expose_set_cookie` returns them).
  - The SSRF guard on every target, with `security.resolve_on_host` for a
    lookup on the Kage host (off by default: the proxy resolves), and a
    guard for the webhook URLs of M9.
  - A listener with the limits `axum::serve` has no switch for: the size
    and count of headers, a deadline for the head of a request (slowloris),
    a deadline for its body, a connection cap, a graceful stop; body and
    request-target limits; CORS off unless origins are named.
  - `/metrics`: the Prometheus exporter over the series the scheduler and
    the pool recorded since M7, the API's own, the personas by state, the
    latest self-test scores, and both sidecars' series merged in; behind
    the admin scope, or on `server.metrics_bind` without a key.
    `/readyz` checks the database, a ready persona and a sidecar.
  - `tests/api-client`: the document checked by an outside validator and a
    TypeScript client generated from it, which the new compose end-to-end
    test (`tests/e2e/api.sh`) fetches a page with through both engines.
  - `docs/runbooks/api.md`.
- M7, the scheduler, pacing, block handling and escalation:
  - A new crate, `kage-scheduler`, over rules in `kage-core` that take
    time as an argument (`kage_core::schedule`) and are mutation-tested.
    A request waits for a persona and its slot: synchronous work before
    jobs, then first come first served, under the caps
    `scheduler.max_browser_sessions` and `scheduler.max_http_inflight`
    and the domain's own concurrency and requests per minute.
  - Pace: a persona that fetched a page from a domain keeps that domain's
    work for its visit, in bursts and breaks at its own factor, with a long
    break after a few bursts (`visit_bursts`, `visit_break_ms` in the
    pacing profiles); the daily cap turns at the persona's own midnight;
    each persona has a day of its own, and `active_hours: enforce | prefer
    | ignore` per domain says what it means; no two personas start in the
    same moment.
  - Persona selection by the score of spec §8.2 over the personas that may
    serve, away from the network of a persona that was just blocked.
  - Retries on another persona after a drawn pause; cooldowns from the
    policy scaled by the confidence of the signature that matched, with a
    floor and a jitter; a pause for a domain after server errors in a row;
    the circuit breaker in `domain_breakers` with one trial after its
    pause and a doubling pause; `domain_paused` as an alert.
  - Under `render: auto` a block on the http path moves the request to
    the browser exactly once: the same persona's for a block a browser
    cures, another persona's after a refusal.
  - One attempt on either path (`PathExecutor`): the http path with the
    persona's client, cookies and the previous page as `Referer`; the
    browser path with a human dwell, a scroll depth from the attempt's
    seed and a wandering pointer, cookies handed over before and after,
    the redirect chain checked against the SSRF guard, a detected
    interstitial given `browser.challenge_wait_ms` to resolve, and the
    browser closed after a block. Every attempt stores its seed.
  - The forwarder (follow-up F10): the one proxy every persona's browser
    and http client is given. The provider's credentials stay in the kage
    process, every byte is counted per persona and domain
    (`bandwidth_usage`), and it relays nothing the SSRF guard refuses,
    nothing for a persona that may not send, and nothing new while a leak
    is on record or the budget is used up.
  - The bandwidth budget (decision D5): `bandwidth.monthly_cap_gb` (250)
    per billing term, judged on the larger of Kage's count and the
    provider's; `bandwidth_budget_warning` at 80 % with the projected
    day, `bandwidth_budget_exhausted` and a hard stop at the cap.
  - One owner of persona contexts (follow-up F27): a persona's browser
    stays open through its visit and `scheduler.context_idle` after it,
    and warm-up visits and self-tests ask the scheduler first.
  - `kage fetch <url>`: one request through the whole pipeline, in
    sentences or as JSON. `kage serve` runs the scheduler, the forwarder
    and the budget; `kage config check` reports them.
  - Metrics of the scheduler and of attempts (spec §20), recorded through
    the `metrics` facade.
  - A compose end-to-end test of the fetch pipeline with real browsers on
    a world of its own (`tests/e2e/fetch.sh`, `deploy/compose.e2e.yaml`).

- M6, the persona manager and session binding:
  - A new crate, `kage-persona`. A proxy that is usable gets one persona,
    generated from the observed geo record of its address: time zone,
    a position near the lookup's coordinates, the country-carrying locale
    and the language list a stock installation of that browser ships with
    in that country (which differs between Chrome and Firefox), an entry
    of `personas.mix` drawn by what the pool and the country are short of,
    and a screen class from a seeded table that the generator is asked
    for by exact bounds. The sidecar's fingerprint is stored whole. The
    persona's seed, in its own column, is what every draw hangs on.
  - A persona follows its proxy: cooling in backoff, a new time zone and
    position when the geo record moves, new locale and languages and a
    cooling until a self-test passes when it moves across a border, and
    retirement with the proxy, whoever replaced it. A retired persona's
    sessions end and its http cookies are deleted at once; its profile
    directory is archived after ten minutes and the archive deleted
    after `personas.retention_days`. `kage serve` runs the manager.
  - Sessions: ids unique per API key, pinned to the least loaded ready
    persona, with an idle and a maximum lifetime (`sessions.*`).
  - Per-domain state on the persona's own calendar: cooldowns after a
    block, the daily cap until local midnight, block counters.
  - The http cookie jar belongs to the persona (`http_cookies.persona_id`),
    and cookies move between its browser context and its http jar in both
    directions, unless its profile is not its browser's major.
  - Warm-up (`personas.warmup`, on by default): a new persona visits a few
    popular neutral sites in its own browser during its first hours and is
    given real work only after three of them or twelve hours.
  - Self-tests per persona, every 24 hours with a jitter of its own, only
    against the operator's own echo and pages; a failure cools the persona
    and raises `http_selftest_failed` or `browser_selftest_failed`, a run
    that cannot read its test site changes nothing. A health probe now
    presents as the persona behind the proxy it checks.
  - `kage persona list|show|retire|selftest|regenerate`, and `--persona`
    on `kage selftest http` and `kage selftest browser`.
  - Pacing: a persona's own factor, log-uniform in 0.8 to 1.25 from its
    seed, for the median and the 95th percentile of its gaps.
  - The replacement budget counts per subscription term when the provider
    reports one. `webshare.accepted_auto_replace` names the automatic
    replacement settings that raise no alert (default
    `auto_replace_invalid_proxies`). The geo record keeps the lookup's
    coordinates.

- M5.1, page reads without user activation:
  - The Chromium engine reads a page over its own debugger session:
    title, text, DOM, selector waits, storage and the measurements of the
    behavior engine are evaluated with `Runtime.evaluate` in an isolated
    world of the engine's own, without the user-gesture flag the driver
    sends. The driver keeps the launch, navigation and its waits, and the
    input of `human: false` steps.
  - The pinned Camoufox build runs with upstream's Juggler fix backported
    into its `omni.ja` (`scripts/patch-camoufox-juggler.mjs`, ADR 0010),
    in the image and after `just setup-camoufox`.
  - The coherence probe compares `navigator.userActivation` and a new
    `AudioContext` with the trusted input events it received:
    `automation.user_activation`, a hard failure, and
    `automation.audio_autoplay`, a warning.
  - The fixture site's activation recorder (`/activation/`) and a parity
    test on both engines: an untouched page is not active and its
    `AudioContext` is suspended at `load`, 300 ms and 4 s later, with the
    captures taken; a click activates it.

- M5, the Firefox engine and the behavior engine:
  - The Firefox engine: Camoufox `v152.0.4-beta.30`, fetched by URL and
    checksum, one browser per persona on its own X display with a
    persistent profile and an authenticated proxy. A Firefox persona claims
    Windows or macOS; the claim was measured coherent in the window, every
    kind of iframe and both kinds of worker, fonts and WebGL included
    (`docs/runbooks/engine-firefox.md`). The fingerprint is generated once
    from the persona's seed and persisted as the browser's whole
    configuration; an open generates nothing. The graphics card is drawn
    from the launcher's table of real devices without the rows that name
    another system's renderer or a software one.
  - The behavior engine, shared by both engines: mouse paths (curved, timed
    by Fitts' law, with tremor, overshoot and correction), wheel scrolling
    in flicks with reading pauses proportional to the text in view, dwell
    from a log-normal, typing with per-key timing, modifier keys and
    optional typos, and an idle hand that wanders. Each persona has its own
    motor profile from its seed; each request its own recorded seed.
  - Input goes through the mouse and keyboard of the context's own X
    display (XTEST), not through a browser driver, on both engines, with
    the keyboard layout of the persona's locale.
  - `navigate` takes `behavior`: scroll depth, human dwell, wander, and the
    entry strategies `none`, `direct`, `link` and `search_engine`, which
    arrive by a real click with the browser's own `Referer`.
  - The flow endpoint: the v1 actions (`goto`, `click`, `type`, `press`,
    `scroll`, `wait_for`, `select`, `hover`, `back`, `extract`,
    `screenshot`, `pdf`, `assert`) streamed as NDJSON; `SidecarClient::flow`
    in `kage-browser`.
  - An engine parity suite: the same contract, capture and behavior tests
    run against both engines.
  - Both images announce a desktop to their browsers: a work area smaller
    than the screen and a sound server with an output and an input
    (follow-ups F9 and F12 of M4).
  - The coherence probe measures fonts and WebGL in every scope and
    reports what surrounds the browser (work area, media devices, audio).
  - `just capture-browser-fingerprint firefox`, the capture
    `tests/fixtures/fingerprints/browser/firefox-152.json`, and Firefox 152
    recorded as browser-only on the http path (its TLS offers one cipher
    suite fewer than the newest profile; a stock Firefox 152.0.4 measures
    the same as the Camoufox build).
  - `just setup-camoufox`, `kage selftest browser --engine firefox`,
    `engines.firefox.browser_major`.
  - The live browser self-test covers the Firefox engine, with its
    recorded baseline `tests/fixtures/selftest/firefox-152-live.json`. The
    judge of bot.sannysoft.com reads that page's Chrome row by the browser
    the page saw: a Firefox has no `window.chrome`.

- M4, the browser sidecar and its Chromium engine:
  - `@kage/contract`: zod schemas for every endpoint of the sidecar
    (health, metrics, persona generation, contexts, navigate, flow, state
    export and import, self-test), the OpenAPI document emitted from them
    and committed (`sidecar/packages/contract/openapi.json`), and a check
    that fails a change which breaks clients without a new major of the
    contract (`just contract-check`, part of `just ci`).
  - The sidecar server: every route behind the shared secret, bodies
    validated in and out, Prometheus metrics, a context manager with a hard
    cap, idempotent opens, idle close and browser recycling, and artifacts
    written to the shared volume with their checksum.
  - The Chromium engine: real Chrome stable, pinned by version and package
    checksum, headful on one X display per persona, with a persistent
    profile and an authenticated proxy. Loopback goes through the proxy and
    WebRTC sends nothing around it. Chrome's sandbox stays on.
  - A chromium persona claims the system its Chrome runs on, Linux, and is
    made by how the browser is launched, with no script in any page
    (ADR 0009). `engines.chromium.allow_cross_os_claims` exists and is off.
  - `navigate` with waits and captures that a page cannot notice: DOM, raw
    body, headers, redirect chain, XHR and fetch bodies, the browser's log,
    timing and challenge signals. Screenshots and PDFs are files on the
    artifacts volume; a capture with a visible side effect is opt-in and
    named in the result.
  - `state/export` and `state/import` in the shape of an `http_cookies`
    row, so a session moves between the http path and a browser.
  - The self-test endpoint and its pages: a coherence probe (window,
    frames, workers, request headers, cache), the fingerprint echo, and
    readers for bot.sannysoft.com and CreepJS. Third-party pages are loaded
    only on request.
  - `kage-browser`: Rust types generated from the committed OpenAPI
    document at build time, a client with timeouts and retries on
    connection errors only, the post-hoc SSRF check of a navigation's
    redirect chain, and a contract test that fails when the document and
    the client drift apart.
  - `kage selftest browser`: runs the self-test for one proxy through the
    sidecar. `selftest.browser` in the configuration names the pages.
    `--idle-before` leaves the context untouched first, and the gated live
    check uses it to measure what an idle Chrome sends by itself
    (`KAGE_LIVE_IDLE_SECONDS`).
  - The fingerprint echo serves `/browser`, a page that makes a real
    browser show its navigation, `fetch()` and `XMLHttpRequest` requests;
    `just capture-browser-fingerprint` records them. Chrome 155 is recorded
    and is browser-only on the http path: its TLS fingerprint is newer than
    any emulation profile (`BROWSER_ONLY_MAJORS`).
  - The Chromium sidecar image and its hardening: non-root, read-only root
    filesystem, no capabilities, and a seccomp profile derived from
    Docker's default that lets Chrome's sandbox work. The stack smoke test
    checks it with a context open.
  - Runbook `docs/runbooks/engine-chromium.md`.

- M3, the http path:
  - `kage-http`: fetches a page through a proxy with wreq, looking like the
    browser a profile names. A client factory keeps one client per proxy and
    profile; wreq-util supplies TLS and the HTTP/2 settings, and Kage writes
    the complete header list itself, in the browser's order and spelling,
    for Chrome and Firefox, over HTTP/2 and HTTP/1.1, for a page and for its
    stylesheets, scripts and images. `Sec-Fetch-Site` follows the referrer
    over the whole redirect chain, and the `Referer` that goes out follows
    the browsers' default policy.
  - Caller headers are checked against a denylist. A header that belongs to
    the browser's identity (`User-Agent`, `Accept-Language`,
    `Accept-Encoding`, `Cookie`, every `Sec-*` and others) is refused with
    an error that names it, never dropped silently.
  - Redirects are followed by Kage, ten at most by default, and every hop
    passes the SSRF guard again; the refused hop is named. The body is read
    up to `http.max_body_bytes`, and the result says when it was cut.
  - A cookie jar with the rules browsers apply (`SameSite`, `Secure`, the
    `__Secure-` and `__Host-` prefixes, the Public Suffix List, limits and
    eviction), stored in Postgres per proxy (migration 0009,
    `http_cookies`). It survives a restart, is never handed to a
    replacement proxy, and forgets session cookies after
    `http.session_cookie_idle`.
  - Timing per phase: tunnel (`connect_ms`), TLS handshake (`tls_ms`), first
    byte and total. The health probe records the first two per proxy, and
    `kage proxy health` shows them, so a proxy that is slow to set up a
    connection can be told from one that is slow to answer.
  - Subresource emulation (`http.fetch_subresources`, off by default): after
    a page, its own site's stylesheets, scripts and icon are fetched the way
    a first paint would, with seeded pauses and a freshness cache. Assets of
    other sites are never fetched.
  - The profile mapping of spec §6.1: (engine, browser major, OS) to a
    wreq-util profile, for the newest eight majors of Chrome and Firefox. A
    test enumerates wreq-util's profiles and fails when a configured engine
    major has none. `engines.chromium.browser_major` and
    `engines.firefox.browser_major` name the engines' majors.
  - A fingerprint echo inside `kage-http`: a TLS server that reports the JA4
    of the ClientHello, the HTTP/2 fingerprint and the request headers in
    wire order. `kage echo serve` runs it; the compose profile `echo` adds
    it to the stack.
  - The fingerprint gate: the JA4, the HTTP/2 fingerprint and the header
    order of all 48 mapped profiles are recorded in
    `tests/fixtures/fingerprints/` and compared on every change in
    `just ci`, over loopback. A dependency bump that moves one fails there.
  - `kage selftest http --proxy <id> --profile <name>`: the same comparison
    through a real proxy, against the operator's echo
    (`selftest.http.echo_url`) or, on demand only, a third-party one. The
    server never asks a third-party echo on a timer.
  - New operator alerts: `http_selftest_failed`, `http_profile_missing`.
  - Configuration: new sections `http`, `selftest` and `engines`.
  - `docs/runbooks/fingerprint-echo.md`; a gated live check of the
    fingerprint through two real proxies (`tests/live/http-selftest.sh`).
  - The coverage gate now includes `kage-http`.

- M2, proxy provider, pool, health and replacement:
  - `kage-proxy`: the `ProxyProvider` trait with two implementations. The
    Webshare client (on wreq, no second TLS stack) reads the proxy list page
    by page, requests and follows replacements, reads the log of replaced
    proxies, the account (plan, replacement quota, automatic-replacement
    settings, stock per country) and hourly traffic. It keeps its own request
    rate below Webshare's limits, waits out a 429, and never sends a request
    that changes something twice. The static list provider serves a fixed
    list from `KAGE__PROXIES__STATIC_LIST`, for tests and other vendors.
  - The pool: stored proxies are kept in step with the provider every
    `proxies.sync_interval`. A proxy missing from the list is retired at once
    only when the provider's own log confirms it, otherwise after three
    syncs, and never on the word of an empty or far too short list.
  - The pool state machine of spec §7.3 as pure functions with property
    tests: three failed checks send a proxy into backoff (1 min doubling to
    1 h, jittered), one probe follows every backoff, and a proxy is replaced
    only after three backoff cycles and six hours of failing without a break
    (`proxies.replace_after`), within `proxies.replacements_per_month`. The
    budget is enforced in Postgres, so the server and the CLI cannot overspend
    it together. Replacements ask for the same country.
  - Health checks, each proxy on its own jittered schedule: reachability,
    latency and exit address through the provider's own echo (a second echo
    is tried once before a failure counts), a leak check against the host's
    own address, a geo lookup of the exit address on the first check, on an
    address change and once a day, and optional IP reputation lookups
    (AbuseIPDB, off without a key). When more than half of the pool fails at
    once the failure counters freeze: the cause is on the path.
  - A proxy that lets the host's own address through is parked and sets the
    pool-wide `egress_unsafe` flag, which later milestones must honor before
    dispatching anything. It clears when a probe through that proxy shows a
    distinct exit address, or with `kage proxy egress --clear`.
  - Proxy user names and passwords are encrypted at rest with
    XChaCha20-Poly1305 under `KAGE__SECRETS__MASTER_KEY`, each value bound to
    its proxy and its field.
  - `kage proxy list`, `kage proxy health`, `kage proxy replace` and
    `kage proxy egress`, as tables or with `--json`; they may run next to the
    server. `kage serve` syncs the pool and runs the health checks when a
    provider is configured; proxies stored from an earlier provider are
    still health-checked when none is.
  - New operator alerts, stored as health events: proxy leak, unsafe egress,
    geo mismatch, bad reputation, suspicious usage type, replacement budget
    exhausted, replacement failed, pool outage suspected, pool capacity low,
    suspect provider list, provider replaced a proxy, provider
    auto-replacement enabled, provider account, provider sync failed, host
    address unknown.
  - Configuration: new `proxies` section, more keys under `webshare` and
    `reputation`, the secret `KAGE__PROXIES__STATIC_LIST`. The provider is
    chosen by `proxies.provider` (`auto` by default: Webshare when its key is
    set, else the static list, else none).
  - `docs/runbooks/proxies.md`; a gated live check of the real proxy list
    (`just test-live`, `tests/live/proxy-health.sh`).
  - The compose stack and `just dev` pass `KAGE_WEBSHARE_API_KEY` and
    `KAGE_ABUSEIPDB_KEY` from `deploy/.env` on when they are set. The smoke
    test never uses them.

- M1, core domain, configuration and store:
  - `kage-core`: the domain model of spec §5 (proxies, personas, sessions,
    requests, attempts, results, artifacts, jobs, keys, events) with typed
    UUIDv7 ids, `Outcome` and the block report.
  - Domain policies (`policies/domains.yaml`): each field is inherited from
    the exact host, then its parent domains from nearest to furthest, then
    `defaults`; per-outcome cooldowns plus a `short` cooldown; pacing
    resolution request, then domain, then default, with `pacing_locked`.
  - Pacing profiles (`policies/pacing-profiles.yaml`) with seeded, replayable
    samplers: log-normal by median and p95 (shifted, so a `min` leaves neither
    a spike nor an edge at the floor), uniform, and bursts with breaks.
  - Block classifier: signatures as data (`policies/block-signatures.yaml`)
    with matchers on status, headers, cookies, body, URL and browser signals,
    covering Cloudflare, Akamai, DataDome, HUMAN, Kasada, Imperva, AWS WAF,
    F5, reCAPTCHA, hCaptcha, generic statuses and soft blocks. Each rule says
    whether a match is retried, escalated to the browser and how long the
    persona cools down; every verdict lists the markers that fired. A
    vendor's cookie on a good page is a hint, never a block. 44 fixtures
    under `tests/fixtures/blocks/`, all synthetic and marked as such.
  - SSRF guard as pure functions: denied address ranges, name rules, literal
    IP targets behind `security.allow_ip_targets`, and a redirect chain that
    re-checks every hop, records `https` to `http` downgrades and names the
    hop it refuses.
  - URL normalization into a dedupe and cache key (spec §13), including
    RFC 3986 percent-encoding normalization. Requests keep the URL as
    discovered.
  - Configuration: `kage.yaml` layered with `KAGE__SECTION__KEY`, secrets
    only from the environment or `_FILE` variables, and `kage config check`,
    which prints the effective configuration with secrets redacted.
    `deploy/kage.example.yaml` lists every key.
  - Policy hot reload: a changed file is validated and swapped in; an invalid
    one keeps the last good policy and raises an alert, stored as a health
    event.
  - `kage-store`: migrations for the 18 tables of spec §18, repositories with
    compile-time checked queries and committed offline data (`.sqlx/`), and
    the content-addressed artifact store with gzip for text and a retention
    job.
  - `kage migrate` (and `--check`); `kage serve` now requires a reachable,
    migrated database and valid policies. The compose stack runs the
    migrations in a one-shot `migrate` service.
  - `just sqlx-check`, `just sqlx-prepare`, `just config-check`; the coverage
    gate now includes `kage-store`.

- M0, repository scaffold and CI gates:
  - Cargo workspace (edition 2024, Rust 1.99.0, workspace lints) with the
    `kage` binary and eight library crates. `kage serve` exposes `/healthz`;
    `kage healthcheck` probes it for the container healthcheck.
  - Sidecar pnpm workspace (TypeScript strict, ESM) with the `contract`,
    `engine-core`, `engine-chromium`, `engine-firefox` and `server` packages.
    The server exposes `/v1/health`; the `ENGINE` variable selects the engine
    package; both engines are stubs.
  - `justfile` with every dev and CI command; `just ci` runs everything a
    pull request runs.
  - Dockerfiles for `kage` and both sidecar variants, and a compose file with
    Postgres 17, healthchecks and hardened containers.
  - GitHub Actions: `ci.yaml`, `nightly-live.yaml` (skeleton) and
    `release.yaml`.
  - Supply-chain gates: cargo-deny, cargo-audit, pnpm audit, a license policy
    for the sidecar, gitleaks, Trivy, CycloneDX SBOMs, and a check that fails
    when `openssl-sys` is in the Rust dependency graph.
  - Coverage gate: at least 80 % line coverage on `kage-core`, `kage-proxy`,
    `kage-crawl`, `kage-extract` and `engine-core`.
  - README with a responsible-use section, dual MIT / Apache-2.0 license
    files, CODEOWNERS and the repository runbook.
  - Exact dependency pins re-verified and recorded in `docs/versions.md`.
  - ADR 0008 (proposed): AGPL-licensed `ua-parser-js` in the Firefox engine
    dependency tree.

### Security

- Kage takes a file a sidecar wrote only as the sidecar describes it
  (security review S1, `docs/security-review.md`). Until now it read
  whatever path below the artifacts directory a sidecar named, following
  links, and ignored the size and the SHA-256 the sidecar sent with it: a
  sidecar that was taken over could name a link to Kage's own
  `/proc/self/environ` or a secret file, and Kage stored that file as the
  request's screenshot. Now the file must lie below `sidecar/` with a real
  path that stays there, be a plain file (opened without following a link
  and without waiting on a pipe), hold at most 128 MiB, and have the size
  and the SHA-256 named; anything else is not taken. `libc` is a direct
  dependency of `kage-scheduler` on Unix for the two flags.
- The SSRF guard judges an IPv4-translated IPv6 address
  (`::ffff:0:a.b.c.d`, RFC 2765) by the IPv4 address inside it, as it
  already judged the IPv4-mapped, IPv4-compatible, NAT64 and 6to4 forms:
  `http://[::ffff:0:7f00:1]/` is loopback (security review G6).
- `webshare.base_url` and `reputation.base_url`, the roots the provider's
  and AbuseIPDB's keys are sent to, must use `https`; plain `http` is
  accepted only to `localhost` and the loopback addresses.
- Kage reads at most 128 MiB of a sidecar's answer (`MAX_ANSWER_BYTES`):
  one that says it is longer is refused unread, one that does not say is
  cut off at the limit, and the call fails as an answer the contract does
  not describe. Until now an answer was read whole, so a sidecar that was
  taken over could fill Kage's memory.
- `kage persona selftest` and `kage selftest http|browser --persona`
  bind a forwarder of their own, as `kage fetch` does (security review
  G4). They handed the persona's browser the provider's proxy with its
  password, which then sat in the sidecar for the run. The stand-in
  self-tests of a proxy (`--proxy`), which have no persona the forwarder
  could name, remain the one exception.
- The sidecars are on an internal network of their own, `browsers`, with
  kage alone (security review L4). They were on the stack's default
  network, where a browser that was taken over could reach Postgres and,
  past the forwarder, the internet. Docker gives an internal network no
  gateway: a browser's only way out is kage's forwarder. `just hardening`
  refuses a sidecar on a network with a way out, the smoke test checks
  the running stack, the observability profile puts its collector on both
  networks, and `deploy/compose.dev.yaml` (kage on the host) keeps the
  sidecars on the default network too.

### Fixed

- On a slow machine a persona scrolls pages again. A browser hands a
  page its pointer moves and its wheel with the frames it draws, and a
  new document draws none for a while when the software renderer
  compiles its shaders: a quarter of a second on a workstation, more
  than half a second on a four-core cloud runner. The hand turned the
  wheel in that time, saw the page not move, and took it for a page that
  does not scroll: every page ended after its first flick with nothing
  read, and the page saw a pointer path of four moves. Now the hand acts
  on a page once it has drawn two frames (waited for at most 2 s, from
  the reader's world), and a flick that moves nothing is turned once
  more after the page draws before the page counts as one that does not
  go further. Found by the engines' parity suites on GitHub's runners;
  reproduced here with the browser's CPUs held to 0.6 of one. For the
  same reason a Chrome screenshot taken right after a load could fail
  with "Unable to capture screenshot": it now asks again for up to 2 s
  while the page has no frame to give.
- A batch or a crawl with several visitors no longer stands still for
  the long break of one of them while another reads (F54). The planner
  told a page that would rather be refused than wait five minutes to
  come back when the resting visitor did, as if the visitors in the
  middle of a page were not there, and the job worker then left the
  whole site alone until that time. Now a visitor that is reading counts
  from its slot, one that an earlier page waits for counts too, and a
  page is refused only when nobody it may have could start in time; a
  site left alone for `no_persona` is asked again when one of its pages
  ends. In the simulation of the load test, 200 pages under `human` with
  three visitors took a median of 146 minutes and take 61, as long as
  when every page may wait as long as it must; 50 pages took up to 79
  minutes (p95) and take 14. Outside batches the same rule holds: a
  request that would rather be refused waits for a persona that is in
  the middle of a page.
- The sidecar images keep no setuid or setgid file (Chrome's
  `chrome-sandbox`, the base image's `su`, `passwd`, `mount` and the
  rest). `no-new-privileges` already kept them from raising anything;
  they are no longer there. `just hardening` checks the Dockerfiles and
  every compose file against the container checklist, `just
  hardening-images` the built images, both in `just ci`.
- The sidecars' process metrics (`process_*`, `nodejs_*`) carry their
  `engine`: Kage merges both sidecars' metrics into one exposition, where
  each of these series was there twice, once from each sidecar, with
  nothing to tell them apart.
- RUSTSEC-2026-0367, -0368 and -0369 against `zip` 8.6.0 are ignored in
  `deny.toml` and `.cargo/audit.toml` until 2026-12-31 or the first
  `utoipa-swagger-ui` on `zip` 9: `zip` only unpacks the vendored Swagger
  UI bundle, pinned by checksum, when Kage is compiled, and the advisories
  are about extracting an attacker's archive.
- `rustls` 0.23.46 in place of 0.23.45 (RUSTSEC-2026-0346 to -0349,
  published 2026-10-14): the TLS of the fingerprint echo and of the
  database connection. Outbound requests to targets are wreq's and never
  used it.
- A job with `webhook.progress_every` was accepted and then failed in the
  worker with `validation_failed` when it was a flow, because the worker
  read the stored job back as a request and the stored job carries what
  the request did not (F46). The same test showed that a crawl never
  reported its progress at all: `job.progress` was queued only while
  other items of the job were open, and a crawl's list grows one page at
  a time. One function is now the only way a worker reads a stored job
  back, and a crawl reports like a batch.
- A file a browser saved instead of showing it (a gzipped sitemap in
  Chrome) was judged as a page with nothing on it: a soft block, which
  cooled the persona that asked for it down for the site and counted on
  the site's breaker. A navigation that was aborted and left neither a
  document nor a body is no page, and turns nobody away. Until M11 a
  second persona then asked for the next sitemap; a site has one scout
  now.
- `just mutants` takes the number of jobs (`just mutants 24`; one
  mutant at a time unless said, with what a parallel run needs written
  into the recipe), and the boundary of `crawl.skipped_extensions` that
  a mutant survived has its test (F47).

- A human `back` step of a flow (the keyboard's `Alt` and left arrow)
  reported success when the tab had no page to go back to, after watching
  five seconds of nothing, where the driver's `back` failed after one; and
  it did not see that it had left the page when the entry before had the
  address of the page it left and the browser had kept that document
  (measured on Chrome 155: a page that takes a campaign tag out of its own
  address). Both ways back now go through one watch that asks the page
  itself which document it shows: keys that led nowhere fail the step with
  "there is no page to go back to" within about a second, and a page that
  came back is arrived on, whatever its address (F38).
- A human `scroll` step of a flow on a page too long for the step's time
  scrolled until that time, finished the flick and the rest it was in, and
  was then failed by the same time ("scroll did not finish within 30000
  ms"), found by the live check of jobs on bot.sannysoft.com. It now stops
  two seconds before its step's time and sits out no rest past it: the
  page is scrolled as far as the time allows and the step succeeds. Give
  the step a longer `timeout_ms` to scroll further.
- One Firefox persona in thirty that claims Windows had no
  `navigator.oscpu` in its generated configuration and showed the engine
  host's `Linux x86_64` in its window. The generator now writes the stock
  value of the claimed system when the device record has none.
- The stand-in persona of `kage selftest browser --proxy` took its
  languages from the caller, and the live suite passed `de-DE, de, en` for
  both engines, a list no stock Firefox sends. Its time zone, locale and
  languages are now derived from the proxy's geo record, like a persona's.

- Reading a page no longer grants it user activation on either engine
  (spec §10.2): both drivers marked every evaluation as a user gesture, so
  a page nobody touched read `navigator.userActivation` as active about
  300 ms after `load` and Chrome let an `AudioContext` run.
- `kage-proxy/tests/reputation.rs` no longer fails by chance (follow-up
  F11): its windows were shorter than the spread of the first lookups for
  about one set of random proxy ids in four. The windows now cover the
  spread and the assertions are per address.

### Changed

- The YAML of `kage.yaml`, the policy files and the static proxy list is
  read by `serde-saphyr` 1.3.0 instead of `serde_yaml` over
  `unsafe-libyaml` (F45; RUSTSEC-2024-0449, which `deny.toml` no longer
  ignores). A file that loaded before can fail now, and the message says
  why: a key said twice in one mapping is an error with its line and
  column (the last one won before, silently); a merge key `<<` is an error;
  unquoted text that YAML reads as a number (`0123456`, `007`, `1_000`) is
  refused where text is expected, and `kage config check` names the key and
  says to write it in quotes; `1e400` and `.inf` are refused. A byte order
  mark is accepted now. A document nests at most 128 levels and holds at
  most a million nodes and 16 MiB of text, and a hostile one is refused in
  milliseconds. On a hot reload an invalid file keeps the last good policy
  and the alert gives the line. The shipped files read exactly as before.
- The shipped default of a domain's `robots` is `ignore` (it was
  `report`, which nothing acted on before M11). A reader never asks for
  `/robots.txt`, so with `report` as the default every site Kage is ever
  pointed at would have been asked for it. Set `robots: report` or
  `robots: enforce` for the sites whose verdict is worth one marked
  address; `defaults: { robots: report }` restores the old meaning for
  all of them.
- `cache_ttl_s` of a fetch request is optional (it was a number that had
  to be 0): left out, the site's policy decides; 0 is "never from the
  cache". `extract` of a fetch request is `{ markdown, links }` (it was
  free-form and refused). The error code `not_available_yet` is gone:
  nothing answers it any more.
- A crawl's sitemaps are asked for by the site's scout, which is
  remembered across crawls (`site_scouts`) and kept out of every fetch
  of the site, not by a persona chosen for each crawl. A scout the site
  turned away is not waited for and not replaced: the files it could not
  ask for are `no_scout`.
- A key's daily page budget counts the requests that were fetched: one
  the response cache answered is no page.

- `timeout_ms` of a browser attempt is the time to reach the page (the
  navigation, its waits, the challenge wait) and bounds nothing after it
  (F44). How long a persona goes on scrolling is a patience Kage draws
  anew for every page from the attempt's seed (`browser.patience`: floor
  6 s, median 38 s, p95 110 s, redrawn above 4 min, at the persona's pace)
  and sends with every navigation it makes: a fetch, an item of a batch, a
  page of a crawl, a warm-up visit (sidecar contract 1.3.0,
  `behavior.patience_ms`). The dwell follows in full, and the attempt is
  waited for as long as the three and a margin add up to. Before, the
  scrolling shared `timeout_ms`, and every page longer than that was left
  at the same instant: 56.0 s after it was asked for, six times of six, in
  both browsers. A consumer that needs the bottom of a tall page uses a
  flow's `scroll` step.
- The sidecar client waits for a navigation as long as that navigation may
  take and not as long as every call may (`navigation_bound`).

- `cargo deny` ignores RUSTSEC-2024-0449 (`unsafe-libyaml` is unmaintained)
  until 2026-12-31: the crate comes in through `figment` and `serde_yaml`
  and reads the operator's own configuration and policy files only. The
  advisory was published in October 2026 and failed the gate for every
  checkout. Moving the YAML reading to a maintained parser is follow-up
  F45 (M12).
- The forwarding headers (`X-Forwarded-For`, `Forwarded`, `X-Real-IP`,
  `True-Client-IP`, `X-Client-IP`, `CF-Connecting-IP` and the rest of the
  family) are on the caller-header denylist (F34).
- A flow's `back` and `wait_for url` no longer hang when the page comes
  back from the browser's cache, and a capture after `back` reports the
  page that came back (F21). A driver `back` with no history fails.
- `job_items.request_id` is no longer a foreign key (migration 0013).
- `server.bind` is `127.0.0.1:8080` unless set (was `0.0.0.0:8080`): the
  API listens on loopback by default (spec §19). The compose file sets
  `0.0.0.0:8080` inside the container and publishes the port on the host's
  loopback, as before. A `kage serve` outside compose that other machines
  reach needs `server.bind`, and a reverse proxy that terminates TLS.
- A stored result names the cookies a site set without their values
  (`[redacted]`) unless `security.expose_set_cookie` is on; the cookies
  themselves are in the persona's jar as before.
- Every request to the API but `/healthz`, `/readyz` and `/openapi.json`
  needs an API key.
- `scripts/check-npm-licenses.mjs` takes the project to check as an
  argument (the sidecar without one) and reads the `license-policy.json`
  next to it.
- The sidecars' `SIDECAR_CONTEXT_IDLE_MS` is 6 minutes in compose (was 2):
  Kage closes a persona's context itself, and the sidecar's limit is the
  backstop.
- Mutation testing of `kage-core` is a gate: `just mutants` fails under
  a score of 70 % (`scripts/check-mutation-score.mjs`). It takes about
  two hours and is not part of `just ci`.
- The Firefox engine refuses a Camoufox build whose `omni.ja` is not the
  one with the Juggler backport, and removes a profile's `startupCache`
  when the profile last started under another archive (a note,
  `omni.sha256`, in each profile directory). An existing developer
  install needs `just setup-camoufox` once.
- A Firefox persona answers `navigator.getAutoplayPolicy()` as a stock
  Firefox does (`allowed-muted`, `disallowed`) and keeps an `AudioContext`
  suspended until its user acts: the launch plan sets
  `media.autoplay.default` to Firefox's default, where the Camoufox build
  ships "allow". The probe's `automation.audio_autoplay` warns otherwise.
- A self-test against a probe page older than M5.1 reports
  `automation.user_activation` as a warning: update the copy on your echo
  host from `tests/fixtures/site/probe/`.
- The sidecar contract is 1.1.0: `behavior` on navigate and flow requests
  and on their results. Nothing was removed.
- `ua-parser-js` is overridden to its MIT 1.0.x line and the license
  exception is gone (ADR 0008). The release workflow does not publish the
  Firefox image: the Camoufox build bundles Microsoft's and Apple's fonts.
- The default persona mix is chromium on linux 40 %, firefox on windows
  45 %, firefox on macos 15 % (spec §6.1).
- A chromium persona's window sits below a 32 px top bar that its display
  announces, so `screen.availHeight` is smaller than `screen.height`.
- The sidecars need the shared secret on every route, the health endpoint
  included; the container healthcheck sends it.
- `sidecar` in the configuration gained each engine's URL, the timeouts
  and the connection retries.
- The JA4 the echo computes leaves GREASE out of the signature algorithms
  as well, which Chrome 155 now sends there.

- The health probe is built by `kage-http`. `proxies.health.profile` must
  name a desktop Chrome or Firefox profile (Edge, Safari, OkHttp and the
  rest are refused), and Firefox on Linux is refused; `kage config check`
  says so. The default (`chrome_149` on Windows) is unchanged.
- `kage proxy health` shows the time each proxy took to open the tunnel and
  to finish the TLS handshake, next to the total.
- `kage config check` validates the http path too and reports the profiles
  this build serves under `http`.
- `ProxyEndpoint` moved from `kage-proxy` to `kage-core`; `kage-proxy`
  still exports it.
- `KAGE__SECRETS__MASTER_KEY` must be 32 random bytes written as 64
  hexadecimal characters; any other value now fails at startup. `just setup`
  has always generated it in this form.
- The `proxies.username` column holds ciphertext (migration 0008). The
  migration adds the columns the pool state machine needs and lets a retired
  proxy share its provider id with its successor.
- `kage config check` also validates the proxy pool settings (the probe's
  emulation profile must exist in this build) and reports which proxy
  provider is in force.
- A secret variable that is set but empty no longer clashes with its `_FILE`
  form: the file is used.
- `kage serve` takes its listen address from `server.bind`
  (`KAGE__SERVER__BIND`) or `--bind`, and fails at startup when a required
  secret is missing. `deploy/.env` gained `KAGE_SIDECAR_TOKEN` and
  `KAGE_MASTER_KEY`; `just setup` adds them to an existing file.

[Unreleased]: https://github.com/Serpe-nt/Kage/compare/v2.0.4...HEAD
[2.0.4]: https://github.com/Serpe-nt/Kage/compare/v2.0.3...v2.0.4
[2.0.3]: https://github.com/Serpe-nt/Kage/compare/v2.0.2...v2.0.3
[2.0.2]: https://github.com/Serpe-nt/Kage/compare/v2.0.1...v2.0.2
[2.0.1]: https://github.com/Serpe-nt/Kage/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/Serpe-nt/Kage/compare/v1.0.2...v2.0.0
[1.0.2]: https://github.com/Serpe-nt/Kage/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/Serpe-nt/Kage/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/Serpe-nt/Kage/releases/tag/v1.0.0
