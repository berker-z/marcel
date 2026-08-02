# Sprint 7: system Trash and restore

**Status:** Implemented — Trash, restore, identity validation, and automated
coverage are complete. Desktop interoperability and mounted-volume checks
remain in the manual hardening matrix.

## Goal

Make recoverable deletion a real Marcel operation: move selected items to the
desktop's native Trash, expose the aggregated system Trash as the bottom item
in Places, and restore without overwriting or silently rebuilding paths.

Permanent deletion is not part of this sprint.

## Upstream and standards audit

The storage contract follows the freedesktop.org Trash Specification 1.0:
<https://specifications.freedesktop.org/trash/latest/>.

Yazi was audited at upstream commit
`319f90e0eab185a231eef5562215ba322e320286`:

- `yazi-scheduler/src/file/file.rs` queues Trash work away from interaction.
- `yazi-fs/src/trash/freedesktop/trash.rs` lists and restores freedesktop Trash
  entries and validates that entries belong to known Trash roots.
- `yazi-fs/src/trash/freedesktop/trash_info.rs` securely parses original paths.
- Yazi delegates native Trash placement to the MIT-licensed `trash` crate.

Marcel follows the same scheduler/platform-adapter split and uses `trash` 5.x
for native placement. Marcel-owned code adds exact entry discovery,
filesystem-identity validation, no-replace restoration, partial-success
reporting, and integration with the shared operation journal. No Yazi code was
copied.

## Behavioral contract

- `Delete` and **Move to Trash** operate on the complete preserved selection.
- Moving to Trash runs on the background executor and never falls through to
  permanent deletion.
- Marcel refuses to trash a system Trash root, an object inside one, or an
  ancestor that contains one; manually browsing a backing directory must not
  permit double-trashing or orphan existing metadata.
- Every successful undoable item records its exact `.trashinfo` path, backing
  path, original path, and metadata identities.
- Undo restores only if both Trash objects retain their recorded identities,
  the original parent still exists as a directory, and the original path is
  unoccupied.
- Restore does not silently recreate a missing original parent. This is
  intentionally stricter than Yazi and the `trash` crate.
- Redo trashes the restored object again and records the newly allocated Trash
  entry rather than assuming the previous entry name can be reused.
- Multi-item restore validates all targets before moving any payload. If a
  move then fails, already-restored payloads are moved back to their recorded
  Trash backing paths.
- A failed `.trashinfo` cleanup after the payload has been safely restored may
  leave an orphaned metadata file, but must never turn a successful data
  restore into missing user data.
- The Places Trash view aggregates valid top-level entries from the home Trash
  and mounted-volume Trash roots known to the platform adapter.
- Dragging backing objects directly out of the Trash view is disabled. Future
  drag-in and drag-out behavior must dispatch Trash and Restore commands,
  respectively, rather than ordinary filesystem moves.

## Acceptance checks

- [x] Add the current Rust 5.x `trash` platform adapter without exact-version
  pinning; Cargo currently resolves the malformed-entry-safe 5.2.6 release.
- [x] Run Trash placement and discovery away from GPUI's foreground executor.
- [x] Implement partial-success accounting and exact entry discovery.
- [x] Add identity-validating, no-replace Trash undo and redo.
- [x] Add explicit restore from the aggregated Trash view and make that restore
  itself undoable and redoable.
- [x] Bind `Delete` through the shared command dispatcher.
- [x] Activate Move to Trash/Restore in the item context menu according to the
  current location.
- [x] Add Trash as the bottom-most Places item using the active icon theme's
  semantic Trash icon.
- [x] Add unit coverage for metadata-path validation, occupied targets,
  missing parents, replaced payloads, Trash-root overlap, and successful
  metadata cleanup.
- [ ] Manually verify multi-selection Trash, Undo, Redo, explicit Restore, and
  collision refusal in both list and icon views.
- [x] Manually verify interoperability with Trash entries produced by another
  freedesktop file manager.
- [x] Manually verify a mounted-volume Trash on a disposable test volume.

## Follow-up

- Generalize browser locations so trashed directories can be navigated
  virtually without exposing their physical backing paths as ordinary folders.
- Add watcher/coalescing support for Trash roots.
- Make dropping ordinary items on the Trash Place dispatch Move to Trash and
  dragging Trash items to filesystem targets dispatch an explicit restore/move
  design.
- Add Empty Trash and permanent deletion only after confirmation,
  accessibility, partial-failure, and non-reversibility rules are specified.
- Decide whether to maintain the optional freedesktop `directorysizes` cache.
