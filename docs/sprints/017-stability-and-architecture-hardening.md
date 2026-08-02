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
- [x] Run the consolidated Trash/restore and permanent-delete matrix, including
  mounted volumes, read-only failures, occupied children, confirmation
  cancellation, partial results, and interoperability with external Trash
  entries.
- [x] Test process interruption during permanent deletion and implement startup
  discovery/recovery guidance for surviving `.marcel-delete-*` quarantines.
- [x] Add private-session-bus integration coverage for branded name ownership,
  ordinary-process generic-name non-ownership, forwarding, typed errors, warm
  activation, and primary-process exit recovery.
- [ ] Manually verify cold service activation, the generic-service opt-in, and
  ordinary-package non-activation of the generic name.
- [x] Defer the non-blocking PDF resize rendering quirk until the remaining
  hardening and feature queues are exhausted; it is not a Sprint 17 blocker.
- [x] Verify explicit thumbnail loading/failure/unsupported presentation and
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

## 2026-08-01 graphical acceptance run

This run used Hyprland 0.56.1 on Wayland at scale 1.0, a private session bus,
isolated XDG data/config/cache roots, and disposable fixtures under `/tmp`.
No user files or the user's real Trash were used.

Verified:

- A 50,000-entry directory remained responsive in list and grid views, near
  both ends of the directory. Switching views now keeps the primary selection
  visible instead of resetting the viewport to the beginning.
- External create, rename, filtering, hidden-file toggling, active-directory
  replacement, and rapid navigation reconciled without stale publication.
  Valid, corrupt, and unsupported preview inputs rendered, failed explicitly,
  or reported unsupported format without crashing.
- Multi-window open, close, reopen, warm single-instance forwarding, typed
  D-Bus errors, and grouped same-directory `ShowItems` were exercised on the
  private bus. Grouped reveal now replaces a stale unrelated selection and
  retains every requested item. A tokenless `Activate` request correctly did
  not steal focus; a token-bearing launcher activation remains to be tested.
- Home Trash multi-selection, Undo, Redo, explicit Restore, occupied restore
  refusal, external `gio trash` interoperability, mounted-volume `.Trash-1000`
  placement, and mounted restore all preserved their declared identities and
  destinations.
- Permanent-delete and Empty Trash confirmations were cancelled without
  writes and then confirmed successfully. The run exposed missing dialog
  footers after the gpui-component API migration; every affected dialog now
  has explicit visible action buttons.
- Custom palettes now update gpui-component's resolved theme tokens as well as
  its legacy color table, so dialogs, switches, and other token-based controls
  inherit Marcel's active palette instead of retaining default black surfaces.
- Grid thumbnails now distinguish pending work with a muted ellipsis badge,
  decode failure with a danger badge, and unsupported files with their normal
  MIME icon. State selection has direct regression coverage; the graphical run
  confirmed ready, failed, and unsupported rendering against isolated cache
  and corrupt-image fixtures.
- A nested `dbus-run-session` regression test now proves branded name
  ownership, generic-name non-ownership, warm activation/forwarding,
  `ShowItems`, typed `InvalidArgs`, and name reacquisition after the primary
  runtime exits without touching the user's session bus.
- A permission-induced partial permanent deletion retained its child in the
  reported quarantine. Killing Marcel immediately after quarantine publication
  left all 20,000 test files intact. Opening that parent in a new process now
  presents recovery guidance, and the fixture was recoverable by moving the
  quarantined directory to a free destination.

Still open from this matrix: high-DPI repetition, token-bearing and cold
activation, generic-service packaging acceptance, and the remaining
unusual-filename/theme/location checks.

## Deferred coordination

- X11 outbound source support stays deferred because upstream GPUI does not
  currently provide it.
- The working but visually imperfect PDF resize behavior is parked indefinitely
  and should be revisited only after higher-value work is exhausted.
- Keep mandatory hosted CI, release metadata, artwork, and public packaging in
  deferred Sprint 16.
