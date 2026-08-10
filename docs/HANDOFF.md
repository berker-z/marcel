# Marcel session handoff

**Prepared:** 2026-08-10
**Branch:** `master`
**Workspace:** `/home/berkerz/Projects/marcel`

Read `AGENTS.md` first, and `CLAUDE.md` for how to run the checks without one
approval prompt per command.

## Where things stand

[Sprint 19](sprints/019-application-global-operations.md) is implemented: the
operation journal, clipboard, busy lock, progress, cancellation, and every
operation task now live on one application-global `OperationCoordinator`, and
the bookmark list has its own application-global store. Windows are surfaces —
they start work, show its questions and reports, and fold its broadcast effects
into their own projection.

That closes the first item in [Sprint 17](sprints/017-stability-and-architecture-hardening.md)'s
queue, and Stage 5 of [`review-2026-08-10.md`](review-2026-08-10.md).

**Its acceptance run has not happened.** The unchecked boxes in Sprint 19 all
need two real windows in a graphical session, and none of them can be reached by
`cargo test`. Start there before taking new work.

## Next: the rest of Sprint 17's unchecked boxes

In order:

1. Carry a pre-commit object key into every post-commit identity refresh, and
   into `delete_trash_backings` from `purge_trash_records`.
2. Make undo of copy and archive output quarantine-first, reusing `delete_ops`.
3. Journal-wide snapshot budget; Move still has no per-operation limit.
4. Physical rather than lexical comparison for the Trash-root delete guard.
5. Surface malformed Trash entries instead of dropping them from the listing.

## Three things that will save time

**Upstream first, and record it when there is nothing to take.** Yazi is at
`319f90e0`, Nautilus at `f67b2e1`; clone them into the scratchpad. Sprint 19's
upstream section is what a verified one looks like — file and line for every
claim. Do not write an upstream finding you have not opened the file for; both
projects are easy to misremember, and this repository's review documents are
trusted precisely because they cite the exact path.

**A rename or a removal moves an inode's ctime.** An identity recorded before
that moment cannot be compared after it. This caused three separate defects in
Sprint 18, each looking like an operation refusing its own work as though
someone else had interfered. Queue items 1 and 2 above are both in this area.

**Never drop a GPUI `Task` from inside itself.** `finish_active` used to take
the operation's own task handle, which is harmless only as long as nothing
follows it in that future. Sprint 19 added a step after it, so the handle is now
deliberately left for the next operation to replace.

## Running it

See `CLAUDE.md`. In short: capture the `nix develop` environment once, then run
fmt, clippy, and tests as one sandboxed command. Confirm a green
`desktop_integration::tests::private_session_bus_integration` outside the
sandbox — it needs `dbus-run-session`. The suite is 234 library tests plus 1
binary test.

Automated checks did not catch a single one of Sprint 18's three interface
defects, and Sprint 19's remaining acceptance is entirely multi-window
behaviour. Drive the real windows before calling interaction work done.
