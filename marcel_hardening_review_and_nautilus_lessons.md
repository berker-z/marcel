# Marcel Hardening Review and Nautilus Lessons

_Current bugs, fixed findings, transaction-model gaps, and lessons worth taking from GNOME Nautilus_

| **Reviewed branch** | operation-commit-integrity                                                                                                            |
|---------------------|---------------------------------------------------------------------------------------------------------------------------------------|
| **Baseline**        | master @ 482a88a15f69766b7f108f8f96201c41bbdb59ac                                                                                     |
| **Review date**     | 10 August 2026                                                                                                                        |
| **Scope**           | Operation integrity, filesystem safety, lifecycle, performance, persistence, Trash, archives, and Nautilus-derived architecture ideas |

> **Executive verdict**  
> The side branch is a real improvement. It fixes the major deterministic failures from the original review and introduces the right core abstraction: a committed filesystem mutation can succeed even when Undo metadata is unavailable. The remaining release-blocking problem is narrower: compound operations still sometimes have multiple commit points but only two result states. Marcel now has strong single-operation semantics; it does not yet have complete compound-transaction semantics.

**Current severity:** No confirmed live P0 on the reviewed branch. Several P1 correctness issues remain and should be treated as pre-release blockers.

**Evidence note:** This synthesis combines the original source-level review, the repository's own executable validation notes, and a second source-level inspection of the operation-commit-integrity branch. The repository reports 181 tests passing with clean cargo fmt and Clippy on the branch. This review environment could inspect GitHub source but did not independently execute those tests.

# Contents

1. Severity model and overall assessment

2. What the side branch fixed

3. P1: remaining correctness and integrity blockers

4. P2: important hardening before public release

5. P3: lower-tier backlog and polish

6. Transaction model: the next architectural step

7. Good ideas to take from Nautilus

8. Ideas not to copy from Nautilus

9. Recommended implementation order

10. Regression and CI matrix

11. Release gate

12. Sources reviewed

# 1. Severity model and overall assessment

| **Priority** | **Meaning for Marcel**                                                                                                                    |
|--------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| P0           | Immediate data-loss, destructive security, or runaway resource bug reachable through a plausible user action. Stop release and fix first. |
| P1           | Correctness or integrity defect that can make disk state, UI state, Undo/Redo history, or user messaging disagree. Release blocker.       |
| P2           | Important hardening, lifecycle, scale, or platform defect. Should be fixed before a public release unless explicitly accepted.            |
| P3           | Known limitation, diagnostics issue, or polish item that does not invalidate the core safety model.                                       |

The branch should be considered substantially safer than master. The strongest change is conceptual rather than local: CommittedOperation makes the filesystem commit an explicit state transition. This removes the previous assumption that Result::Err always meant "nothing happened".

The remaining problem is that this model is not yet consistently extended to multi-item and multi-step operations. Once an operation can commit item A, then attempt item B, there are three states rather than two: unchanged, fully committed, and partially committed/recovered. The current code still sometimes collapses the third state into an ordinary error.

> **Current release posture**  
> Merge-worthy hardening branch, but do not mark the transaction layer complete yet. Fix P1.1 through P1.6 before treating Undo/Redo and compound file operations as reliable under failure or races. P1.7 through P1.9 should follow immediately.

# 2. What the side branch fixed

These findings from the original review are genuinely resolved or materially reduced on operation-commit-integrity.

| **Original finding**                                           | **Status** | **What changed**                                                                                                     |
|----------------------------------------------------------------|------------|----------------------------------------------------------------------------------------------------------------------|
| Committed move reported as failure                             | Fixed      | Move snapshots are now best-effort bookkeeping. A rename can succeed even when a socket/FIFO makes Undo unavailable. |
| Single rename/create/archive mutation returns Err after commit | Fixed      | CommittedOperation separates filesystem success from optional Undo metadata.                                         |
| Symlinked self-copy amplification                              | Fixed      | Destination ancestry is resolved before copy/move; static alias-to-descendant recursion is rejected.                 |
| Copy staging check-then-delete race                            | Fixed      | Private staging now uses atomically reserved TempDir paths.                                                          |
| Unbounded recursive walkers                                    | Fixed      | Copy, snapshot, measurement, and delete planning use explicit work stacks.                                           |
| Basename cut reconciliation                                    | Fixed      | CompletedTransfer records exact source and destination paths.                                                        |
| Per-frame drag payload work                                    | Fixed      | Drag payload is cached by selection/projection revision and stores Arc-backed path slices.                           |
| Linear path lookup in hot UI paths                             | Fixed      | DirectorySession carries a revisioned path index.                                                                    |
| Predictable state/bookmark temp-file collision                 | Fixed      | NamedTempFile plus fsync and parent-directory sync prevents interleaved temp corruption.                             |
| Trash hardlink identification guess                            | Fixed      | Ambiguous Trash records disable Undo rather than guessing the wrong entry.                                           |

> **Important distinction**  
> The branch fixes the most embarrassing failure mode from the first review: a simple mutation no longer commonly succeeds on disk and then gets reported as a normal failure. That is a large change in trustworthiness.

# 3. P1: remaining correctness and integrity blockers

**P1 Mixed undoable and unundoable moves can still desynchronize the browser** *\[Live, branch-specific\]*

> **Summary:** A transfer may successfully move every selected item while producing an Undo record for only the subset that could be snapshotted.
>
> **Mechanics:** If a transfer includes one ordinary directory and one directory containing a Unix socket, both appear in completed. Only the ordinary directory appears in OperationRecord::Move. start_transfer currently derives visible DirectoryChanges from the operation record whenever one exists, so the successfully moved socket-containing directory can be omitted from browser reconciliation.
>
> **Impact:** The disk is correct but the browser can keep showing a vanished source or fail to show a destination. The UI may also retain a partial Undo record while displaying an all-or-nothing Undo availability message.
>
> **Recommended fix:** Always derive transfer-visible effects from CompletedTransfer records. Treat the operation record as Undo metadata only. Prefer making the whole transfer non-undoable if any completed move is unundoable, unless Marcel deliberately implements partial Undo semantics.
>
> **Regression test:** Regression: move two directories in one operation, one containing a live Unix socket. Verify both sources disappear, both destinations appear, and Undo behavior is explicit and consistent.
>
> **Relevant code:** src/file_ops.rs, src/app.rs

**P1 Compound operations still have unrepresented partial-commit states** *\[Live\]*

> **Summary:** CommittedOperation solves one commit boundary, but Undo/Redo, multi-item restore, retrash, and recursive Undo deletion can cross several commit points before returning.
>
> **Mechanics:** A multi-move Undo can rename item A successfully, fail on item B, then fail while rolling A forward again. Similar shapes exist in Trash restore/retrash, redo compensation, and remove_snapshotted_tree. The function may return ordinary Err after disk state changed.
>
> **Impact:** The application can interpret Err as unchanged, reinsert the original history record, and present failure while disk state is partial or recovered differently.
>
> **Recommended fix:** Introduce an explicit mutation outcome with Unchanged, Committed, and Partial states. Once any sub-operation commits, an ordinary error must no longer be returned without carrying exact resulting DirectoryChanges and a refreshed recovery/history record.
>
> **Regression test:** Regression: inject deterministic failure on the Nth rename/remove in a compound Undo and assert the returned state exactly describes disk state.
>
> **Relevant code:** src/file_ops.rs, src/trash_ops.rs, src/app.rs

**P1 Successful compensation can leave a stale retry record** *\[Live\]*

> **Summary:** A rollback may restore paths while changing inode ctime, making the original operation record invalid even though the visible filesystem looks restored.
>
> **Mechanics:** The branch correctly discovered that rename updates root ctime. rollback_undone_moves and related helpers return only `Result<()>`. If item A is moved, then moved back during compensation, the old snapshot identity is stale. The app can still reinsert the pre-attempt record.
>
> **Impact:** A subsequent Undo/Redo can fail with "changed or replaced" even though the only change was Marcel's own attempted operation and compensation.
>
> **Recommended fix:** Compensation should return a refreshed retry record, or explicitly state that history must be dropped. Never reinsert an identity record known to predate compensating renames.
>
> **Regression test:** Regression: fail the second item of a two-item Undo, allow rollback of the first, then retry Undo and require either success or explicit history removal.
>
> **Relevant code:** src/file_ops.rs rollback_undone_moves / rollback_failed_redo; src/trash_ops.rs rollback_restored

**P1 Post-commit identity refresh can adopt a replacement object** *\[Live race\]*

> **Summary:** After commit, Marcel sometimes stats the published path and accepts whatever object currently occupies that pathname as the Undo target.
>
> **Mechanics:** refresh_snapshot_identities overwrites the stored identity without first proving device, inode, and kind still match the staged/moved object. The same pattern exists in rename, reverse rename, create directory, archive publication, and Trash restore finalization.
>
> **Impact:** A concurrent actor could move Marcel's output away, replace the pathname, and cause future Undo to act on an unrelated object.
>
> **Recommended fix:** Carry a stable object key from before publication. Post-commit refresh may update ctime only after confirming device, inode, and kind are unchanged. On mismatch, the mutation remains successful but becomes non-undoable.
>
> **Regression test:** Regression: replace the committed destination between rename and refresh using a test hook; verify Marcel never records the replacement.
>
> **Relevant code:** src/file_ops.rs refresh_snapshot_identities, rename_entry, reverse_rename; src/trash_ops.rs restore finalization

**P1 Copy Redo can remove the source from the browser projection** *\[Live, branch-specific\]*

> **Summary:** The fallback DirectoryChanges in redone_transfer assumes every transfer removes completed sources.
>
> **Mechanics:** That assumption is true for Move and false for Copy. A Copy redo that succeeds but loses Undo metadata can return no operation record, causing the fallback to mark the original source as removed from the UI.
>
> **Impact:** The browser can hide a source file that still exists on disk until a watcher/rescan corrects it.
>
> **Recommended fix:** Pass TransferMode to redone_transfer or construct DirectoryChanges separately in the Copy and Move redo paths. Copy should only upsert destinations.
>
> **Regression test:** Regression: force a Copy redo to succeed without an Undo record and verify the source remains visible.
>
> **Relevant code:** src/file_ops.rs redone_transfer

**P1 Undo deletion of copy/archive output is not failure-atomic** *\[Open from original review\]*

> **Summary:** Undo validates a snapshotted output tree and then deletes entries leaf-first at their live destination.
>
> **Mechanics:** After validation succeeds, another process can add or modify an entry. Marcel may delete several original children before a later remove fails. remove_snapshotted_tree then returns Err after partial destruction.
>
> **Impact:** The output is partially removed, while Undo/Redo history may be restored as though the tree remained intact.
>
> **Recommended fix:** Reuse the permanent-delete strategy: atomically quarantine each validated top-level output first, then erase quarantine contents. If erasure becomes incomplete, return committed/partial state rather than ordinary failure.
>
> **Regression test:** Regression: inject a new child after validation but before final directory removal. Verify either nothing is removed or the result is represented as a committed partial state.
>
> **Relevant code:** src/file_ops.rs remove_snapshotted_tree; src/delete_ops.rs provides the stronger model

**P1 Trash retry and rollback still have commit-integrity gaps** *\[Partially fixed\]*

> **Summary:** Trash restoration is safer, but retrash and purge still contain cases where committed work cannot be fully represented or compensated.
>
> **Mechanics:** retrash_records can successfully Trash items whose exact new TrashRecord cannot be identified, then compensate only entries present in outcome.records. Restore removes .trashinfo with an unchecked remove_file after earlier validation. Purge validates a payload identity, then passes only its pathname to delete_trash_backings, reopening a replacement race.
>
> **Impact:** The journal can disagree with Trash state, a replacement metadata file can be removed, or a replaced Trash payload can be permanently erased.
>
> **Recommended fix:** Make Trash outcomes per-item and commit-aware. Use remove_matching_trash_info for cleanup. Pass expected TrashIdentity into delete_trash_backings and verify it at quarantine. Do not return unchanged-style Err after any untracked Trash placement commits.
>
> **Regression test:** Regression: force record-identification failure after successful Trash placement; replace trashinfo between validation and cleanup; replace payload between purge validation and delete quarantine.
>
> **Relevant code:** src/trash_ops.rs, src/delete_ops.rs

**P1 Filesystem operations are still owned by a window** *\[Open from original review\]*

> **Summary:** OperationController and its tasks are scoped to a Marcel window rather than the application.
>
> **Mechanics:** Dropping a GPUI Task does not prove a smol::unblock filesystem job stopped. OperationController has no Drop path that raises cancellation. Closing a window can therefore remove the only controller and progress surface while work continues.
>
> **Impact:** Long copies or archive operations can continue invisibly with no surviving entity to reconcile results or record history. Non-cancellable permanent deletion is especially inappropriate as a window-owned lifecycle.
>
> **Recommended fix:** Move active filesystem operations to an application-global OperationCoordinator. Windows subscribe to progress/results. Add RAII cancellation for cancellable jobs, and explicit detached ownership for non-cancellable commit phases.
>
> **Regression test:** Regression: start copy/archive/delete, close the initiating window, and assert the operation is either cancelled before commit or remains owned/reconciled by the application.
>
> **Relevant code:** src/operations.rs, src/app.rs

**P1 Trash-root protection is lexical rather than physical** *\[Live inference\]*

> **Summary:** The permanent-delete guard checks path overlap with Trash roots using lexical starts_with comparisons.
>
> **Mechanics:** A regular file reached through a symlinked ancestor can physically live inside a Trash root while its lexical path appears outside it. Kernel path resolution during rename/delete follows the ancestor link.
>
> **Impact:** The intended safety rule "ordinary permanent delete must not operate inside system Trash" can potentially be bypassed through an alias path.
>
> **Recommended fix:** For non-symlink targets, resolve/compare the physical parent location or stable directory identity before quarantine. Preserve no-follow behavior for deleting the symlink object itself.
>
> **Regression test:** Regression: create alias -\> ~/.local/share/Trash/files-like fixture, address a regular child through alias, and ensure ordinary delete refuses it while deleting the alias symlink itself remains safe.
>
> **Relevant code:** src/trash_ops.rs paths_overlap_trash_root; src/delete_ops.rs

# 4. P2: important hardening before public release

**P2 Bookmark persistence can lose edits across windows** *\[Live\]*

> **Summary:** Temp-file corruption is fixed, but every window still owns an independent bookmark model and save task.
>
> **Mechanics:** Window A and B can load the same initial set. A saves a new bookmark; B later saves its stale snapshot plus a different edit. Atomic publication works perfectly, but A's change disappears.
>
> **Impact:** User data can be lost with no parse error or warning.
>
> **Recommended fix:** Use one process-global bookmark store/writer, or serialize saves under a lock that reloads and merges the current on-disk model. Last-writer-wins is acceptable for view state, not for bookmark data.
>
> **Regression test:** Regression: two independent models perform disjoint additions and save concurrently; final state must preserve both.
>
> **Relevant code:** src/bookmarks.rs, src/app.rs

**P2 Move history is iterative but still unbounded** *\[Live\]*

> **Summary:** The stack-overflow defect is fixed, but Move can still snapshot arbitrarily large trees for Undo.
>
> **Mechanics:** snapshot_tree allocates one PathSnapshot per descendant with no limit. Up to 100 operation records may remain in the journal. Validation later builds another complete tree and hash map.
>
> **Impact:** Large directory moves can create severe latency and heap pressure or OOM rather than stack overflow.
>
> **Recommended fix:** Give Move the same bounded-history policy as Copy. Moving must remain allowed; exceeding the history budget should downgrade the operation to success without Undo.
>
> **Regression test:** Regression: move a synthetic tree exceeding the configured snapshot limit and verify bounded memory behavior plus explicit non-undoable success.
>
> **Relevant code:** src/file_ops.rs snapshot_tree / OperationJournal

**P2 Cancellation accounting omits untouched sources** *\[Live\]*

> **Summary:** Cancellation records one failure and breaks the transfer loop, leaving all later sources absent from the result.
>
> **Mechanics:** A 100-item transfer cancelled after item 10 may report 10 completed items and one cancellation while 89 unattempted items have no explicit state.
>
> **Impact:** Notifications, clipboard behavior, telemetry, and tests cannot distinguish skipped-by-cancel from silently forgotten.
>
> **Recommended fix:** Represent a per-source outcome or explicit cancelled/skipped list. The operation result should account for every requested source exactly once.
>
> **Regression test:** Regression: cancel a large multi-source operation at a deterministic point and assert completed + failed + cancelled equals requested.
>
> **Relevant code:** src/file_ops.rs transfer_paths_impl

**P2 Permanent-delete completion still conflates stat errors with absence** *\[Open from original review\]*

> **Summary:** The final completion test treats any symlink_metadata error on the quarantine path as proof that deletion completed.
>
> **Mechanics:** NotFound proves absence. PermissionDenied, EIO, and other errors mean the state is unknown.
>
> **Impact:** Marcel can report a root as successfully permanently deleted when it actually became inaccessible or encountered an I/O failure.
>
> **Recommended fix:** Match specifically on io::ErrorKind::NotFound. Other errors must produce an incomplete-delete failure and preserve quarantine recovery guidance.
>
> **Regression test:** Regression: inject PermissionDenied/EIO-like failure at final stat and require an incomplete result.
>
> **Relevant code:** src/delete_ops.rs

**P2 Trash listing silently hides malformed or inaccessible records** *\[Open from original review\]*

> **Summary:** list_trash_records discards record_from_item failures via `filter_map(...ok())`.
>
> **Mechanics:** Broken metadata, permission failures, or unusual Trash entries simply do not appear in Marcel's model.
>
> **Impact:** "Empty Trash" can mean empty the subset Marcel parsed, not necessarily the complete system Trash. Users receive no warning that items were skipped.
>
> **Recommended fix:** Return valid records plus structured warnings/errors. For Empty Trash, either operate through the Trash backend directly or clearly block/announce incomplete enumeration.
>
> **Regression test:** Regression: seed one valid and one malformed Trash record and verify the UI/model surfaces the incomplete state.
>
> **Relevant code:** src/trash_ops.rs list_trash_records

**P2 Archive parser still runs unsandboxed** *\[Open from original review\]*

> **Summary:** Marcel has good archive staging and validation but executes 7-Zip with the user's normal privileges.
>
> **Mechanics:** Entry limits, output limits, path validation, process-group cancellation, and post-extraction checks mitigate archive-bomb and path-traversal behavior. They do not mitigate a vulnerability in the archive parser itself.
>
> **Impact:** A malicious archive exploiting 7-Zip could gain the same filesystem access as Marcel.
>
> **Recommended fix:** Run archive parsing/extraction inside Bubblewrap or a Landlock-style sandbox with read-only access to the archive and write access only to private staging. Keep the current post-extraction validation as defense in depth.
>
> **Regression test:** Regression: sandbox integration test verifies the backend cannot read an unrelated sentinel file or write outside staging.
>
> **Relevant code:** src/archive_ops.rs

**P2 Hosted CI is still absent** *\[Open\]*

> **Summary:** The repository reports strong local checks but no hosted workflow currently proves them for each commit.
>
> **Mechanics:** The branch notes 181 tests plus clean fmt and Clippy. GitHub showed no workflow runs/statuses for the reviewed branch.
>
> **Impact:** Regressions in exactly the failure/race paths being hardened can land unnoticed, especially across architectures and filesystem environments.
>
> **Recommended fix:** Add hosted fmt, Clippy, test, Nix build, and hostile filesystem fixtures. Run at least x86_64 Linux automatically; add aarch64 where practical.
>
> **Regression test:** CI should include deterministic failure injection for commit/rollback boundaries rather than depending only on naturally occurring races.
>
> **Relevant code:** .github/workflows (currently absent) / flake packaging

# 5. P3: lower-tier backlog and polish

These are worth fixing, but they do not currently dominate the safety model. Several are already tracked in docs/TODO.md.

| **Item**                              | **Recommendation**                                                                                                                        |
|---------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| Hide Marcel-owned staging             | Filter .marcel-copy-\* and .marcel-archive-\* from browser/watch projections while operations run.                                        |
| Setuid/setgid/sticky copy policy      | Document whether privileged mode bits should be preserved, cleared, or gated. Current behavior differs from common cp expectations.       |
| RENAME_NOREPLACE diagnostics/fallback | Detect unsupported filesystems and report a specific limitation rather than a generic operation failure.                                  |
| Cross-device move preflight           | Reject/label cross-filesystem drops before the user drops rather than accepting and failing afterward.                                    |
| Terminal child reaping                | Avoid zombies from Open in Terminal child processes.                                                                                      |
| Thumbnail cache permissions           | Create the freedesktop thumbnail cache directory with 0700 semantics.                                                                     |
| IconProvider reuse                    | Avoid reconstructing icon discovery/cache state for every watcher batch.                                                                  |
| Subprocess argument terminators       | Use -- consistently for pdftoppm, pdfinfo, and gio open even where current paths are absolute.                                            |
| Source identity after long copy       | Optionally verify the source object did not change identity during a lengthy copy if Marcel wants stronger copy-consistency guarantees.   |
| PDF/cache hardening                   | Use stronger content/cache invalidation where same-size, mtime-preserved replacements matter; bound temporary conversion output by bytes. |

# 6. Transaction model: the next architectural step

The branch has already found the right conceptual center: filesystem mutation and Undo metadata are separate outcomes. The next step is to make this true for compound operations as well.

```rust
enum MutationResult {
    Unchanged(anyhow::Error),

    Committed {
        changes: DirectoryChanges,
        history: Option<OperationRecord>,
        warning: Option<String>,
    },

    Partial {
        changes: DirectoryChanges,
        recovery: Option<OperationRecord>,
        error: anyhow::Error,
    },
}
```

The exact type name is less important than the invariant: once a filesystem mutation commits, the code path must return an outcome that describes the new state. Plain Err is reserved for paths where Marcel can prove that no committed effect escaped.

## A useful operation pipeline:

1.  Prepare: validate names, destinations, permissions, identities, and traversal limits. Build all fallible metadata required for the operation where possible.

2.  Commit: perform one minimal filesystem mutation or one explicitly tracked step of a compound mutation.

3.  Finalize: perform only infallible path rebasing plus identity refresh that can downgrade Undo but cannot retroactively turn success into failure.

4.  Compensate: if a later compound step fails, compensation itself returns refreshed state/history, never just ().

5.  Publish: the UI reducer consumes exact committed changes. Undo metadata is optional and must never be used as a proxy for visible effects.

## Recommended separation of concerns:

- **OperationCoordinator:** application-global owner of active jobs, cancellation, progress, journal, clipboard, and committed results.

- **Mutation engine:** pure filesystem code with explicit prepare/commit/partial result semantics and no GPUI knowledge.

- **DirectorySession:** projection/cache of one visible location. It reacts to committed DirectoryChanges and watcher events.

- **Window/UI:** subscribes to operation progress and renders notifications. Closing a window does not own the lifetime of filesystem work.

- **History:** stores only records verified to describe the current filesystem identity. Compensation either refreshes the record or drops it.

# 7. Good ideas to take from Nautilus

> **Guiding rule**  
> Borrow Nautilus's solved product and lifecycle ideas, not its entire GTK/GIO/GObject architecture. Marcel's advantage comes from being a focused Rust/GPUI local file manager with explicit semantics.

## 7.1 Make operations application-level infrastructure

Nautilus treats file operations as background jobs whose lifetime is not conceptually tied to one view. Marcel should take this almost directly at the architectural level: one process-global coordinator, persistent progress, cancellation state, and result delivery to any surviving window.

- Move OperationController out of Marcel/window state.

- Allow a new window to see operations started elsewhere.

- Do not lose reconciliation/history when an initiating window closes.

- Use weak UI references; the filesystem job survives or cancels according to its own policy.

## 7.2 Add a location abstraction before adding remote filesystems

Nautilus works through GFile/GIO, so local paths, remote locations, mounts, and virtual locations share one broad interface. Marcel should not necessarily adopt GIO internally, but it should stop letting PathBuf define the entire model before remote/mounted/Trash locations arrive.

- Define a Location enum/trait with LocalPath as the first backend.

- Model Trash as a virtual location rather than pretending its backing paths are an ordinary directory.

- Later add mounted volumes and remote URIs behind the same UI-level location contract.

- Keep local operations optimized and explicit rather than forcing every local call through a generic remote API.

## 7.3 Copy Nautilus's conflict-decision model, not silent overwrite behavior

Nautilus has a mature conflict workflow: replace, replace all, merge, merge all, skip, skip all, and rename. Marcel's current no-overwrite rule is a good default and should remain the safety baseline. The lesson is to add explicit user decisions when conflicts become a supported feature.

- No silent overwrite remains the default invariant.

- Conflict resolution returns a decision object to the operation engine.

- Apply-to-all should be explicit state within one transfer.

- Directory merge should be a separate, well-tested operation rather than an accidental side effect of copy.

## 7.4 Separate search engines from directory filtering

Nautilus distinguishes current-view behavior from broader search/index services. Marcel's fuzzy current-folder filter is excellent for navigation and should stay. A future search feature should be a separate subsystem rather than stretching the filter into a filesystem crawler.

- Keep instant in-folder fuzzy filter unchanged.

- Add simple recursive search as a fallback engine.

- Optionally integrate an indexed engine on Linux later.

- Represent search results as a virtual location so normal preview/selection behavior still works.

## 7.5 Build a real Properties/permissions surface

Nautilus handles ownership, groups, recursive permissions, metadata, and richer file properties. Marcel does not need all of that immediately, but read-only Properties is high-value infrastructure because it creates one place for identity, MIME, size, timestamps, permissions, links, and filesystem details.

- Start read-only.

- Use the same Properties presentation for in-app and D-Bus requests.

- Add permission editing only after operation semantics and validation are clear.

- Expose device/inode and symlink target in an advanced section: Marcel already relies heavily on identity.

## 7.6 Treat mounts and volumes as first-class state

Nautilus has explicit mount, unmount, eject, volume, and removable-media operations. Marcel should eventually model this rather than discovering mount paths as ordinary folders.

- Places entries should know whether they are local folders, mounts, or removable devices.

- Unmount/eject operations need their own lifecycle and busy-state handling.

- Cross-device move decisions become clearer when source/destination device identity is part of the location model.

## 7.7 Adopt Nautilus-level testing discipline

Nautilus has displayless file-operation tests, UI/display tests in a compositor, sanitizer jobs, architecture coverage, packaging builds, and release pipelines. This is one of the highest-value things Marcel can copy with almost no product downside.

- Hosted cargo fmt, Clippy, tests, and Nix builds on every branch/PR.

- Headless Wayland interaction tests for selection, dialogs, DnD, and window closure.

- Failure-injection fixtures for the exact commit/rollback invariants Marcel cares about.

- x86_64 plus aarch64 release checks.

- Periodic sanitizer/Miri-style checks where dependencies permit.

## 7.8 Keep desktop integration as a deliberate contract

Nautilus provides FileManager1, search-provider, portal, extension, and file-operation D-Bus surfaces. Marcel is already deliberately handling FileManager1 ownership. Continue this cautious approach.

- Installing Marcel must not silently seize generic file-manager ownership.

- D-Bus inputs remain untrusted and bounded.

- Finish ShowItemProperties through the same Properties UI.

- Treat portal/file-picker backend work as a separate project rather than a checkbox.

## 7.9 Use global progress and persistent operation semantics

Nautilus users can reason about long-running work independently of the current folder view. Marcel's progress panel is good; the next step is making it represent application-level jobs and partial outcomes.

- One progress object per operation with exact per-item state.

- Completed/failed/cancelled/skipped counts sum to requested items.

- Closing or navigating views never erases the operation state.

- Notifications should distinguish full success, success without Undo, partial success, cancellation, and rolled-back failure.

## 7.10 Preserve Marcel's stricter destructive-operation philosophy

This is a lesson from comparison rather than something Nautilus does better. Marcel's quarantine-first permanent delete is one of its strongest designs. Extend that philosophy into Undo cleanup and other destructive compound actions rather than replacing it with generic recursive delete behavior.

- Quarantine before irreversible erasure.

- Identity-check before destructive action.

- Do not allow cancellation once irreversible erasure starts unless the result can be represented precisely.

- Expose recovery guidance for interrupted quarantine remnants.

# 8. Ideas not to copy from Nautilus

Nautilus is a mature general-purpose desktop component. Some of its complexity is the price of that role, not a goal Marcel should inherit.

| **Avoid**                                                   | **Why**                                                                                                                            |
|-------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| Do not rewrite Marcel around GObject/GIO                    | Take the location abstraction idea, but keep Rust-native local operation primitives and explicit identity/transaction semantics.   |
| Do not broaden scope before the operation model is finished | Remote filesystems, plugins, cloud providers, and portal backends multiply edge cases. Finish local transaction integrity first.   |
| Do not give up preview-first layout                         | Nautilus has broader platform integration, but Marcel's persistent preview is a core product advantage and should remain central.  |
| Do not silently adopt overwrite/merge semantics             | Nautilus supports them because users need them. Marcel should add them only as explicit decisions with hardened tests.             |
| Do not let compatibility layers hide filesystem semantics   | A generic backend can turn precise local guarantees into backend-dependent behavior. Keep local contracts documented and testable. |
| Do not recreate Nautilus's breadth inside app.rs            | Extract coordinators when a concrete ownership boundary exists. Avoid a giant event bus or speculative trait forest.               |

# 9. Recommended implementation order

This sequence closes the correctness model before adding new capabilities.

1. Fix transfer reconciliation so DirectoryChanges always comes from exact CompletedTransfer results. Remove or explicitly define partial Move Undo.

2. Introduce Partial/RolledBack outcomes for compound mutations. Plain Err becomes legal only before the first commit.

3. Refresh identities safely: verify device, inode, and kind before updating ctime. Never adopt a replacement object into history.

4. Make Undo deletion quarantine-based and failure-atomic for Copy and archive outputs.

5. Close Trash identity gaps: identity-checked metadata cleanup, expected identity carried into purge, and commit-aware retrash compensation.

6. Move OperationController into an application-global OperationCoordinator. Add explicit window-close behavior and per-item operation state.

7. Make bookmark storage process-global. Keep browser-state last-writer-wins if desired.

8. Bound Move history and complete cancellation accounting.

9. Fix permanent-delete NotFound handling and physical Trash-root checks.

10. Add hosted CI and deterministic failure injection. Only then resume public-release and feature work.

11. After the hardening gate: read-only Properties, New File, Duplicate/Move To, conflict UI, mounts/removable media, then remote locations.

# 10. Regression and CI matrix

| **Area**                       | **Fixture**                              | **Required invariant**                                    |
|--------------------------------|------------------------------------------|-----------------------------------------------------------|
| Mixed move                     | Normal dir + socket dir in one move      | Both disk/UI effects represented; Undo semantics explicit |
| Partial Undo                   | Fail Nth rename                          | Return Partial or refreshed RolledBack, never stale Err   |
| Replacement race               | Swap committed object before refresh     | No Undo record targets replacement                        |
| Copy Redo without history      | Force snapshot/history loss              | Source remains visible; destination appears               |
| Undo output race               | Add child after validation               | No silent partial deletion                                |
| Retrash identification failure | Trash succeeds but record lookup fails   | Committed state represented; no phantom unchanged result  |
| Trash metadata replacement     | Replace .trashinfo before cleanup        | Replacement preserved                                     |
| Trash purge replacement        | Replace payload after validation         | Replacement not deleted                                   |
| Window close                   | Close initiating window mid-copy/archive | Job cancelled or survives under app-global owner          |
| Bookmarks multi-window         | Two disjoint edits                       | Final set contains both edits                             |
| Huge move                      | Exceed snapshot budget                   | Move succeeds, memory bounded, Undo disabled              |
| Cancellation                   | Cancel after deterministic item count    | Every requested source accounted for                      |
| Permanent delete final stat    | Inject non-NotFound error                | Incomplete result, never success                          |
| Symlinked Trash ancestor       | Access Trash child through alias         | Ordinary delete refused                                   |
| Archive sandbox                | Backend attempts unrelated read/write    | Sandbox denies access                                     |
| Deep tree                      | Thousands of nested directories          | No stack overflow                                         |
| Large directory UI             | 50k entries + select all                 | No O(n) scan/clone per frame                              |

## Suggested hosted pipeline:

- cargo fmt --check

- cargo clippy --all-targets --all-features -- -D warnings

- cargo test --all-targets --all-features

- nix flake check / package build

- headless Wayland smoke suite for core interactions

- failure-injection operation tests

- x86_64 Linux mandatory; aarch64 Linux release gate

- periodic ASan-equivalent checks for C dependencies and external tooling where applicable

# 11. Release gate

A practical definition of "safe enough for a public 0.1.0" for Marcel:

- No known P0.

- All P1 operation-integrity findings closed with regression tests.

- Compound operations cannot return an unchanged-style error after a commit without exact partial-state reporting.

- Undo/Redo records are never reinserted after compensation unless their identities have been refreshed.

- Window closure cannot orphan filesystem work.

- Copy, Move, Trash, Restore, permanent delete, archive create/extract, Undo, and Redo each have deterministic failure-injection tests.

- Hosted fmt/Clippy/test/Nix checks pass on the release commit.

- Destructive-operation manual smoke matrix passes on home filesystem plus at least one mounted/removable filesystem.

- Archive parsing is sandboxed or the unsandboxed risk is explicitly accepted for the personal-only release.

- Known P2/P3 deferrals are documented in release notes rather than implicitly presented as complete functionality.

> **What "done" should mean**  
> The goal is not to make Marcel as broad as Nautilus before 0.1.0. The goal is to make Marcel's smaller promise unusually trustworthy: fast local browsing, excellent preview, predictable no-overwrite transfers, and destructive operations whose UI/history state never lies about what happened on disk.

# 12. Sources reviewed

Marcel branch sources

- Operation review and remediation status: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/docs/review-2026-08-05.md](https://github.com/berker-z/marcel/blob/operation-commit-integrity/docs/review-2026-08-05.md)

- File operations and transaction journal: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/file_ops.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/file_ops.rs)

- Application reconciliation and Undo/Redo UI: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/app.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/app.rs)

- OperationController: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/operations.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/operations.rs)

- Trash operations: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/trash_ops.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/trash_ops.rs)

- Permanent deletion: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/delete_ops.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/delete_ops.rs)

- Directory session and path index: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/directory_session.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/directory_session.rs)

- Selection revisions: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/selection.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/selection.rs)

- Persistent browser state: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/state.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/state.rs)

- Bookmarks persistence: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/bookmarks.rs](https://github.com/berker-z/marcel/blob/operation-commit-integrity/src/bookmarks.rs)

- Current backlog: [https://github.com/berker-z/marcel/blob/operation-commit-integrity/docs/TODO.md](https://github.com/berker-z/marcel/blob/operation-commit-integrity/docs/TODO.md)

Nautilus sources used for architectural comparison

- Repository: [https://github.com/GNOME/nautilus](https://github.com/GNOME/nautilus)

- File operation interface: [https://github.com/GNOME/nautilus/blob/main/src/nautilus-file-operations.h](https://github.com/GNOME/nautilus/blob/main/src/nautilus-file-operations.h)

- Displayless test suite: [https://github.com/GNOME/nautilus/blob/main/test/displayless/meson.build](https://github.com/GNOME/nautilus/blob/main/test/displayless/meson.build)

- CI pipeline: [https://github.com/GNOME/nautilus/blob/main/.gitlab-ci.yml](https://github.com/GNOME/nautilus/blob/main/.gitlab-ci.yml)

- AppStream metadata and declared integration surfaces: [https://github.com/GNOME/nautilus/blob/main/data/org.gnome.Nautilus.metainfo.xml.in.in](https://github.com/GNOME/nautilus/blob/main/data/org.gnome.Nautilus.metainfo.xml.in.in)

## Review limitations

The GitHub connector provided current branch source and repository metadata. Local network isolation prevented an independent clone/build in the container. Therefore runtime claims attributed to the repository's own review document, such as the 181-test result and measured deep-tree/per-frame benchmarks, are reported as repository evidence rather than independently reproduced in this pass.

# Conclusion

Marcel is now past the stage where the primary concern is obvious unsafe file-manager code. The side branch fixes the major deterministic transaction bugs, deep-recursion aborts, hot-path scaling problems, and several persistence races. The remaining work is more specific and more architectural: model partial commit explicitly, keep history identities truthful through compensation, and make filesystem work belong to the application rather than a window.

That is also where Nautilus is most useful as a reference. Its strongest lessons for Marcel are operation lifetime, location abstraction, conflict decisions, platform integration discipline, and testing depth. Marcel should borrow those solved ideas while keeping the things it already does unusually well: preview-first browsing, explicit local filesystem semantics, no silent overwrite, identity-aware history, and quarantine-first destructive operations.
