# Sprint 20: Cleanup interlude

**Status:** Implemented — every code item below is delivered except the drop
device-identity affordance, which is named and left open with its reason. The
manual acceptance matrices inherited from Sprints 18 and 19 need a graphical
session and remain unrun; they are what stands between this and acceptance.

The suite is 250 library tests plus 1 binary test, up from 234.

## Goal

Close everything standing between the current tree and a *release-readiness
conversation*, and close it in one deliberate pass rather than as a tail of
half-finished sprints.

This is not a feature sprint and it is not an architecture sprint. It is an
interlude: [Review D](../review-2026-08-18.md) found two P0 defects in code
Sprint 18 shipped, three sprints have unchecked acceptance boxes, and
[`review-2026-08-10.md`](../review-2026-08-10.md)'s plan has four unstarted
stages. Those three lists overlap enough that working them separately would mean
touching `file_ops.rs` three times.

The sprint's own precondition is Review D's closing point: the quality gate was
green for every defect it found. Anything here that can only be checked by a
person is listed as such and is not silently converted into a passing test.

## What was wrong

Two defects, one root cause, in code this repository already had the vocabulary
to prevent.

- **D1/D5 — `merge_directories` returns the wrong shape.** It plans first, which
  is right, and then uses bare `?` after mutation has begun
  (`src/file_ops.rs:2534`, `:2545`, `:2555`). A merge that stops half way has
  created directories and published files that its return value does not
  mention, so `transfer_paths_impl` records a failure with no completed effect
  and no undo record — the exact inversion `CommittedOperation` exists to make
  unrepresentable. The same `Err` channel carries cancellation
  (`bail!("Operation cancelled")`), so cancelling a merge is reported to the
  user as a failure and the loop keeps going.
- **D2 — one name, two meanings.** `.marcel-replaced-*` is used both for an
  original whose replacement *succeeded* (reclaimable: the user chose to
  overwrite it and only Undo could ever want it) and for an original Marcel
  *failed to put back* (irreplaceable: it is the user's only copy). The
  abandoned-quarantine sweep is correct for the first and destructive for the
  second, and it runs on every directory listing (`src/app.rs:796-801`), not
  only at startup.

Both were reachable through a gate of 234 green tests. The failure-injection
seam that would have caught D2 has been an unchecked box since Sprint 18 and an
explicit request since `review-2026-08-10.md` Stage 4.

## Decisions taken

- **After the first irreversible write, `Result` is the wrong type.** Not a
  merge rule — the rule. Any function that can leave the disk changed while
  returning must return what it changed. Merge gets `MergeOutcome`; the
  invariant it restores is the one already written down for
  `CommittedOperation` and `MutationOutcome`.
- **Cancellation is not failure, at every layer that can produce it.**
  `TransferOutcome` has accounted for them separately since Sprint 17; the merge
  path simply never reached that accounting. A cancelled merge now stops the
  operation and reports cancellation, exactly as a cancelled copy does.
- **Undo storage and recovery storage are different things with different
  names.** `.marcel-replaced-*` stays reclaimable and stays hidden.
  `.marcel-recovered-*` is created only when Marcel fails to put an original
  back, carries no process id so no sweep can ever claim it, is deliberately
  *not* hidden, and is surfaced in the browser. This reuses the convention
  `.marcel-delete-*` remnants already established (`src/app.rs:6731`) rather
  than inventing a second recovery mechanism.
- **Deleting quarantined data validates identity, like everything else.**
  `ReplacedItem` already carries the `FileIdentity`; the deletion path takes the
  item, not a bare path. The one caller with no identity available — the
  abandoned sweep — re-validates immediately before removing and refuses on a
  mismatch, which narrows the window without pretending to close it.
- **The failure-injection seam lives at the rename chokepoint.**
  `local_fs::rename_no_replace` is the single call every commit boundary in the
  tree goes through, so one test-only hook there reaches publication,
  quarantine, restoration, and move without scattering `#[cfg(test)]` through
  the operation layer.
- **Marcel's own bookkeeping never costs the user an operation.** Quarantine
  names are budgeted against `NAME_MAX`, truncating the copied-in original name
  rather than failing a replacement that the filesystem would have allowed.

## Correctness contract

- A mutation that stops part way returns what it committed. No caller can
  conclude "no effect" from a failure that changed the disk.
- Every source of every transfer is accounted exactly once, as completed,
  failed, skipped, already-in-place, or cancelled — including sources handled by
  the merge path.
- Cancelling any operation is reported as cancellation.
- Data Marcel failed to restore is never deleted by Marcel, is discoverable
  without reading a dismissed notification, and survives the process that
  created it.
- No cleanup path deletes an object it has not identified.
- One operation's undo record is bounded by one operation's snapshot budget,
  through every path that can contribute to it.

## Delivered scope

### Blocking — Review D

- [x] **D1.** `MergeOutcome { created, undoable, stopped }` replaces
  `Result<Vec<PathSnapshot>>`. Additions reach `merged_created` whether the
  merge finished, failed, or was cancelled.
- [x] **D5.** A cancelled merge accounts its source as cancelled, stops the
  operation, and is reported as cancellation.
- [x] **D2.** A failed restoration promotes its quarantine to
  `.marcel-recovered-*`: never swept, not hidden, surfaced in the browser with
  guidance naming both the original path and where the data now is. Applied at
  all three sites — forward transfer, undo of copy, undo of move — and to every
  item still in quarantine when restoration stopped, not only the one that
  failed.
- [x] **D3.** `erase_replacement_quarantine` takes the `ReplacedItem` and
  validates its identity before removing. The abandoned sweep, which has no
  record to compare against, carries the identity it read while scanning and
  refuses on a mismatch.
- [x] **D4.** Merge shares one snapshot budget with the operation that contains
  it; exceeding it downgrades to success-without-undo rather than growing the
  record.
- [x] **`NAME_MAX`.** Replacement and permanent-delete quarantine names are
  bounded by one shared `quarantined_name`, truncating the appended original
  name on a character boundary. It carries raw non-UTF-8 names through byte for
  byte, which the previous lossy conversion did not.

### Blocking — the seam that proves it

- [x] A test-only fault-injection hook in `local_fs::rename_no_replace`, keyed
  on the destination file name and removed by a guard, with deterministic
  coverage of: a transfer failing after its destination was quarantined, that
  transfer's restoration also failing, and a merge stopped part way by failure.

### Inherited code queue — Sprint 17 and `review-2026-08-10.md`

- [x] Give Move the same bounded snapshot budget as Copy (Stage 6, in part).
  `COPY_UNDO_SNAPSHOT_LIMIT` is now `UNDO_SNAPSHOT_LIMIT`, one allowance shared
  by copied sources and output, merge additions, and moved trees.
- [x] Carry a pre-commit object key into every post-commit identity refresh, and
  into `delete_trash_backings` from `purge_trash_records` (Stage 4).
- [x] Make undo of copy and archive output quarantine-first, reusing
  `delete_ops` (Stage 3, remainder).
- [x] Compare physical location, not lexical prefix, when refusing to
  permanently delete a Trash root (Stage 7).
- [x] Surface malformed or unreadable Trash entries instead of dropping them
  from the listing, and tell Empty Trash what it cannot see (Stage 7).

### Found while working, fixed here

- [x] **Permanently deleting a tree holding two hard links to one file failed.**
  The delete plan records a ctime per entry, and removing the first link moves
  the shared inode's ctime, so the plan's own first removal invalidated the
  entry for the second. Directories already refresh after each child; files had
  no equivalent. Copy preserves hard links, so this tree is one paste away. It
  surfaced because quarantine-first undo now runs copy output through exactly
  this path.

### Inherited backlog — `review-2026-08-05.md` lower tier

- [x] Pass `--` to `pdftoppm`, `pdfinfo`, and `gio open`.
- [x] Create the freedesktop thumbnail and PDF cache directories as `0700`.
- [x] Reap terminal children spawned by Open in Terminal.
- [x] Diagnose filesystems without `RENAME_NOREPLACE` explicitly instead of
  failing every rename, move, publication, restore, and quarantine with a
  generic error.
- [ ] **Not done: compare device identity when deciding drop acceptance.**
  `can_move_files_to` is a pure path predicate consulted while the pointer
  moves, so adding a `stat` per source puts filesystem work back on the render
  path this repository has already spent a commit removing. Doing it properly
  means caching the destination's device for the life of the drag session, and
  the only way to confirm the hover styling afterwards is a graphical run. The
  user-facing message is already accurate — `move_error` names the
  cross-filesystem limitation explicitly on `EXDEV` — so what remains is the
  affordance, not the outcome. It stays in [`TODO.md`](../TODO.md).

### Documentation

- [x] Review D retained as supplied; cross-check recorded with per-finding
  verdicts and the finding it missed.
- [x] `review-2026-08-10.md` stage markers corrected — Stages 2 and 5 had landed
  and were still marked open, which made a trusted document wrong.
- [x] `TODO.md`, `docs/README.md`, and `HANDOFF.md` reflect the actual queue.

## Acceptance checks

### Automated

- [x] A merge stopped by an error records every addition it made, reports the
  source as failed, and Undo removes exactly those additions and nothing the
  destination already held.
- [x] A merge stopped by cancellation reports the source as cancelled rather
  than failed, and does not attempt later sources.
- [x] A merge whose additions exceed the operation's snapshot budget still
  completes, and reports itself as not undoable.
- [x] A move whose tree exceeds the operation's snapshot budget still completes,
  and reports itself as not undoable.
- [x] A transfer that fails after quarantining its destination, whose
  restoration also fails, leaves the original in `.marcel-recovered-*`, names
  where it went in its failure, and leaves nothing in `.marcel-replaced-*`.
- [x] The abandoned sweep does not remove a `.marcel-recovered-*` remnant, at
  any process id.
- [x] The browser surfaces a `.marcel-recovered-*` remnant as recovery guidance,
  alongside an interrupted deletion's, and does not hide it.
- [x] Quarantine deletion refuses an object whose identity does not match the
  record, and removes one that does.
- [x] An identity refresh after a commit refuses an object published over the
  path in the meantime.
- [x] A Trash backing replaced between purge validation and deletion is refused.
- [x] A tree holding two hard links to one file is permanently deleted
  completely.
- [x] A Trash root reachable through a symbolic link is still refused, while
  deleting a link that merely points into the Trash is still allowed.
- [x] A replacement of a file whose name is near `NAME_MAX` succeeds and stays
  reversible.

One acceptance claim was written stronger than its test: the cancellation case
asserts the accounting, not "records every addition it made". Deterministic
mid-merge cancellation would need a second injection point that flips the flag
between two commits, and the additions-survive half is already pinned by the
failure case, which is the same code path. Recorded rather than quietly
softened.

### Manual — inherited from Sprint 19, unrun

Every one needs two real windows in a graphical session.

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
  window still restores the replaced original.

### Manual — inherited from Sprint 18, unrun

- [ ] Apply-to-all applies only within its operation, and replace-all does not
  imply merge-all or skip-all.
- [ ] Cancelling from a conflict stops the operation and is reported as
  cancellation. (D5 makes this true in code; it still needs the window run.)
- [ ] Closing the initiating window while a conflict decision is pending leaves
  no parked worker and no partially applied operation.
- [ ] No operation overwrites without a recorded decision, verified across
  paste, drop, archive publication, and Trash restore.

### Gate

- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment, with
  `desktop_integration::tests::private_session_bus_integration` confirmed
  outside the sandbox. 250 library tests plus 1 binary test.

Note for the next run: the special-file tests bind Unix sockets, and `sun_path`
is 108 bytes. A long `TMPDIR` makes six of them fail with errors that look like
socket-permission problems and are not. `CLAUDE.md` now says so.

## Out of scope

- **Merging a folder while moving it.** Still the deliberate Sprint 18 gap. A
  move-merge is a recursive move with per-leaf conflict decisions, which is a
  design question, not a cleanup.
- **A journal-wide snapshot budget.** Stage 6 asks for one budget across the
  whole journal; this sprint bounds each operation, including the merge path
  that escaped its bound. The journal-wide version stays open.
- **Splitting the conflict dialog out of `OperationCoordinator`.** Review D
  advises against it for now and so does Sprint 19. Unchanged.
- **Hosted CI.** Stage 8, Sprint 16 scope, and a release gate rather than a
  hardening item.
- **Archive sandboxing.** Already recorded as a packaging project, with the
  unsandboxed risk explicitly accepted for a personal release.
- **One coalescing writer for browser view state.** Last-writer-wins remains
  acceptable there; bookmarks were the case that mattered and Sprint 19 fixed
  them.
