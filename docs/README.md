# Marcel internal documentation

The root `README.md` is Marcel's public landing page. This directory holds the
durable product, engineering, and release records used to build it.

## Current milestone

Marcel has reached the personal daily-driver milestone. Sprint 15 completed the
free archive baseline, private Iosevka/Nordzy identity bundle, declarative Nix
visual settings, and persistent view/hidden-file interaction state.

The next planned milestone is
[Sprint 16: public release presentation and metadata](sprints/016-public-release-presentation.md).
It prepares the documentation, branded artwork, AppStream metadata, and
platform-neutral installation structure required before `v0.1.0`.

## Source-of-truth documents

- [`TODO.md`](TODO.md): cross-sprint product roadmap and backlog.
- [`release.md`](release.md): release, packaging, artifact, and repository
  submission handbook. It is platform-neutral in intent while documenting Nix
  as the only currently shipped route.
- [`interaction-model.md`](interaction-model.md): conventional file-manager
  interaction and safety contract.
- [`copy-semantics.md`](copy-semantics.md): copy fidelity and symbolic-link
  policy.
- [`external-review.md`](external-review.md): external architectural review
  retained as design input, not an implementation specification.
- [`../THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md): upstream reuse,
  bundled assets, adaptations, and license notices.
- [`sprints/`](sprints/): numbered implementation plans and acceptance history.

`HANDOFF.md` is a historical agent handoff, not a current roadmap or required
per-turn checklist.

## Sprint status convention

- **Planned:** contract and acceptance criteria exist; implementation has not
  started.
- **In progress:** the sprint still has active implementation work.
- **Implemented:** its automated/product slice is present, though named manual
  or release-gate checks may remain.
- **Accepted:** every required automated and manual check is complete.

Older sprint files preserve the status and open checks from their own period.
The current milestone above and `TODO.md` decide what is next.
