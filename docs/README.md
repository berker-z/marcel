# Marcel internal documentation

The root `README.md` is Marcel's public landing page. This directory holds the
durable product, engineering, and release records used to build it.

## Current milestone

Marcel has reached the personal daily-driver milestone. Feature and release work
remain paused for hardening.

[Sprint 20: cleanup interlude](sprints/020-cleanup-interlude.md) is the current
document. It closed [Review D](review-2026-08-18.md)'s findings together with
the remaining code queue from
[Sprint 17](sprints/017-stability-and-architecture-hardening.md),
[Sprint 18](sprints/018-destination-conflict-decisions.md), and
[`review-2026-08-10.md`](review-2026-08-10.md), because those three lists
overlapped in the same files. Its code slice is complete; what remains is the
graphical acceptance matrix it inherits from
[Sprint 19](sprints/019-application-global-operations.md) and Sprint 18, which
`cargo test` cannot reach.

[Sprint 16: public release presentation and metadata](sprints/016-public-release-presentation.md)
remains planned but deprioritized until that matrix is run.

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
- [`review-2026-08-18.md`](review-2026-08-18.md): cross-check of the fourth
  review, the first to read the tree Sprints 18 and 19 produced. Its four
  findings all reproduced; it records a fifth in the same function that the
  review missed, and the evidence for each. The plan is Sprint 20.
- [`review-2026-08-10.md`](review-2026-08-10.md): cross-check of a third review,
  with per-finding verdicts and re-tiering, one rejected finding, three findings
  it missed, and the Yazi and Nautilus evidence that decided the remediation
  plan. Records why Marcel keeps identity validation where Nautilus does not.
- [`../THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md): upstream reuse,
  bundled assets, adaptations, and license notices.
- [`sprints/`](sprints/): numbered implementation plans and acceptance history.

[`HANDOFF.md`](HANDOFF.md) is a short pointer to the current state and the next
queue. `TODO.md` and the sprint documents remain the source of truth; the
handoff only says where to start.

## Sprint status convention

- **Planned:** contract and acceptance criteria exist; implementation has not
  started.
- **In progress:** the sprint still has active implementation work.
- **Implemented:** its automated/product slice is present, though named manual
  or release-gate checks may remain.
- **Accepted:** every required automated and manual check is complete.

Older sprint files preserve the status and open checks from their own period.
The current milestone above and `TODO.md` decide what is next.
