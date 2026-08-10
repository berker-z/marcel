# Marcel internal documentation

The root `README.md` is Marcel's public landing page. This directory holds the
durable product, engineering, and release records used to build it.

## Current milestone

Marcel has reached the personal daily-driver milestone. Feature and release
work are now paused for
[Sprint 17: stability and architecture hardening](sprints/017-stability-and-architecture-hardening.md).
It closes confirmed correctness and hostile-input problems, pays down manual
validation debt, and mechanically extracts cohesive ownership from the
application coordinator.

[Sprint 18: destination conflict decisions](sprints/018-destination-conflict-decisions.md)
runs alongside it. An occupied destination is now a question rather than a
failure, and it precedes Sprint 17's application-global operation owner because
interactive operations must be able to reach a user interface.

[Sprint 16: public release presentation and metadata](sprints/016-public-release-presentation.md)
remains planned but deprioritized until the hardening sprint is complete.

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
- [`review-2026-08-05.md`](review-2026-08-05.md): two cross-checked operation
  layer reviews with per-finding verdicts, reproductions, and remediation
  status. Unlike `external-review.md`, its confirmed findings were defects.
- [`review-2026-08-10.md`](review-2026-08-10.md): cross-check of a third review,
  with per-finding verdicts and re-tiering, one rejected finding, three findings
  it missed, and the Yazi and Nautilus evidence that decided the remediation
  plan. Records why Marcel keeps identity validation where Nautilus does not.
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
