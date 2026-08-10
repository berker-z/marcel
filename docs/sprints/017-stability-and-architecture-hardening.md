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

- A mutation that crosses more than one commit point reports which side of that
  boundary a failure landed on. An ordinary "nothing happened" error is legal
  only where Marcel can prove no commit escaped.
- No failure path reinstates an operation record whose recorded identities
  predate a compensating rename. Compensation that restores the paths still
  invalidates the record, because renaming a root moves its ctime.
- Visible transfer effects come from the exact recorded transfers and the
  transfer mode, never from an undo record that may describe only a subset.
- A tree Marcel cannot copy or archive is still movable and still undoable.
  Object kinds it cannot reproduce are refused only where undo would have to
  recreate or delete them.
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
- [x] Manually exercise multi-window close/reopen, activation, and grouped
  `ShowItems` routing in a graphical session.
- [x] Reconfirm bilateral Chrome/Marcel dragging on Wayland after migrating to
  GPUI's upstream external-drag lifecycle.
- [x] Run the consolidated watcher, rapid-navigation, large-directory,
  thumbnail, corrupt-preview, and high-DPI checks retained by Sprints 1, 2, 4,
  and 5.
- [ ] Finish the remaining pointer-selection and marquee checks retained by
  Sprint 2.
- [x] Run the consolidated Trash/restore and permanent-delete matrix, including
  mounted volumes, read-only failures, occupied children, confirmation
  cancellation, partial results, and interoperability with external Trash
  entries.
- [x] Test process interruption during permanent deletion and implement startup
  discovery/recovery guidance for surviving `.marcel-delete-*` quarantines.
- [x] Add private-session-bus integration coverage for branded name ownership,
  ordinary-process generic-name non-ownership, forwarding, typed errors, warm
  activation, and primary-process exit recovery.
- [x] Manually verify cold service activation, the generic-service opt-in, and
  ordinary-package non-activation of the generic name.
- [x] Defer the non-blocking PDF resize rendering quirk until the remaining
  hardening and feature queues are exhausted; it is not a Sprint 17 blocker.
- [x] Verify explicit thumbnail loading/failure/unsupported presentation and
  close any remaining state or accessibility gap.
- [x] Run the remaining rename, location-bar, theme, unusual-filename,
  non-Latin fallback, and scale-factor interaction checks retained by earlier
  sprints.
- [x] Return an explicit three-state outcome from `undo_operation`,
  `redo_operation`, and the Trash mutations, and reinstate a history record
  only for a failure that provably never reached the filesystem.
- [x] Derive redone-transfer effects from `CompletedTransfer` and the transfer
  mode so a copy redo never marks its still-present source removed.
- [x] Keep moved trees holding a socket, FIFO, or device node undoable, while
  archive creation, extraction, and snapshotted-tree removal still refuse them.
- [x] Bound the operation journal to 20 records per stack.
- [ ] Give Move the same bounded snapshot budget as Copy, and make the budget
  journal-wide rather than per-operation.
- [ ] Carry a pre-commit object key into every post-commit identity refresh, and
  into `delete_trash_backings` from `purge_trash_records`.
- [ ] Make undo of copy and archive output quarantine-first, reusing
  `delete_ops`, so a partial erase cannot leave the record claiming a whole
  tree.
- [ ] Account for every requested source exactly once across completed, failed,
  and cancelled outcomes.
- [ ] Compare physical location, not lexical prefix, when refusing to
  permanently delete inside a Trash root.
- [ ] Surface malformed or unreadable Trash entries instead of dropping them
  from the listing, and stop Empty Trash implying it emptied them.
- [ ] Move active filesystem operations, the operation journal, and the busy
  lock to an application-global owner so closing a window cannot orphan work or
  discard history.
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

Still open from this matrix: token-bearing compositor activation and the
remaining detailed pointer-selection/marquee checks.

## 2026-08-02 isolated scale and activation run

This run used a private D-Bus daemon and a headless Sway 1.12 Wayland output at
physical 1600×1000, logical 800×500, and scale 2. Input and screenshots stayed
inside the nested compositor and could not target the maintainer's live
desktop.

Verified:

- The real ordinary flake output cold-activated through branded `Activate` and
  `Open`, installed no generic activation file, and returned `ServiceUnknown`
  for a generic FileManager1 request on a minimal bus.
- The generic opt-in initially exposed a real ownership defect: its service
  launched Marcel without requesting `org.freedesktop.FileManager1`. It is now
  a complete wrapped package variant whose CLI, branded service, and generic
  service all request both names. A cold `ShowFolders` call activated the
  rebuilt output and the same process owned both names.
- The Nix builder now supplies D-Bus as a check-only dependency and gives the
  private-bus integration test an explicit hermetic session configuration.
- Nord, Gruvbox Dark, and System Light rendered consistently at scale 2. Text,
  bundled icons, thumbnails, switches, separators, and the thumbnail failure
  badge remained sharp and correctly colored.
- Invalid input retained the current directory and rendered a visible error;
  paths with spaces navigated correctly; a non-Latin folder and filename
  rendered and navigated correctly; narrow breadcrumbs compacted to
  `root / … / tail`.
- An invalid-UTF-8 source renamed to a valid name, and Undo/Redo restored and
  reapplied its exact raw identity. A valid non-ASCII name renamed to another
  Unicode name and remained selected.

## 2026-08-10 transaction-integrity slice

A third review, cross-checked in [`review-2026-08-10.md`](../review-2026-08-10.md),
found that `CommittedOperation` closed the single-commit boundary and left the
multi-commit one open. Undo, Redo, and the Trash mutations could rename one
item, fail on the next, compensate, and still return an ordinary error, after
which the application reinstated the record that attempt had just invalidated.

Both upstreams were read at pinned commits before deciding anything, and the
decision came from Nautilus:

- Yazi has no filesystem undo at all, so the journal has no upstream precedent.
  Its move path is rename-first with no content inspection, which is what makes
  moved trees holding special files safe to keep undoable, and its `ChaType`
  supplied the object-kind taxonomy now mirrored in `SnapshotKind`.
- Nautilus discards its undo record whenever an undo fails for any reason other
  than user cancellation, and clears the pending action before the asynchronous
  undo runs rather than reinstating it afterwards. Marcel adopts the rule and
  refines it: its prepare/commit/finalize discipline can prove when nothing
  committed, so those failures stay retryable. Nautilus cannot express that
  distinction because it stores no identities that can go stale.
- Nautilus's Trash undo discards the return value of the move that performs
  each restore and reports success when any entry merely matched. Marcel's
  identity-validating, rolling-back restore is stronger than either upstream,
  so this slice kept Marcel's model and added only the commit-boundary split.

Deliberately deferred rather than done: the remaining unchecked boxes above,
which the review tiers as identity-refresh races, accounting completeness, and
the application-global operation owner.

## Deferred coordination

- X11 outbound source support stays deferred because upstream GPUI does not
  currently provide it.
- The working but visually imperfect PDF resize behavior is parked indefinitely
  and should be revisited only after higher-value work is exhausted.
- Keep mandatory hosted CI, release metadata, artwork, and public packaging in
  deferred Sprint 16.
