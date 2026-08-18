# Marcel session handoff

**Prepared:** 2026-08-18
**Branch:** `master`
**Workspace:** `/home/berkerz/Projects/marcel`

Read `AGENTS.md` first, and `CLAUDE.md` for how to run the checks without one
approval prompt per command.

## Where things stand

[Sprint 20](sprints/020-cleanup-interlude.md) closed the code queue. It was a
deliberate interlude rather than a feature sprint: [Review D](review-2026-08-18.md)
found two P0 defects in code Sprint 18 shipped, three sprints had unchecked
boxes, and [`review-2026-08-10.md`](review-2026-08-10.md) had four unstarted
stages. Working them separately would have meant touching `file_ops.rs` three
times.

What that means in practice:

- A merge that stops part way now returns what it added, so Undo can take it
  back, and a cancelled merge is reported as cancellation instead of failure.
- An original Marcel fails to put back is promoted out of undo storage into
  `.marcel-recovered-*` recovery storage, which nothing sweeps and the browser
  points the user at. The abandoned-quarantine sweep used to delete it.
- Every quarantine deletion validates identity first.
- One snapshot budget covers a whole transfer — copy, merge, and move.
- Undo of copy and archive output is quarantine-first, via `delete_ops`.
- The Trash-root guard compares physically; the Trash listing reports what it
  could not read instead of dropping it.

Sprints 17 and 18's remaining items are code-complete, and Stages 1–5 and 7 of
the 2026-08-10 plan are done.

**No graphical acceptance run has happened.** Sprint 20 carries Sprint 19's
eight multi-window checks and Sprint 18's four interaction checks unchanged, and
none of them can be reached by `cargo test`.

[Sprint 21](sprints/021-a-launch-is-a-window.md) then fixed the thing that made
that matrix impractical. Running `marcel` while Marcel is open used to navigate
the window you were reading — or, with no argument, raise it and ignore the
folder you were standing in. A launch now opens a window, folders have an
Open in New Window entry, and the application owns the window list rather than
`main.rs` keeping a private one.

## Next: the graphical acceptance matrix, then release readiness

In order:

1. Run Sprint 20's manual list with two real windows — now reachable with two
   terminal commands rather than a hand-written `gdbus call`. That is the last
   thing between the current tree and a release-readiness conversation. Run
   Sprint 21's own short list while you are there.
2. Fix what it exposes, with focused regression coverage.
3. Then, and only then, reopen [Sprint 16](sprints/016-public-release-presentation.md):
   hosted CI, release metadata, and a tagged `0.1.0`.

Still open by decision, not by omission: the journal-wide snapshot budget
(Stage 6's remainder), hosted CI (Stage 8), the drop device-identity affordance
(reasoned in Sprint 20), and merging a folder while *moving* it.

## Four things that will save time

**Keep `TMPDIR` short.** The special-file tests bind Unix sockets, and
`sun_path` is 108 bytes. A long scratchpad path fails six of them at once with
errors that look like sandbox problems and are not.

**Upstream first, and record it when there is nothing to take.** Yazi is at
`319f90e0`, Nautilus at `f67b2e1`; clone them into the scratchpad. Sprint 19's
upstream section is what a verified one looks like — file and line for every
claim.

**A rename or a removal moves an inode's ctime.** This is now behind three
separate fixes: the move path, the merge snapshot ordering, and — new in Sprint
20 — permanent deletion of a tree holding two hard links to one file, where the
plan's own first removal invalidated its entry for the second. When an identity
check refuses something that should obviously have worked, ask what moved the
ctime before assuming interference.

**After the first irreversible write, `Result` is the wrong type.** Review D
found the merge bug by looking for exactly this, and it is the shape to look for
next time: the comments understood the invariant, the types elsewhere encoded
it, and one fresh code path used `?` after a commit anyway.

## Running it

See `CLAUDE.md`. In short: capture the `nix develop` environment once, then run
fmt, clippy, and tests as one sandboxed command with a short `TMPDIR`. Confirm a
green `desktop_integration::tests::private_session_bus_integration` outside the
sandbox. The suite is 252 library tests plus 1 binary test.

Automated checks did not catch a single one of Sprint 18's three interface
defects, and did not catch either of Review D's P0s. Drive the real windows
before calling interaction work done.
