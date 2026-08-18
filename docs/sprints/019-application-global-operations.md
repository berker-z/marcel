# Sprint 19: The application owns its operations

**Status:** Implemented — the ownership move, the effect broadcast, the shared
conflict surface, and the shared bookmark store are in place and pass the local
quality gate. The multi-window acceptance checks below are unrun; they need a
graphical session, and they are carried unchanged into
[Sprint 20](020-cleanup-interlude.md), which is where they will be run.

[Review D](../review-2026-08-18.md) read this work and endorsed both the
ownership model and the decision not to accept the sprint until that list is
run.

## Goal

Move active filesystem operations, the operation journal, the file clipboard,
and the busy lock out of the window and into the application, so that closing a
window cannot orphan work or discard history.

This is the first item in [Sprint 17](017-stability-and-architecture-hardening.md)'s
remaining hardening queue and Stage 5 of the plan in
[`review-2026-08-10.md`](../review-2026-08-10.md). It deliberately follows
[Sprint 18](018-destination-conflict-decisions.md), which made conflict
decisions interactive and thereby constrained this design: an operation can now
block a worker thread waiting for a user, so any ownership model that outlives a
window has to say what happens to the question that window was going to ask.

## What was wrong

Three defects, one cause. `OperationController` lived on `Marcel`, so it died
with the window.

- **A1 — closing a window orphans running work.** The controller held the
  cancellation flag and the GPUI tasks, but the work itself ran inside
  `smol::unblock` and kept going on the blocking pool after the entity was
  dropped. Closing the window removed the only progress surface and the only
  reconciler while the mutation ran to completion.
- **M1 — the journal and the busy lock were per-window.** Two windows kept two
  undo stacks and two busy locks. Window B could mutate a path window A's record
  depended on, and A's later Undo would refuse with *"changed or was replaced"* —
  Marcel blaming an outsider for another Marcel window. Two windows could also
  run conflicting operations on the same paths at once. Closing a window
  silently discarded its entire undo history, and with it the only handle on the
  quarantines that history was holding for replaced files.
- **The bookmark P2.** Every window kept its own bookmark list and its own save
  task. The file was published atomically, which makes each write indivisible
  but does not make two writers agree: a window that had not seen the other's
  addition wrote its stale list over the top. [`review-2026-08-05.md`](../review-2026-08-05.md)
  parked this as "benign last-writer-wins"; that judgement was wrong for
  bookmarks specifically, because the loss is user data and leaves no parse
  error behind.

## Upstream study

Both upstreams were cloned and read at their pinned commits before anything was
decided.

### Yazi (`319f90e0`) — the question cannot arise, which is itself the finding

Yazi has no windows, so it has no window-versus-application ownership problem to
solve. `Tasks::serve` builds exactly one `Scheduler` and holds it as
`Arc<Scheduler>` on the running application (`yazi-core/src/tasks/tasks.rs:22`),
and every submission goes through that one worker pool. There is no per-surface
copy of anything.

Also re-confirmed at this commit, because the repository's earlier reviews
rest on it: the only `undo` in the tree is the text-input widget
(`yazi-widgets/src/input/actor/undo.rs`). Nothing in `yazi-scheduler`,
`yazi-fs`, `yazi-core`, or `yazi-actor` has filesystem undo. Recording it again
so the next reader does not go looking a third time.

### Nautilus (`f67b2e1`) — the same conclusion, reached earlier

- **The undo manager is a process singleton.** `nautilus-file-undo-manager.c:58`
  holds `static NautilusFileUndoManager *undo_singleton`, handed out by
  `nautilus_file_undo_manager_get`. Windows do not own records; they connect to
  its `undo-changed` signal and re-derive their menu state
  (`nautilus-window.c:1226`, `nautilus_window_on_undo_changed`).
- **Progress is a process singleton too.** `nautilus-progress-info-manager.c:44`
  holds `static NautilusProgressInfoManager *singleton`, and each window's
  indicator reads from it rather than from its own jobs
  (`nautilus-progress-indicator.c:483`). One window can therefore display work
  another window started.
- **A job holds its window weakly.** `init_common`
  (`nautilus-file-operations.c:533`) attaches the parent window through
  `g_object_add_weak_pointer`, so destroying the window silently nulls the
  job's pointer and the job carries on. The window is a dialog parent, not an
  owner.

This is the shape Marcel now has. Marcel keeps its own stronger model on top —
identity-validated undo, quarantine-backed replacement — and one thing Nautilus
does not have at all.

### Where Marcel diverges, deliberately

- **Marcel excludes; Nautilus does not.** Every Nautilus operation is its own
  `g_task_run_in_thread` (`nautilus-file-operations.c:5247` for copy, and the
  same pattern for move, delete, and empty-trash) with nothing serializing them.
  Concurrency is the feature, and the progress list is how the user follows it.
  Marcel keeps one operation at a time, and this sprint is what finally makes
  that promise true across windows rather than only within one. Marcel's undo
  records store filesystem identities that a concurrent second operation can
  invalidate, so "one at a time" is load-bearing here in a way it is not there.
- **Marcel cannot let a question lose its window.** `run_dialog` passes the
  possibly-null parent straight into `adw_alert_dialog_choose`
  (`nautilus-file-operations.c:798`) and blocks the worker on a mutex and
  condition variable until the response callback fires. A vanished parent is,
  for Nautilus, a dialog with no parent. For Marcel it would be a parked
  blocking-pool thread, because Sprint 18 made the worker wait on a channel that
  only a live surface can answer. Nautilus never had to write down where a
  question goes when its window is gone; Marcel does, and that rule is below.

## Decisions taken

- **A window is a surface, not an owner.** It starts operations, shows their
  questions and their reports, and folds their effects into its own projection.
  The application owns the journal, the clipboard, the busy lock, the
  cancellation flag, the progress, and the tasks.
- **Effects are broadcast, not applied to the initiating window.** Every window
  hears every committed change and reduces the part that concerns it. This is
  not extra generality for its own sake: a second window showing the folder a
  transfer landed in previously waited on a watcher event to notice, and a
  window browsing Trash never noticed another window trashing something at all,
  because the Trash listing has no watcher.
- **Reveal is the one effect that is not shared.** Selecting and scrolling to
  the result belongs to the window that asked for the work. It carries the
  originating window handle and every other window ignores it.
- **A question follows the work, not the window.** The initiating window answers
  for an operation while it is open. Once it is gone the operation is still the
  application's, so the next question opens on whichever window the user is
  looking at. With no window at all there is no surface, the pending question is
  dropped, and dropping it answers `Cancel` — which is what keeps Sprint 18's
  rule true: a worker never parks on a reply that cannot arrive.
- **A question already on screen dies with its window.** Re-homing applies to
  the *next* question. A dialog destroyed with its window drops its
  `PendingConflict`, which cancels the operation — the existing, deliberate
  behaviour, and the right reading of closing a window mid-question.
- **The busy lock now excludes across windows.** Every window's Paste, Undo,
  Compress, and Delete are disabled while any window's operation runs. That is
  stricter than before and it is the point: it was never safe for two windows to
  mutate the same tree at once, only unenforced.
- **Quarantines are released when the application exits, not when a window
  closes.** Releasing them on window close was the old bug wearing a helpful
  face: the records were still meant to be live, and erasing what they held made
  a subsequent Undo of a replacement unrecoverable. `on_app_quit` now does it,
  with `Drop` as a backstop and the existing startup reclamation as the answer
  to a crash.
- **Bookmarks get the same treatment, and browser view state does not.** One
  list, one writer, all windows reading it. Losing a bookmark is silent
  user-data loss; losing which of two windows last set the view mode is not.

## Correctness contract

- No filesystem work is owned by a window. Closing a window neither cancels an
  operation that has begun nor removes the reconciler that will apply its
  result.
- One journal, one clipboard, and one busy lock exist per process. A record
  written from one window is the next Undo in every window, including after the
  window that produced it has closed.
- An interactive operation resolves every conflict against a live user
  interface, or refuses. It never blocks on a surface that does not exist.
- A committed effect reaches every window's projection. No window's view of the
  filesystem depends on which window started the mutation.
- Reported outcomes reach a window while any window is open, and are dropped
  silently only when none is.
- A bookmark added in one window is never lost to a save from another.

## Delivered scope

- `OperationCoordinator`, an application-global GPUI entity owning the journal,
  clipboard, busy lock, cancellation, progress, and every operation task.
  `src/operations.rs` now runs rename, new folder, compress, extract, undo,
  redo, trash, restore, permanent delete, empty Trash, and transfer end to end.
- `OperationEvent`, the broadcast every window subscribes to: committed
  directory changes with an origin-scoped reveal, Trash entries added or
  removed, and Trash-listing invalidation.
- `src/surface.rs`, the shared answer to "which window speaks for this" —
  origin, then the active window, then any live window, then nothing.
- `BookmarkStore`, an application-global list with a single coalescing writer.
- `Marcel` reduced to a surface: it reads the coordinator for enablement and
  progress, calls its `start_*` methods, and applies its events.

## Acceptance checks

- [x] The journal, clipboard, busy lock, progress, cancellation, and operation
  tasks live on one application-global owner, and no window holds any of them.
- [x] One running operation locks out every other surface, verified for
  transfer, archive, permanent delete, undo, and the simple mutations.
- [x] A record outlives the surface that created it.
- [x] An operation reports to and asks the window that started it while that
  window is open, moves to a live window when it is not, and finds no surface
  when no window is left.
- [x] Bookmarks are one list with one writer; the sidebar reads the store rather
  than a per-window copy.
- [ ] Start a copy large enough to observe, close the initiating window, and
  confirm the copy completes, its record enters the journal, and a second window
  both shows the progress and reconciles the result.
- [ ] Close the only window during a copy and confirm the process completes the
  work it had committed to and exits without a parked worker.
- [ ] Undo in window B a mutation started in window A, before and after closing
  window A.
- [ ] With two windows open, confirm Paste, Undo, Compress, and Delete are
  disabled in both while either is running an operation.
- [ ] Raise a conflict from window A, close window A while the dialog is open,
  and confirm the transfer cancels rather than parking.
- [ ] Raise a conflict from window A with window B also open, close window A
  before the *next* conflict, and confirm that conflict opens on window B.
- [ ] Cut in one window and paste in another.
- [ ] Trash an item from window A while window B is browsing Trash, and confirm
  B's listing gains it without a reload.
- [ ] Add a bookmark in each of two windows and confirm both survive, in the
  file as well as on screen.
- [ ] Replace a file, close the window that did it, and confirm Undo in another
  window still restores the replaced original — the quarantine must not have
  been released with the window.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment.

## What automated coverage can and cannot reach here

The repository has no GPUI test harness, and adding one is its own piece of
work. What is unit-testable is the state machine and the routing rule, and both
are covered: the busy lock excluding every entry point, history surviving its
surface, and surface selection across origin-closed, nothing-focused, and
no-window-left. What is not testable without a harness is the part that only
exists once there are two real windows — which is why the list above is mostly
manual, and why Sprint 18's lesson stands: every one of that sprint's three
interface defects passed `cargo test`.

## Out of scope

- A user-facing New Window action. Additional windows still arrive only through
  a D-Bus request carrying more than one location, which is what made A1 low
  priority rather than invalid. Nothing here depends on adding one.
  [Sprint 21](021-a-launch-is-a-window.md) added one anyway, for a reason this
  sprint did not anticipate: with no way for a person to open a second window,
  the acceptance list below was not something anyone could reasonably run.
- Merging the browser-state writer. Last-writer-wins is acceptable for view
  state and the cleanup stays in [`TODO.md`](../TODO.md).
- A shared progress surface listing several concurrent operations. The busy lock
  still allows exactly one at a time, so one progress panel remains correct.
- The rest of Sprint 17's queue: the journal-wide snapshot budget, pre-commit
  identity keys across post-commit refreshes, quarantine-first undo of copy and
  archive output, the physical Trash-root check, and malformed Trash entries.
