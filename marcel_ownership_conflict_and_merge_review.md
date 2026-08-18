# Ownership, Conflict, and Merge Review

_Retained verbatim as supplied. Review D._

| **Reviewed tree** | `master` @ `2cf4eac` |
|-------------------|----------------------|
| **Review date**   | 18 August 2026 |
| **Scope**         | The ~24 commits since the hardening baseline: `operations.rs`, `file_ops.rs`, `conflict.rs`, bookmarks, surface routing, and the Sprint 19 ownership work |
| **Cross-check**   | [`docs/review-2026-08-18.md`](docs/review-2026-08-18.md) |
| **Plan**          | [`docs/sprints/020-cleanup-interlude.md`](docs/sprints/020-cleanup-interlude.md) |

---

Yeah. I went through the current master, with particular attention to the ~24 commits since the last hardening baseline, operations.rs, file_ops.rs, conflict.rs, bookmarks, surface routing, and the Sprint 19 ownership work.

Overall: this is substantially better code. I would now describe Marcel as a serious alpha rather than an impressive prototype with some terrifying filesystem semantics. Claude did good work here. The application-global coordinator is the right architecture, the operation result model is much more rigorous, and the conflict machinery is unusually thoughtful. But I found one systemic data-recovery problem I consider genuinely serious, plus one clean violation of your new transaction model in directory merge.

The biggest win is the new OperationCoordinator. This is basically what I wanted last time. One application-owned journal, clipboard, busy lock, cancellation state, progress and task ownership, while windows become projections/surfaces. Effects are broadcast to all windows, while reveal stays origin-specific. That's a coherent ownership model, not just another controller extracted from app.rs. The companion surface abstraction is also small and sensible: original window, then active window, then another live window, then nobody.

More importantly, your filesystem semantics have developed an actual vocabulary. CommittedOperation means “the disk changed, regardless of whether bookkeeping survived.” MutationOutcome::{Unchanged, Committed, Discarded} distinguishes failures before and after a commit boundary. TransferOutcome accounts separately for completed, failed, skipped, already-in-place and cancelled sources. This is exactly the sort of explicit state model filesystem code needs.

The old ugly stuff has mostly been dealt with properly too. Copy publication uses private staging plus no-replace rename. Self-containment resolves real paths, so the symlink recursion disaster is addressed. Move snapshots are bookkeeping rather than a prerequisite for the move succeeding. Visible changes come from actual completed transfers instead of inferred undo records. The no-UI conflict path also turned out to be correct: although ConflictPolicy itself uses Skip as its noninteractive default, plan_source intercepts noninteractive occupied destinations and converts them into visible refusal failures before that decision is reached.

Conflict resolution itself is good. Independent replace-all/merge-all/rename-all/skip-all state is exactly right. The blocking resolver is acceptable because the wait occurs on your blocking worker, and losing the receiver produces cancellation instead of a stranded worker. The naming implementation even handles raw Unix names and avoids the stupid foo (2) (2).txt pattern.

Now the bad part.

1. Directory merge violates your own commit-boundary model

This is the clearest bug I found.

merge_directories() plans the merge first, which is good. Then it starts creating directories and copying files. But after the first successful mutation, subsequent operations still use ordinary ?. If directory 1 gets created, file A gets copied, and file B fails, merge_directories() returns plain Err. The successfully created material disappears from the return value.

Then transfer_paths_impl() handles that Err simply by adding the source to failures. It doesn't receive the partial created set, doesn't put those mutations in completed, and doesn't create an undo record for them.

So you've recreated the exact class of bug CommittedOperation was invented to eliminate:

filesystem changed → function returns failure → caller thinks there was no committed effect.

It's less catastrophic than the old version because merge is additive, so you aren't normally destroying existing destination data. But you can leave half a merge behind with no proper reconciliation and no Undo. The source comment literally says a partially failed merge “has added a subset of what it planned, which is describable.” Right now it isn't described.

I'd fix this before considering the conflict sprint truly done. merge_directories needs an outcome carrying the exact additions even on failure, something like MergeOutcome { created, failure }, or just use your existing commit-boundary vocabulary. After the first successful write, plain Result is the wrong type.

2. Your replacement quarantine has a real data-loss edge

This one is nastier.

When replacing an existing destination, Marcel moves the original into a .marcel-replaced-* quarantine first. Excellent. If publishing the replacement then fails, Marcel tries to restore the quarantined original. Also excellent.

But if that restoration fails, you record a failure and leave the original sitting in the quarantine. It isn't entered into the journal or some separate recovery structure.

Your abandoned-quarantine collector later reasons that a .marcel-replaced-* file owned by a dead PID must be unreachable garbage and deletes it.

Those two assumptions don't compose.

A failed replacement followed by failed restoration means the quarantine contains the user's original data, still required for recovery. After that Marcel process dies, the reclamation code can classify it as abandoned replacement garbage and erase it.

There's a related version during Undo. Undo removes the replacement and calls restore_replaced_items. If restoring one of several quarantined originals fails after crossing the commit boundary, you return Discarded, the old history record isn't reinstated, and any still-quarantined original is no longer reachable from the journal. The coordinator's shutdown cleanup only drains records still in the journal, so that orphan isn't protected by the application owner either.

That's the one I'd treat as P0.

You need two concepts, not one prefix:

replacement undo storage, which may be garbage-collected once its successful replacement record becomes unreachable, and recovery storage, which exists because Marcel failed to restore something and therefore must never be silently deleted.

If rollback restoration fails, promote that quarantine into recovery state. Rename it, persist a recovery marker, whatever. On startup, surface it. Do not sweep it.

3. Quarantine deletion trusts the path more than the identity

You already store FileIdentity in ReplacedItem, which is good. But erase_replacement_quarantine() receives only a path, stats whatever happens to exist there, and recursively deletes it. The comment says nothing else can have taken the path because Marcel created it by atomic rename. That's only true at creation time. Another process can remove it and place something else there before journal eviction or shutdown cleanup.

This is almost comical because the rest of Marcel is now religious about identity validation.

I'd change deletion to consume/inspect the ReplacedItem, verify device/inode/ctime, and refuse cleanup on mismatch. For the startup scanner, where you don't have the identity anymore, err conservative.

Not as reachable as #2, but it's exactly the kind of race you hardened everything else against.

4. Merge silently defeats the undo snapshot budget

COPY_UNDO_SNAPSHOT_LIMIT purports to bound one operation's snapshot cost. Ordinary copy threads a remaining budget across sources. Good.

Merge calls copy_one(..., COPY_UNDO_SNAPSHOT_LIMIT) independently for every leaf, then accumulates all of their snapshots into one giant files vector.

Because each merge leaf gets a fresh 100k budget, a huge directory union can generate essentially unbounded snapshots in a single operation record. This is P2, not scary corruption, but it undermines the explicit resource-control model you're building. The merge needs one shared remaining budget for the whole operation.

There are some smaller things. The replacement quarantine filename prepends Marcel metadata to the original filename without respecting NAME_MAX, so replacing an already-near-255-byte filename can fail simply because the hidden quarantine name is too long. OperationCoordinator is also already ~1,300 lines and contains both operation orchestration and the GPUI conflict-dialog implementation. I wouldn't refactor it yet because it's still cohesive, but I would probably eventually move the surface-specific conflict presentation out. Don't “clean it up” into generic traits or an event framework rn.

And there is one process concern I actually like: your own Sprint 19 doc explicitly admits the application-global ownership migration has not had its two-window graphical acceptance run yet. The unit-testable state machine passes, but closing origin windows, cross-window Undo, conflict rehoming, shared clipboard and shared bookmarks remain unchecked manually. Given that you already found GUI bugs in this work that sailed through ordinary tests, I wouldn't consider Sprint 19 accepted until you run that list. That's good engineering discipline, btw. You're not pretending the tests prove things they cannot prove.

As for Claude's quality specifically: good. Better than I expected for this kind of work. This isn't random AI slop with twelve abstraction layers. The resulting types capture real invariants, the ownership migration is conceptually correct, error states are substantially more explicit, and there's clearly been a lot of adversarial thinking.

But the merge bug is also extremely characteristic of LLM-written systems code. The comments understand the invariant perfectly. The types elsewhere encode it perfectly. Then one fresh implementation path casually uses ? after the first irreversible mutation and violates the entire thing. The prose is better than the proof. That's exactly where you still need review.

So my score would be something like: architecture 9/10, ordinary code quality 8/10, filesystem-safety model 8.5/10 after the last round, current release readiness 6.5/10 until #1/#2 are fixed and Sprint 19 gets its manual run.

The difference from what I saw last time is huge. The old blockers weren't “fixed until the tests pass”; the architecture actually absorbed the lessons. I would keep Claude on this. I just wouldn't let Claude declare its own filesystem invariants satisfied. That's my job apparently lol.
