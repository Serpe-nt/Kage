# Upgrading an engine

A new Chrome or a new Camoufox build changes what every persona of that
engine looks like to a site: it is fingerprint-critical
(`docs/versions.md`) and goes in a pull request of its own, never with
anything else and never merged by a bot. The steps of each engine are in
its runbook; this page is what they share and the order to keep.

| Engine | Procedure |
|---|---|
| Chromium (patchright with the pinned Chrome) | [engine-chromium.md, "Moving to a new Chrome"](engine-chromium.md#moving-to-a-new-chrome) |
| Firefox (Camoufox) | [engine-firefox.md, "Moving to a new Camoufox build"](engine-firefox.md#moving-to-a-new-camoufox-build) |

## What every upgrade does

1. **Pin the new build** by version and checksum in
   `deploy/docker/sidecar.Dockerfile` (and, for Camoufox, the constants of
   `engine-firefox/src/host.ts`). The image build and `just setup` read the
   same lines; an engine refuses any build but the pinned one.
2. **Run the sidecar's tests on it**: `just test-node`. The parity suite
   runs the same contract, capture and behaviour tests on both engines,
   in the real browsers.
3. **Record the browser's fingerprint** (`just
   capture-browser-fingerprint [firefox]`) and decide what the new major
   may do, as "A new browser major" in
   [fingerprint-echo.md](fingerprint-echo.md#a-new-browser-major) says:
   whether the http path may stand in for it, or the major is
   browser-only.
4. **Tell the deployment** the new major: `engines.chromium.browser_major`
   or `engines.firefox.browser_major` (`KAGE_CHROMIUM_BROWSER_MAJOR`,
   `KAGE_FIREFOX_BROWSER_MAJOR` in compose). The stack's smoke test fails
   when the image and the setting disagree.
5. **Test the personas**: rebuild the images (`just build-images`), start
   the stack, run `kage persona selftest <id>` on a persona of the engine
   against your own pages, then the live suite once
   ([repo.md](repo.md#nightly-live-suite)).
6. **Write it down**: `docs/versions.md` (the row and the verification
   log) and the changelog.

## On a running deployment

- **Personas move with their engine.** A persona's document names the
  browser it was made for. At its first open after the deploy (real
  work, a warm-up visit or a self-test, whichever comes first) the
  engine refuses it (`browser_major_mismatch`), kage has the sidecar move
  it (`POST /v1/personas/upgrade`: the browser version and the
  `User-Agent` move, the device, the place and the seed do not, and the
  same document always gives the same answer), stores it, records
  `persona_browser_upgraded` and opens the context as the moved persona.
  Its sessions, the cookies in its profile, its http jar and its
  per-domain state continue, as a real browser's update leaves them. Its
  wreq profile becomes the one the new major resolves to; for a
  browser-only major that is none, and its http path stays off.
- **Set the major and the image together.** kage stores a move only to
  the major `engines.<engine>.browser_major` names (anything else fails
  the attempt with `persona_error` and the log says why), and
  `kage config check` refuses a major nobody captured.
- **Watch it happen.** `kage persona selftest <id>` on one persona of the
  engine right after the deploy moves it and tests it at once; the others
  move as they are used. Count what moved:

  ```sh
  docker compose --file deploy/compose.yaml exec postgres psql --username kage --command \
    "SELECT details->>'engine', details->>'from_major', details->>'to_major', count(*)
       FROM health_events WHERE kind = 'persona_browser_upgraded' GROUP BY 1, 2, 3"
  ```

- Roll out by recreating the sidecar of the engine (`docker compose up
  --detach --wait browser-firefox`) with kage at the release that pins
  that browser; kage closes the contexts of that engine and opens them
  again at the next page.
- **A Firefox major is one-way.** Firefox moves a profile forward at its
  first start under a newer major and refuses it to an older one
  afterwards (measured on 152 and 156: the older browser gives no context
  within a minute). So before that first start the Firefox engine copies
  the profile, without its caches, to `.upgrade/<persona id>/` on the
  personas volume, with an `upgrade.json` (persona, profile directory,
  the major it came from, when). kage deletes a copy
  `personas.upgrade_backup_days` (7) after it was taken. Chrome was not
  measured; the Chromium engine copies nothing, so treat a Chrome major
  as one-way too and keep the volume's backup.

## Going back

**Before any persona opened under the new image** nothing has moved:
recreate the sidecar from the previous release's image. Check first that
the count above is zero.

**Within `personas.upgrade_backup_days` of the deploy** a Firefox
deployment goes back to the previous release as a whole (kage and both
sidecars from the previous tag: this release's kage refuses the previous
major in its configuration). Every step is needed: the profiles and the
documents go back together.

1. Stop kage and the Firefox sidecar, so that no browser has a profile
   open and nothing opens one:

   ```sh
   docker compose --file deploy/compose.yaml stop kage browser-firefox
   ```

2. Put each kept profile back, and keep the moved one next to its copy
   (`.upgrade/<persona id>.after`) until the rollback has run a day:

   ```sh
   docker compose --file deploy/compose.yaml run --rm --no-deps --entrypoint sh browser-firefox -c '
     cd /var/lib/kage/personas || exit 1
     for note in .upgrade/*/upgrade.json; do
       [ -f "$note" ] || continue
       kept=${note%/upgrade.json}
       profile=$(sed -n "s/.*\"profile_dir\":\"\([^\"]*\)\".*/\1/p" "$note")
       case "$profile" in ""|/*|*..*) echo "skipped $kept"; continue;; esac
       [ -d "$profile" ] && mv "$profile" "$kept.after"
       cp -a "$kept" "$profile" && rm "$profile/upgrade.json" && echo "restored $profile"
     done'
   ```

3. Move the stored documents back. The `persona_browser_upgraded` event
   keeps what each persona was before its move; this restores exactly
   that for every persona that is still of the new major (here 156):

   ```sh
   docker compose --file deploy/compose.yaml exec postgres psql --username kage --command "
     UPDATE personas p
        SET fingerprint   = e.details->'previous'->'fingerprint',
            user_agent    = e.details->'previous'->>'user_agent',
            wreq_profile  = e.details->'previous'->>'wreq_profile',
            browser_major = (e.details->>'from_major')::int
       FROM health_events e
      WHERE e.kind = 'persona_browser_upgraded' AND e.subject_kind = 'persona'
        AND e.subject_id = p.id AND e.details->>'engine' = 'firefox'
        AND (e.details->>'to_major')::int = 156 AND p.browser_major = 156"
   ```

   The number it reports is the number of personas the count above
   showed as moved. A persona generated after the deploy has no earlier
   self: it stays at the new major, and the previous image refuses it;
   retire it (`kage persona regenerate <id>` once the previous release
   runs).
4. Start the previous release: check out its tag, build its Firefox
   image (`docker compose --file deploy/compose.yaml build
   browser-firefox`), and `up` with its configuration
   (`engines.firefox.browser_major` back to the previous major).
5. Check one persona: `kage persona selftest <id>` passes, and
   `kage persona show <id>` names the previous major. After a day,
   remove `.upgrade/` (the previous release does not sweep it).

**After the window** the copies are gone, and going back is a restore of
the personas volume and the database from the operator's backups, taken
at the same moment, or new personas (`kage persona regenerate`, which
inherits nothing). Firefox's `--allow-downgrade` is never used: Mozilla
warns that it can lose a profile's data.
