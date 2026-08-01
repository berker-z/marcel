# Sprint 17: stability and architecture hardening

**Status:** In progress — feature and release work are paused while confirmed
correctness, hostile-input, lifecycle, and coordinator-ownership problems are
closed.

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

- Move preview loading, folder preview, PDF scheduling, wrapping, and related
  cancellation/cache state behind a `PreviewController`.
- Move internal/native file-drag lifecycle, hit regions, and edge scrolling
  behind a `DragController`.
- Move Places, bookmarks, Trash-place metadata, sidebar menus, and drop regions
  behind a `SidebarController`.
- Group remaining pane/layout/filter/location/rename state where a small
  `WindowUiState` improves ownership without obscuring GPUI interaction.
- Preserve Marcel's existing commands, gpui-component surfaces, and behavior;
  do not add a trait graph, global event bus, or speculative abstraction.

## Acceptance checks

- [ ] Reject `Pages: 0` and cover inspection/rendering with a regression test.
- [ ] Rescan once for ambiguous native-watcher revalidation failures and share
  the policy with operation reconciliation.
- [ ] Reveal every `ShowItems` target in a same-directory batch.
- [ ] Preserve a deterministic primary after toggle, retain, and completed
  marquee interactions.
- [ ] Remove closed windows from routing and prove stale handles/entities are
  not selected for activation or location requests.
- [ ] Round-trip arbitrary Unix `OsString` names in the filesystem model and
  render invalid UTF-8 names without display collisions.
- [ ] Allow an invalid-UTF-8 source name to be renamed safely to a valid name.
- [ ] Bound still and animated full-image previews and cover oversized,
  over-dimensioned, and over-frame inputs.
- [ ] Surface bounded directory-entry degradation instead of silently omitting
  all failed entries.
- [ ] Remove per-comparison lowercase allocation and reuse folded filter data.
- [ ] Consolidate no-replace rename and occupancy primitives with operation
  regression coverage unchanged.
- [ ] Extract preview, drag/drop, sidebar, and cohesive window UI ownership
  mechanically from `app.rs`.
- [ ] Run focused large-directory, invalid-filename, watcher-error,
  multi-window, malformed-PDF, and hostile-image checks.
- [ ] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment.

