# Marcel session handoff

**Prepared:** 2026-08-10
**Branch:** `master`, pushed and clean at `e4865a4`
**Workspace:** `/home/berkerz/Projects/marcel`

Read `AGENTS.md` first.

## Where things stand

[Sprint 18](sprints/018-destination-conflict-decisions.md) is implemented and
accepted: an occupied destination now asks instead of failing, through paste and
drag-and-drop, with skip, rename, replace, merge, and cancel. Its one deliberate
gap is merging a folder while *moving* it.

[`review-2026-08-10.md`](review-2026-08-10.md) cross-checks a third external
review. Its transaction-integrity findings are closed.

## Next: Sprint 17's unchecked acceptance boxes

They are the queue, in order:

1. **Application-global operation owner.** Closing a window can still orphan
   filesystem work, and the journal and busy lock are per-window. Sprint 18
   constrained this: interactive operations must reach a user interface, and a
   blocked worker must never outlive the UI that can answer it.
2. Carry a pre-commit object key into every post-commit identity refresh, and
   into `delete_trash_backings` from `purge_trash_records`.
3. Make undo of copy and archive output quarantine-first, reusing `delete_ops`.
4. Journal-wide snapshot budget; Move still has no per-operation limit.
5. Physical rather than lexical comparison for the Trash-root delete guard.
6. Surface malformed Trash entries instead of dropping them from the listing.

## Two things that will save time

**Upstream first, and record it when there is nothing to take.** Yazi is at
`319f90e0`, Nautilus at `f67b2e1`; clone them into the scratchpad. Yazi has no
filesystem undo and no conflict interaction, which is itself a finding worth
stating rather than rediscovering.

**A rename or a removal moves an inode's ctime.** An identity recorded before
that moment cannot be compared after it. This caused three separate defects in
Sprint 18, each looking like an operation refusing its own work as though
someone else had interfered.

## Running it

The suite needs the Nix dev shell; a plain shell fails building
`yeslogic-fontconfig-sys`. `desktop_integration::tests::private_session_bus_integration`
needs `dbus-run-session` and fails under a restrictive tool sandbox, so confirm
a green run outside one.

Automated checks did not catch a single one of Sprint 18's three interface
defects. Drive the real window before calling interaction work done.
