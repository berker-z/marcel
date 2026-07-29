# Sprint 8: permanent deletion and Empty Trash

## Goal

Add explicit, conventional permanent deletion without weakening Marcel's
recoverable-delete default. `Shift+Delete`, the item context menu, selected
items in Trash, and Empty Trash all reach one confirmed, background operation.

Permanent deletion is intentionally never added to Undo history.

## Yazi audit

Yazi was audited at upstream commit
`319f90e0eab185a231eef5562215ba322e320286`:

- `yazi-scheduler/src/file/file.rs` emits per-entry delete work and removes
  leaves before their containing directories.
- `yazi-scheduler/src/file/traverse.rs` explicitly disables symlink following
  for delete traversal.
- `yazi-fs/src/engine/traits.rs` provides the no-follow recursive removal
  contract used by local and virtual filesystem engines.

`src/delete_ops.rs` conceptually adapts those ordering, no-follow, and progress
principles. Marcel adds whole-selection preflight, filesystem identities,
top-level quarantine, paired freedesktop Trash metadata cleanup, and its own
background-worker interface. No Yazi code was copied.

## Safety contract

- `Delete` remains recoverable Move to Trash. `Shift+Delete` requests permanent
  deletion.
- Every permanent-delete entry point opens a modal warning that the operation
  cannot be undone. The destructive confirmation button uses the semantic
  danger variant.
- Before any erasure, every selected top-level path is identity-checked and
  atomically renamed with Linux `RENAME_NOREPLACE` to a unique
  `.marcel-delete-*` quarantine path in its existing parent.
- If staging or traversal planning fails for any selected root, all earlier
  quarantines are renamed back and no erasure begins.
- Traversal uses `symlink_metadata`, never follows symbolic links, and records
  device, inode, ctime, and file-mode identities for every planned entry.
- Leaves are removed before directories. Each entry is revalidated immediately
  before removal, along with every planned directory ancestor; a replaced path
  or symlink redirection is refused.
- Nested or duplicate selections collapse to their top-level selected root.
- Ordinary permanent deletion refuses filesystem roots and any path inside,
  equal to, or containing a known system Trash root.
- Once confirmed and preflighted, permanent deletion is not cancellable.
  Stopping halfway would necessarily be a partially destructive result and
  could not be represented as safe cancellation. Progress remains visible.
- An I/O failure after erasure begins is reported as a partial failure. Any
  surviving data stays under the reported `.marcel-delete-*` quarantine name
  rather than being silently discarded or restored as a deceptively complete
  original tree.
- Deleting from Trash first validates the exact payload and `.trashinfo`
  identities. The payload is permanently removed before its matching metadata
  file. A metadata-cleanup failure is reported without pretending the payload
  remains recoverable.
- Empty Trash acts on the exact aggregated entries shown when confirmation is
  opened. Entries created concurrently afterward are not swept into the
  already-confirmed operation.

## Acceptance checks

- [x] Add shared Delete Permanently and Empty Trash commands with centralized
  enabled state.
- [x] Bind `Shift+Delete` to Delete Permanently.
- [x] Activate the concise `Delete` item-menu action in ordinary and Trash
  locations while retaining explicit permanent wording in confirmation.
- [x] Add Empty Trash to the Trash empty-space context menu.
- [x] Use gpui-component confirmation dialogs and danger buttons.
- [x] Keep planning and recursive deletion off GPUI's foreground executor.
- [x] Show bottom-right item/byte progress without offering unsafe
  cancellation.
- [x] Implement atomic top-level quarantine, no-follow traversal,
  leaf-before-directory removal, and identity revalidation.
- [x] Permanently purge selected Trash entries with paired metadata cleanup.
- [x] Add tests for recursive deletion, symlink non-following, nested
  selections, quarantine collisions, and paired Trash purge.
- [ ] Manually verify `Shift+Delete`, both item-menu variants, and their
  confirmation wording in list and icon views.
- [ ] Manually verify Empty Trash from the Trash empty-space menu.
- [ ] Manually verify that cancelling the confirmation performs no writes.
- [ ] Manually induce an occupied/read-only child and verify the partial-error
  message identifies a recoverable quarantine remnant.

## Follow-up

- Add crash-startup discovery and a recovery UI for `.marcel-delete-*`
  quarantine remnants.
- Consider a typed-confirmation preference for unusually broad deletions, but
  do not normalize confirmation fatigue by prompting more than the risk
  requires.
- Add a Trash toolbar Empty action only if testing shows the context-menu
  action is insufficiently discoverable.
