# Sprint 17: stability and architecture hardening

**Status:** Implemented — the automated hardening slice and post-migration
Wayland drag validation are complete. Feature and release work remain paused
while the outstanding desktop/manual acceptance matrix is run.

## Goal

Treat Marcel as a stable daily-use product whose next work is hardening rather
than novelty. Fix the concrete failures found in external review, strengthen
the boundaries around untrusted files and desktop requests, and mechanically
move cohesive state out of `app.rs` without introducing an event bus or an
abstract trait architecture.

Sprint 16 remains responsible for public presentation, release metadata, and
mandatory hosted CI. This sprint keeps the local Rust quality gate mandatory
and does not add features, distribution formats, or release automation.

## Correctness contract

- PDF inspection rejects zero-page documents before any page index is clamped
  or scheduled.
- Native watcher and Marcel-owned operation reconciliation share one metadata
  revalidation policy. An ambiguous failure requests one bounded rescan for the
  coalesced batch rather than silently leaving stale state.
- `ShowItems` preserves every validated item in each requested directory and
  reveals the complete group through the normal selection model.
- A settled nonempty selection has a deterministic primary item. Temporary
  primary-less marquee state must be reconciled when the gesture completes.
- Closed windows are removed from desktop routing and release their retained
  Marcel entity, workers, and caches. Activation prunes unusable handles and
  selects the most recent live window.

## Filesystem and hostile-input contract

- Preserve the authoritative `OsString` filename independently of its UI
  label. Invalid UTF-8 names receive an unambiguous escaped display form and
  can be renamed to a valid user-entered name without losing the raw source
  identity.
- Full image previews enforce input-byte, dimension, decoded-pixel, animation
  frame, and decoded-memory limits before publication. Decode work stays off
  GPUI's foreground executor and stale work cannot publish.
- Directory enumeration reports bounded degraded-entry diagnostics instead of
  silently dropping every per-entry or metadata failure.
- Directory sorting and filtering do not allocate folded names inside every
  comparison or rebuild a folded candidate for every query update.
- Shared no-replace rename and occupancy primitives live in one Marcel-owned
  local-filesystem module; operation-specific identity and rollback models
  remain separate.

## Coordinator extraction

- Move preview, folder-preview, PDF, wrapping, and related cancellation/cache
  ownership behind a `PreviewController`. GPUI-bound scheduling and rendering
  may remain on `Marcel` until moving them creates a testable seam.
- Move internal/native file-drag state, hit regions, and edge-scrolling
  ownership behind a `DragController` while keeping GPUI event orchestration
  on `Marcel`.
- Move Places, bookmarks, Trash-place metadata, sidebar menus, and drop-region
  ownership behind a `SidebarController`.
- Group remaining pane/layout/filter/location/rename state where a small
  `WindowUiState` improves ownership without obscuring GPUI interaction.
- Preserve Marcel's existing commands, gpui-component surfaces, and behavior;
  do not add a trait graph, global event bus, or speculative abstraction.

## Acceptance checks

- [x] Reject `Pages: 0` and cover inspection/rendering with a regression test.
- [x] Rescan once for ambiguous native-watcher revalidation failures and share
  the policy with operation reconciliation.
- [x] Reveal every `ShowItems` target in a same-directory batch.
- [x] Preserve a deterministic primary after toggle, retain, and completed
  marquee interactions.
- [x] Remove closed windows from routing and prune unusable handles before
  activation or location requests.
- [x] Round-trip arbitrary Unix `OsString` names in the filesystem model and
  render invalid UTF-8 names without display collisions.
- [x] Allow an invalid-UTF-8 source name to be renamed safely to a valid name.
- [x] Bound still and animated full-image previews and cover oversized,
  over-dimensioned, and over-frame inputs.
- [x] Hand bounded decoded image frames directly to GPUI without a disk
  encode/read/decode loop.
- [x] Surface bounded directory-entry degradation instead of silently omitting
  all failed entries.
- [x] Remove per-comparison lowercase allocation and reuse folded filter data.
- [x] Consolidate no-replace rename and occupancy primitives with operation
  regression coverage unchanged.
- [x] Extract preview, drag/drop, sidebar, and cohesive window UI ownership
  mechanically from `app.rs`.
- [x] Replace the private GPUI Wayland drag fork with the pinned upstream
  external-file-drag lifecycle and remove the vendored framework tree.
- [x] Run automated large-directory, invalid-filename, watcher-error,
  malformed-PDF, and hostile-image checks.
- [ ] Manually exercise multi-window close/reopen, activation, and grouped
  `ShowItems` routing in a graphical session.
- [x] Reconfirm bilateral Chrome/Marcel dragging on Wayland after migrating to
  GPUI's upstream external-drag lifecycle.
- [ ] Run the consolidated watcher, rapid-navigation, large-directory,
  selection, thumbnail, corrupt-preview, and high-DPI checks retained by
  Sprints 1, 2, 4, and 5.
- [ ] Run the consolidated Trash/restore and permanent-delete matrix, including
  mounted volumes, read-only failures, occupied children, confirmation
  cancellation, partial results, and interoperability with external Trash
  entries.
- [ ] Test process interruption during permanent deletion and implement startup
  discovery/recovery guidance for surviving `.marcel-delete-*` quarantines.
- [ ] Add private-session-bus integration coverage for name ownership,
  forwarding, typed errors, cold/warm activation, and primary-process exit;
  manually verify generic-service opt-in and ordinary-package non-ownership.
- [ ] Reproduce the known PDF resize problem with the maintainer and turn the
  confirmed behavior into a focused fix and regression check.
- [ ] Verify explicit thumbnail loading/failure/unsupported presentation and
  close any remaining state or accessibility gap.
- [ ] Run the remaining rename, location-bar, theme, unusual-filename,
  non-Latin fallback, and scale-factor interaction checks retained by earlier
  sprints.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment.

The checklist above is the canonical remaining hardening queue. Open manual
boxes in earlier sprint documents retain their detailed procedures and history;
they do not make those implementation sprints active again.

## Deferred coordination

- X11 outbound source support stays deferred because upstream GPUI does not
  currently provide it.
- Keep mandatory hosted CI, release metadata, artwork, and public packaging in
  deferred Sprint 16.
