# Sprint 4 — Controller extraction and directory session

**Status:** Implemented — both ownership extractions and their quality gate are
complete. Large-directory manual acceptance is consolidated under Sprint 17.

## Goal

Establish narrow ownership boundaries before adding filesystem watching and
native desktop drag-and-drop. This sprint is a behavior-preserving extraction,
not a clean-architecture rewrite.

## Slice A — Operation controller

- [x] Move clipboard, journal, busy, cancellation, task, and progress ownership
  from `Marcel` into an `OperationController`.
- [x] Preserve the shared command dispatcher and every current create, copy,
  move, undo, redo, notification, refresh, and selection behavior.
- [x] Keep filesystem implementation in `file_ops.rs`; the controller owns
  application lifecycle and pure state transitions.
- [x] Add tests for busy/cancel transitions, journal transitions, and partial
  cut-clipboard retention.
- [x] Keep operation progress and notifications in their shared bottom-right
  status lane.

## Slice B — Directory session

- [x] Extract current-directory entries, visible projection, loading state,
  generation tickets, filtering, selection reconciliation, and pending reveal
  into a directory/browser session.
- [x] Keep scroll handles, painted bounds, marquee geometry, and rendering in
  the browser view layer.
- [x] Define and test a pure incremental event reducer for add, remove, change,
  rename, and rescan-required events before connecting a watcher.
- [x] Preserve list/grid behavior, fuzzy filtering, navigation, and selection
  in the automated suite. The manual 10,000-entry list/icon responsiveness
  check remains part of the sprint's final acceptance run.

## Non-goals

- No filesystem watcher is connected during the extraction.
- No native desktop drag-and-drop is added.
- No copy semantics or conflict behavior changes.
- No visual redesign.

## Quality gate

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The 10,000-entry manual fixture must remain responsive at both the beginning
and end of the directory in list and icon views.
