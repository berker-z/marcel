# Sprint 3: Safe file operations

## Goal

Turn Marcel from a read-only browser into a trustworthy file manager. Every
mutation must run away from GPUI's foreground executor, refuse implicit
overwrites, report failures, refresh affected views, and participate in a
bounded undo/redo history when it is genuinely reversible.

## First slice: New Folder

- [x] `New Folder` is active in the current-directory context menu.
- [x] A gpui-component dialog collects and validates the folder name.
- [x] Creation runs on the background executor and uses one atomic
  `create_dir` call; existing destinations are never overwritten.
- [x] Successful creation refreshes the directory and selects the new folder.
- [x] Failures are presented through a gpui-component notification.
- [x] Completed creates enter a bounded in-memory undo stack and clear redo.
- [x] `Ctrl+Z` undoes the create only when the path is still the exact directory
  Marcel created and remains empty.
- [x] `Ctrl+Y` recreates it only when the destination remains unoccupied.
- [x] Undo/redo and new writes are serialized.
- [x] Top-left Undo and Redo buttons reflect the shared journal and busy state;
  Refresh remains available from the current-directory menu.
- [x] Unit tests cover validation, occupied destinations, identity conflicts,
  non-empty undo refusal, history branching, and the history bound.

## Implementation notes

- gpui-component 0.5.1's `Root` stores dialog and notification state but does
  not attach its public dialog/notification layers in `Root::render`. Marcel
  mounts `Root::render_dialog_layer` and `Root::render_notification_layer` in
  its top-level render tree as a compatibility bridge. The dialog,
  notification, input, and buttons remain gpui-component implementations.
- Window-wide type-to-filter yields whenever a non-search input owns focus.
  This applies to the New Folder dialog and establishes the routing contract
  for future rename and New File editors.

## Yazi study notes

Yazi commit `319f90e0eab185a231eef5562215ba322e320286` was reviewed before
expanding Marcel's file-operation layer:

- Yazi's create actor runs asynchronously, coordinates with its watcher,
  creates through its VFS engine, publishes an incremental `FilesOp`, and
  reveals the result.
- Long copy, cut, delete, and trash operations flow through a dedicated
  scheduler with worker pools, priorities, progress, cancellation tokens,
  unique-destination handling, and a rename fast path for moves.
- Yazi's source currently provides undo/redo snapshots for its text input
  widget, not a general filesystem-operation undo journal.

Marcel's current New Folder path is therefore not a direct Yazi adaptation. It
shares the non-blocking principle but deliberately adds serialized,
identity-validating filesystem undo and forbids overwrite. Before copy, move,
trash, or recursive operations land, Marcel should adapt Yazi's scheduler,
incremental-update, progress, and cancellation patterns behind Marcel-owned
interfaces and record the exact adaptation in `THIRD_PARTY_NOTICES.md`.

## Safety contract

The initial create record stores the destination and the filesystem identity
observed immediately after creation. Undo checks that identity again and then
uses `remove_dir`, which succeeds only for an empty directory. Marcel must stop
with a conflict if another process replaced the path or if the directory gained
contents. It must never recursively delete during create undo.

Redo repeats the original atomic creation only after confirming that no entry
occupies the destination and stores the newly created identity in the updated
record. Completing a new forward operation clears redo history.

The operation history holds at most 100 completed records. Navigation,
selection, filtering, and preview actions do not change it. Only successful
filesystem effects enter history.

## Follow-up slices

1. New File using the same create contract.
2. Rename with source and destination identity checks.
3. Move to Trash with freedesktop Trash metadata and restore.
4. Copy/cut/paste with desktop clipboard interop, conflict decisions,
   cancellation, progress, and partial-success records.
5. Permanent deletion only after an explicit confirmation design and
   accessibility review.

## Out of scope for the first slice

- Recursive removal or permanent deletion.
- Overwrite and merge decisions.
- Multi-operation concurrency.
- Cross-process or persistent undo history.
- Pretending an operation is reversible when its validation contract cannot be
  satisfied.
