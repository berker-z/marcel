# Sprint 18: Destination conflict decisions

**Status:** Implemented — the sprint's scope is present and accepted in a
graphical session, apart from the named acceptance checks below that remain
unrun and are carried into [Sprint 20](020-cleanup-interlude.md). Merging a
folder while *moving* it is the one deliberate gap.

[Review D](../review-2026-08-18.md) later found two defects in this sprint's
merge and replacement code, both closed in Sprint 20: a merge that stopped part
way reported no committed effect while half of it sat in the destination, and a
failed rollback left the user's only copy in storage a later Marcel would sweep.
The second lived precisely in the gap this sprint's last acceptance box named
and did not cover.

## Goal

Replace Marcel's no-overwrite-only rule with explicit destination-conflict
decisions, without weakening the safety model that rule was standing in for.

Today every mutation refuses an occupied destination. That is a good default and
it remains the default, but it is not a complete file manager: a user who wants
to replace a file has no way to say so, and the refusal arrives as a failure
rather than as a question. This sprint adds the question.

Two documented contracts changed here, so this was a product decision and not
only an implementation one. [`interaction-model.md`](../interaction-model.md)
stated that interactive conflict decisions were parked and no-overwrite failure
was the safe behavior; that is now the fallback where nothing can answer, not
the whole policy. [`TODO.md`](../TODO.md) parked conflict decisions until their
safety and UX work was scheduled, and now parks only cross-filesystem transfers.
Both were updated when this slice landed.

Sequencing note: this sprint precedes the application-global operation owner
tracked in [`Sprint 17`](017-stability-and-architecture-hardening.md).
Interactive operations must be able to reach a user interface, so deciding how
that works first prevents building an ownership model that cannot support it.

## Upstream study

### Yazi has no conflict interaction to adopt

Yazi's model is a single `force` boolean chosen before the operation starts
(`yazi-scheduler/src/file/file.rs`). With `force` unset it renames the
destination to a unique name through `unique_file` (`yazi-vfs/src/fns.rs`); with
`force` set it overwrites. There is no dialog, no per-item decision, no skip, and
no merge.

That is coherent for a terminal file manager where `paste --force` is a
keybinding. It is not the model Marcel needs, and Marcel should not adopt
automatic unique-renaming as a silent default: renaming without asking is a
quieter surprise than refusing, but it is still a surprise.

### Nautilus supplies the model

Read at commit `f67b2e1`.

- **Response set** is smaller than a feature list suggests: cancel, skip,
  replace, and rename-to-a-new-name, each carrying an apply-to-all flag
  (`nautilus-file-operations.c`, the conflict branch of `copy_move_file`).
- **Merge is not a separate choice.** When source and destination are both
  directories the same replace response means merge; the dialog only relabels
  it.
- **Apply-to-all is three independent per-job flags** — `skip_all_conflict`,
  `merge_all`, `replace_all` — because "merge everything" and "replace
  everything" are different intentions and must not collapse into one.
- **Conflict handling is error-driven, not check-then-act.** The operation is
  attempted, the backend returns an exists error, the user is asked, and the
  loop retries with new parameters. There is no window between testing for
  occupancy and writing, so the design has no time-of-check/time-of-use race.

### The interaction seam

The mechanism matters more than the button set. Nautilus states it directly:

> Dialogs are ran from operation threads, which need to be blocked until the
> user gives a valid response

It packages the question, hops to the main thread with `g_main_context_invoke`,
and blocks the worker on a mutex and condition variable until an answer arrives.

Marcel's equivalent is available and legitimate. Operations already run on
`smol::unblock` blocking-pool threads, and this project's rule is that
filesystem work stays off GPUI's *foreground* executor — not that it never
blocks. A worker can send a conflict and a response channel to the foreground
and wait for the reply.

### Where Marcel must diverge

Nautilus's undo cannot restore what a replace destroyed. Undo of a copy deletes
the destinations (`nautilus-file-undo-operations.c`, `ext_copy_duplicate_undo_func`),
and nothing anywhere retains the file that was replaced. Accepting a replace in
Nautilus therefore ends the reversibility of that data.

Marcel already refuses that trade elsewhere: permanent deletion quarantines
before erasing, and undo validates identity before acting. Conflict decisions
must not become the one place where data leaves without a way back.

## Product contract

- No silent overwrite, in any operation, ever. A replacement happens only as the
  result of an explicit decision for that operation.
- Refusal remains the default and the fallback. When a decision cannot be
  obtained, the item is refused, not replaced.
- Apply-to-all is scoped to one operation and is never persisted, never a
  setting, and never inferred from a previous operation.
- Replace and merge are distinct decisions with distinct sticky state. Merge
  applies only to a directory landing on a directory.
- Directory merge is a separately specified and separately tested operation, not
  an emergent side effect of copying into an existing tree.
- Rename-to-resolve validates the new name with the same rules as Rename and New
  Folder, and re-enters conflict checking if the chosen name also collides.
- The user can always cancel the whole operation from a conflict, and cancelling
  is distinct from skipping.

## Correctness contract

- Conflict handling is error-driven: attempt, observe the exists error from an
  atomic primitive, ask, retry. No mutation may reintroduce a check-then-act
  occupancy test as the basis for writing.
- Every requested source ends in exactly one terminal state — completed,
  skipped, replaced, failed, or cancelled — and those states sum to the number
  requested. This supersedes the separate cancellation-accounting item in
  Sprint 17.
- Undo and Redo never present a conflict decision. They validate recorded
  identities and refuse, as they do now. An undo that asks the user to replace
  something is a worse outcome than an undo that declines.
- A conflict raised when no user interface can answer it — the initiating window
  is gone, or the operation was started from a non-interactive surface —
  resolves to refusal without blocking. A worker must never park indefinitely on
  a reply that cannot arrive.
- Replacing an item must not silently end its recoverability. An operation that
  replaced anything either retains the means to restore what it replaced, or is
  reported as not undoable. Reporting success with a silently unreversible
  replacement is not acceptable.
- The preferred mechanism is the existing quarantine model: move the replaced
  item aside atomically, publish the replacement, and let undo restore the
  quarantined original. Where that is not possible, the operation degrades to
  success-without-undo and says so.
- Conflict decisions are recorded in the operation outcome, so a notification
  can state what was replaced, skipped, and renamed rather than reporting a
  count alone.

## Delivered scope

- Paste and drag-and-drop into an occupied destination.
- The four responses, apply-to-all for each, and independent sticky flags for
  skipping, replacing, merging, and automatic renaming.
- Quarantine-backed replacement, with undo restoring the replaced original and
  the quarantine released once nothing can reach it.
- Directory merge as the union of two trees, undoable by removing exactly what
  it added.

## Decisions taken

- **Drag-and-drop asks, exactly as paste does.** Both gestures funnel through
  one transfer entry point, so the dialog is shared rather than duplicated.
- **A transfer onto the folder an item already occupies is answered, not
  asked.** Moving something to where it already is changes nothing, so it does
  nothing and is reported as neither work nor refusal. Copying something into
  its own folder means duplicating it, so it takes the next free name without a
  prompt. The rule is keyed on the transfer mode rather than the gesture,
  because cut-and-paste inside one folder means what dragging inside it means.
- **Merge is the union of two trees, not a recursive replacement.** The
  destination keeps everything it has and gains what it lacks. This avoids the
  prompt storm of asking per colliding descendant, and it is what keeps merge
  inside the existing guardrails: nothing is displaced, so nothing needs holding
  aside, and undo is exactly "remove what was added".
- **A quarantined replacement is restored if its transfer then fails**, rather
  than being left for the startup recovery path.
- **Replace is offered when a file lands on a directory.** It is destructive,
  but it is quarantine-backed like any other replacement, so undo restores the
  directory whole.

## Still open

- **Merging a folder while moving it.** Expressing it as renames would leave the
  source partly emptied; expressing it as copy-then-delete would abandon the
  rename-only model that keeps moves atomic and same-device. Merge is therefore
  reachable through paste but not through a drag, which is an inconsistency
  worth closing.
- **Conflict decisions for surfaces other than transfers.** Archive publication
  and Trash restore still refuse an occupied destination outright.

## Acceptance checks

- [x] Attempting to paste onto an occupied destination presents a conflict
  decision rather than failing, and refusing leaves both items untouched.
- [x] Replace, skip, rename, and cancel each produce the exact disk and
  projection state they name, verified per response. Replace, rename, and the
  merge refusal were confirmed in a graphical session; skip and cancel have
  automated coverage only.
- [x] An alternative name increments rather than nesting, starts at `(2)`,
  preserves compound extensions and dotfile names, and puts the suffix at the
  end of a directory name.
- [ ] Apply-to-all applies only within its operation, and replace-all does not
  imply merge-all or skip-all. Covered by unit tests; unverified in a window.
- [x] A rename response that collides again re-enters the conflict decision
  instead of failing or overwriting.
- [x] Cancelling from a conflict stops the operation and is reported as
  cancellation, not as failure — in code. It was *not* true for a merge, which
  reported cancellation as a failure and kept going;
  [Sprint 20](020-cleanup-interlude.md) fixed that and covered it. The window
  run is carried there and remains unrun.
- [x] Completed, skipped, replaced, failed, and cancelled outcomes account for
  every requested source exactly once.
- [x] Undo restores an item that a replace overwrote, or the operation reported
  itself as not undoable at the time it completed.
- [x] Undo and Redo never present a conflict decision, and still refuse on a
  changed or replaced identity. Undoing a replacement is deliberately not
  redoable, because redoing would displace the restored item again.
- [x] A transfer whose destination resolves to its own source is refused rather
  than offered as a decision, including through a hard link.
- [x] A conflict raised with no reachable user interface refuses and completes
  the operation instead of blocking a worker thread.
- [ ] Closing the initiating window while a conflict decision is pending leaves
  no parked worker and no partially applied operation. The worker path is
  covered by tests; the window path is unverified.
  [Sprint 19](019-application-global-operations.md) restated the rule: a
  question already on screen dies with its window and cancels, while the *next*
  question moves to another live window, or refuses when there is none.
- [x] Conflict handling introduces no occupancy test that a concurrent writer can
  invalidate between the test and the write.
- [x] Merging folds one directory into another, keeping everything the
  destination already held at every depth and adding only what it lacked, with
  an existing subdirectory joined rather than replaced.
- [x] Undo of a merge removes exactly what the merge added, leaves what was
  already there, and refuses to remove a directory that has since gained an
  entry rather than deleting it.
- [x] A second merge of the same source adds nothing and records nothing to
  undo.
- [x] A transfer onto the folder its source already occupies does nothing for a
  move and duplicates for a copy, without prompting for either.
- [ ] No operation overwrites without a recorded decision, verified across
  paste, drop, archive publication, and Trash restore. Transfers ask; archive
  publication and Trash restore still refuse an occupied destination outright.
- [x] A replaced item held for undo is released when its record is displaced,
  when the application exits, and when a later run finds it abandoned by a
  process that is gone. Releasing on *window* close was correct only while the
  journal died with the window;
  [Sprint 19](019-application-global-operations.md) made the record outlive the
  window, so erasing the quarantine there would have destroyed data a live
  record still pointed at.
- [x] Marcel's own working files stay out of the browser, so a replacement does
  not make a hidden sibling appear beside its target.
- [x] Deterministic failure-injection coverage for a failure arriving after a
  replacement has been quarantined but before the replacement is published.
  Delivered in [Sprint 20](020-cleanup-interlude.md), which built the seam and
  found a data-loss path in exactly this gap: a restoration that also failed
  left the original in storage a later Marcel would sweep.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment.

## 2026-08-10 first-slice acceptance run

Run against disposable fixtures under `testo/`, covering a plain collision, a
collision whose `(2)` was already taken, a compound `.tar.gz` extension, a
dotfile, a directory onto a directory, and a name with no collision at all.

Verified: an occupied destination asks instead of failing; renaming produces
`(2)`, then `(3)`, then `(4)` as earlier names fill up, never nesting;
`archive.tar.gz` keeps its compound extension; `.bashrc` keeps its leading dot;
a directory takes the suffix at the end of its whole name; replacing works and
Undo brings back what it displaced; merging two directories is refused with a
clear message; and pasting a file into the folder it came from is refused
outright.

Three defects surfaced that every automated check had passed:

- The apply-to-all control drew whatever value it was handed and was never
  handed one, so it always rendered unchecked while quietly changing the answer
  that would be sent. It is now a checkbox reading live state, which is also
  what it should have been: a modifier on the button about to be pressed.
- The dialog builder read the window entity, which panicked because it runs
  inside the update that opened the dialog. Entity access belongs in a dialog's
  callbacks, never its builder — every other dialog already followed that rule.
- Answering a conflict sent the decision but left the dialog on screen, because
  the close wrapper handles a click on the element it wraps and a button with
  its own handler consumes it. Transfers ran to completion behind a stack of
  undismissed dialogs.

All three were interaction defects invisible to `cargo test`, which is the
argument for keeping a graphical run in this sprint's acceptance rather than
treating the automated gate as sufficient.

A second run covered merging, against a source and destination sharing a
directory name, a colliding file inside it, a colliding subdirectory, and a
subdirectory present only in the source. The destination kept every file it
held at every depth, gained only what it lacked, and Undo removed exactly what
had arrived. Adding a file to a merged folder and then undoing refused to
remove that folder rather than deleting the file, and merging a second time
added nothing and recorded nothing.

One defect class recurred three times across this sprint and is worth stating
plainly: **a rename or a removal moves an inode's ctime, so an identity
recorded before that moment cannot be compared after it.** It appeared when
quarantining a replaced file, when snapshotting a directory a merge was about
to fill, and when undo removed the children of a directory it had recorded.
Each time the symptom was an operation refusing its own work as though an
outsider had interfered.

## Out of scope

- Cross-filesystem transfers, which remain parked with their own separate
  safety work.
- Conflict decisions for operations that do not currently exist, including
  Duplicate and Move To.
- Persisting any conflict preference across operations or sessions.
- The application-global operation owner, which follows this sprint and is
  tracked in [`Sprint 17`](017-stability-and-architecture-hardening.md). This
  sprint only constrains it: operations must be able to reach a user interface,
  and must degrade safely when they cannot.
