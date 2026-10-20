<!--
Title: a conventional commit, e.g. "feat(proxy): a provider for Example Proxies" or "fix(core): …".
One change, one pull request. A vulnerability goes privately (SECURITY.md), never here.
No key, proxy credential, proxy address or personal data anywhere in this pull request.
-->

## What and why

<!-- What changes, for whom, and why. Link the issue, the milestone of docs/02-plan.md, or the decision (D-number, ADR). -->

## Acceptance evidence

<!-- One entry per acceptance criterion (the milestone's, the issue's, or your own): what was run and what it showed. -->

- [ ] Criterion:
  - Evidence:

## Definition of done

- [ ] New code has tests at the layer `docs/01-spec.md` §21 names; a new module of a core crate keeps 80 % line coverage
- [ ] `just ci` passes locally
- [ ] No new `unwrap`, `expect` or `panic!` in a library crate outside tests
- [ ] Public types and functions documented; OpenAPI documents and the API test client regenerated (`just openapi`) and committed if a type of theirs changed
- [ ] `docs/versions.md` has a row for every new or bumped dependency, with its licence; a fingerprint-critical pin moves only in a pull request of its own
- [ ] `CHANGELOG.md` has a line under Unreleased, breaking changes first
- [ ] Conventional commits; nothing secret in the code, the tests, the fixtures or the logs

## Deviations from the spec

<!-- Anything the spec got wrong or that was impossible, and what was done instead, with an ADR draft when it changes a decision. "None" if none. -->
