<!-- Title: conventional commit, e.g. "feat(m3): http path". One milestone, one PR. -->

## Milestone

<!-- M<N> and its title from docs/02-plan.md. -->

## Acceptance evidence

<!-- One entry per acceptance criterion of the milestone: what was run and what it showed. -->

- [ ] Criterion:
  - Evidence:

## Definition of done

- [ ] New code has tests at the layer docs/01-spec.md §21 names
- [ ] `just ci` passes locally
- [ ] No new `unwrap`/`expect` in library crates outside tests
- [ ] Public types documented; OpenAPI regenerated and committed if touched
- [ ] `docs/versions.md` updated for any new or bumped dependency
- [ ] `CHANGELOG.md` has an entry under Unreleased

## Deviations from the spec

<!-- Anything the spec got wrong or that was impossible, and what was done instead. "None" if none. -->
