# Sprint 3: Safe file operations

**Status:** Implemented — the planned safe-operation slices and automated
checks are complete. Later clipboard interoperability and conflict UX remain
separate parked work.

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
  mounts `Root::render_dialog_layer` in its top-level render tree and mounts
  the public notification entities in a bottom-right stack as a compatibility
  bridge. The custom notification mount is necessary because 0.5.1 hardcodes
  `NotificationList` to top-right and exposes no placement option. The dialog,
  notification, input, and buttons remain gpui-component implementations.
- Window-wide type-to-filter yields whenever a non-search input owns focus.
  This applies to the New Folder dialog and establishes the routing contract
  for future rename and New File editors.

## Second slice: internal copy, cut, and paste

- [x] `Ctrl+C`, `Ctrl+X`, and `Ctrl+V` use the shared command dispatcher.
- [x] Cut and Copy are active for the complete visible selection in the item
  context menu; Paste is active in both item and current-directory menus when
  Marcel's file clipboard has content.
- [x] Transfers run on the background executor and remain serialized with
  every other write and undo/redo operation.
- [x] Copy supports regular files, directories, and symbolic links without
  following links. Unsupported special files fail visibly.
- [x] A copy is assembled at a hidden staging path and published with Linux
  `RENAME_NOREPLACE`; an occupied destination is never overwritten and an
  interrupted copy is never exposed under its requested name.
- [x] Copy checks cancellation between directory entries and one-MiB file
  chunks. Escape requests cancellation while a transfer is active.
- [x] Cut/paste uses a no-replace rename fast path and currently reports a
  clear error for cross-filesystem moves.
- [x] Multi-item forward transfers record the exact successful subset and
  retain failed cut items in the clipboard.
- [x] Copy and move records store recursive filesystem identities. Undo and
  redo refuse changed, replaced, missing, or occupied paths.
- [x] Unit tests cover recursive file/directory/link copies, occupied
  destinations, cancellation, modified-output conflicts, and move/copy
  undo/redo.
- [x] Active copy and move operations expose a bottom-right progress card with
  preparation state, current item, item/byte totals, and an explicit Cancel
  button. The card and transient notifications share one layout stack and
  cannot overlap.
- [x] Progress is reported through a shared atomic snapshot: copy preparation
  measures the source tree off-thread, byte counts advance per bounded copy
  chunk, and same-filesystem moves advance per top-level item. GPUI polls at a
  bounded interval rather than receiving an event for every chunk.

### Deliberate limits of this slice

- The file clipboard is session-local. Desktop `text/uri-list` and
  `x-special/gnome-copied-files` interoperability remains required.
- Cross-filesystem cut/paste is deliberately parked until a verified
  copy-then-remove design is scheduled; Marcel does not silently fall back to
  a riskier implementation.
- Destination-conflict decisions are deliberately parked. The current
  no-overwrite failure remains the complete policy until that interaction is
  designed.
- Queued work and progress for undo/redo or future operation types remain
  follow-up work.
- Successful transfers currently refresh the displayed directory. Filesystem
  watching and Yazi-style incremental list events remain follow-up work.

## Third slice: internal drag moves and bookmarks

- [x] List and icon entries expose one shared internal file-drag payload. A
  selected item carries the complete visible selection; dragging an unselected
  item carries only that item.
- [x] Navigable browser folders, XDG Places, and Bookmarks are move drop
  targets backed by the existing serialized, no-overwrite transfer engine and
  operation journal.
- [x] No-op drops and attempts to move a directory into itself are rejected
  before dispatch and again in the filesystem layer.
- [x] Bookmarks appear directly below Places with a separator.
- [x] Dropping browser folders anywhere in the unoccupied Bookmarks section
  adds bookmarks without moving or modifying those folders. The link cursor
  and section highlight distinguish this from a filesystem move; bookmark rows
  remain filesystem move targets for their destination folders.
- [x] Bookmark rows navigate on click, accept filesystem moves into their
  target folder, and can be dragged to any ordering position using row bounds
  and a fixed-height insertion indicator.
- [x] Right-clicking a bookmark offers the compact Remove Bookmark action.
  Removal changes only Marcel's bookmark list and never touches the target
  folder.
- [x] Bookmark order is persisted asynchronously as escaped `file:` URLs at
  `$XDG_CONFIG_HOME/marcel/bookmarks` (falling back to
  `~/.config/marcel/bookmarks`). Writes are serialized and atomically replace
  the settings file.
- [x] File drags over the directory browser use the same bounded edge zone,
  proximity acceleration, frame interval, and scroll limits as marquee
  selection. Ordinary pointer hovering never scrolls.
- [x] Selected-file drag payloads are built once per normal render and shared
  by visible selected rows. Active marquee renders skip that payload entirely,
  preventing repeated full-directory scans in very large folders. Single-row
  payloads consume the row's existing directory flag directly, so rendering
  near the end of a large directory does not scan from its beginning.
- [x] Painted entry hit regions retain the row's navigable flag. Drag-hover
  target negotiation therefore examines only painted regions and never
  searches the full directory vector by path, keeping invalid sidebar drops
  independent of the dragged item's position in a very large directory.

### Deliberate drag limits

- Internal filesystem drags currently mean Move. Modifier-selected Copy and
  Link actions need explicit cursor/action negotiation before being enabled.
- Cross-filesystem moves retain the existing safe failure behavior.
- Hover-open folders, dropping on empty browser space, scrollable bookmark
  overflow, and native incoming/outgoing desktop drag-and-drop remain follow-up
  work.

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

Marcel's New Folder path is therefore not a direct Yazi adaptation. It shares
the non-blocking principle but deliberately adds serialized,
identity-validating filesystem undo and forbids overwrite.

The second slice conceptually adapts Yazi's per-item worker outcomes,
cooperative cancellation, partial-success accounting, and rename-first move
path behind Marcel's `file_ops` interface. Marcel adds hidden staging plus
`RENAME_NOREPLACE`, recursive identity snapshots, and general filesystem
undo/redo. No Yazi code was copied. Progress reporting, worker queues,
cross-filesystem copy-then-remove, and incremental list updates have not yet
been adapted.

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
4. Finish copy/cut/paste with desktop clipboard interop, cross-filesystem
   moves, conflict decisions, progress UI, and an explicit cancel control.
5. Permanent deletion only after an explicit confirmation design and
   accessibility review.

## Out of scope for the first slice

- Recursive removal or permanent deletion.
- Overwrite and merge decisions.
- Multi-operation concurrency.
- Cross-process or persistent undo history.
- Pretending an operation is reversible when its validation contract cannot be
  satisfied.
