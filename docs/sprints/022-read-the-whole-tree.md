# Sprint 22: Read the whole tree

**Status:** Implemented — every code item below is delivered with regression
coverage where a test can reach it, and the local quality gate is green. The
graphical acceptance matrix inherited from Sprints 20 and 21 remains unrun and
gains a short list of checks of its own below.

The suite is 260 library tests plus 1 binary test, up from 252.

## Goal

Review the entire application — not the operations core again — and close what
that review finds in the same pass.

Four reviews had read `file_ops.rs` and its neighbours, each finding real
defects a green gate missed, and each hardening the same region further. The
code those reviews never read had no such history: the load/watcher seam, the
preview surface, bookmarks persistence, the D-Bus surface, and the window
layer Sprint 21 had just added. This sprint ran a fifth review across all of
it, cross-checked every candidate finding against the exact code path, and
fixed everything that survived verification.

The evidence document is [`review-2026-08-20.md`](../review-2026-08-20.md)
(Review E): per-finding tiers, code paths, and the confirmed-sound list. This
sprint records what was *done* about it.

## What was wrong

Twenty confirmed defects, ten of them P1. The pattern repeats Review D's
closing point — the quality gate was green over every one — with a new
corollary: the defects lived almost entirely where no reviewer had read.

- **The operations core held exactly one defect (E1), and it was in code
  Sprint 20 itself shipped.** Undo of a copy that included a merge removed the
  merge's additions one commit at a time, and then, if validating the copy's
  own output failed, returned `Unchanged` — the record stayed in the journal
  claiming a disk state that no longer existed, and no retry could ever
  validate again. The same "`Result` is the wrong type after the first
  irreversible write" shape as D1, one function over.
- **The preview surface trusted file names to describe file contents (E2).**
  `open(2)` on a FIFO with no writer blocks forever; a cancellation flag
  cannot interrupt it, and dropping a GPUI task does not unwind a
  `smol::unblock` closure already running — the pool thread is lost for the
  life of the process. Every preview, thumbnail, and activation open was
  reachable this way, thumbnails by merely scrolling a grid past a FIFO named
  like an image.
- **Bookmarks could destroy themselves (E3, E18).** A failed load left an
  empty in-memory list with nothing marking the store read-only; the next
  drag-in atomically saved that emptiness over the user's real file. An edit
  racing the still-async load lost data by a different route to the same end.
- **The load/watcher seam assumed streams, events, and operation results never
  interleave (E4, E5).** The watcher only started after enumeration finished,
  so every external change during a large scan was silently lost; and an
  operation result applied mid-stream inserted an entry the stream then
  inserted again, because the sorted merge does not deduplicate.
- **Sprint 21 made every launch a window and nothing bounded launches (E6).**
  `org.freedesktop.Application.Open` takes 64 URIs per request from any
  session-bus peer, with no cross-request limit — and the packaged
  FileManager1 variant answers on a name sandboxed applications are routinely
  granted.
- **A cluster of single-window UX defects (E7–E10)** where what the user saw
  disagreed with what the application knew: reveal selected an item without
  scrolling to it; a new folder opened at the previous folder's scroll offset;
  a silently promoted primary kept the vanished file's preview on screen while
  the footer named the new one; incrementally added Trash rows sorted,
  filtered, and displayed by the invisible backing name; and bookmark menus
  and drags acted on an index another window could have shifted.
- **The remainder (E11–E21)** spanned the D-Bus name handshake (another file
  manager owning `FileManager1` read as "another Marcel is running" and
  forwarded the launch into an activation loop), a stray window on every cold
  D-Bus activation, an injectable and unbounded PDF page count, Trash-purge
  reconciliation by original path instead of by entry, stale operation
  removals deleting recreated files from the view, reveal clobbering, Escape
  in one window cancelling another window's transfer, symlinked config files
  replaced by regular files, and a live second instance's delete quarantine
  reported as an interruption.

## Decisions taken

- **A file is what its opened descriptor says, not what its name says.**
  `local_fs::open_regular_file` opens with `O_NONBLOCK` and verifies the
  *opened* descriptor is a regular file, so the check cannot be raced, and on
  a regular file the flag changes nothing. Every open on the preview surface —
  sniffing, text, images, animations, thumbnails, portal activation — goes
  through it. Selecting or scrolling past something can never cost a worker
  thread.
- **A store that cannot prove it holds the user's data refuses to write.**
  `BookmarkStore` records *why* it must not be modified — load failed, load
  still running, or the file holds lines Marcel cannot represent — and answers
  every mutation with that reason on the asking window. Unparseable lines are
  counted, not silently pruned: Marcel never writes such lines, so their
  presence means the file holds something saving would destroy. Duplicates
  carry no data and are still collapsed.
- **While a load streams, the stream owns the listing.** The watcher starts
  when the load starts, not when it finishes; events and operation results
  that arrive mid-stream are deferred *as paths* and re-validated in one
  catch-up batch at `Done`. Deferring paths rather than event payloads
  collapses every ordering question into "stat it again", which is the same
  answer the watcher already gives. A rescan requested mid-stream defers the
  same way.
- **Every applied change is re-validated, removals included.** An operation's
  `removed` list can be stale by the time it lands; re-statting it costs one
  lookup and stops a recreated file from being deleted from the view.
- **Undo validates before it removes, when removal is not atomic.** The merge
  path's removals commit one at a time, so the copy's own output is validated
  first; a failure after any removal discards the record and reports what was
  removed, never `Unchanged`.
- **Identity, not position, names the target of a destructive click.** The
  bookmark context menu and drag carry the bookmark's path and re-verify it at
  the store; an index that no longer holds that path is a no-op, and a menu
  that no longer describes its bookmark disappears.
- **Cancellation authority follows the surface.** Escape cancels the running
  operation only from the window that started it — or from anywhere once that
  window is gone, exactly the re-homing rule reports already follow. The
  progress card's explicit Cancel stays available everywhere: clicking it says
  what it means.
- **A launch is a window, and windows are bounded.** `MAX_LIVE_WINDOWS` (32)
  caps the registry; a refused batch says so on a live window. Well past what
  a person uses, well before a session-bus peer can freeze the desktop.
- **The generic file-manager name is an opt-in extra, never a startup
  condition.** The connection is built owning only the application name;
  `org.freedesktop.FileManager1` is requested afterwards and its refusal is
  logged and survived. `NameTaken` now means exactly one thing: another
  Marcel.
- **A D-Bus activation waits for its request.** A cold `DBusActivatable`
  start runs `marcel` argument-less in the daemon's working directory and then
  delivers the real request over the bus; the initial window is deferred when
  the starter-bus environment says that is what happened, and an `Activate`
  arriving with no window to raise opens one.
- **Untrusted numbers get bounds wherever they enter.** The PDF page count is
  parsed from the *last* `Pages:` line (metadata strings print earlier and can
  embed newlines straight out of the document), capped at 50,000, and the cap
  is re-applied when reading the cache, so a poisoned cache heals. The
  watcher's raw-event drain is capped. WebP animation decoding gets the same
  limits GIF already had.

## Correctness contract

- No path the user can select, scroll past, or activate can block a worker
  thread indefinitely.
- A mutation that changed the disk is never reported as having changed
  nothing, on any path, including compensation paths.
- Marcel never persists a bookmark list it cannot prove is the user's, and
  says why when it refuses.
- What the browser shows converges to the disk regardless of how loads,
  watcher events, and operation results interleave.
- A destructive click acts on the object the user aimed at, or on nothing.
- No session-bus peer can make Marcel consume unbounded windows, memory, or
  layout.
- Configuration writes preserve what the user set up, symlinks included.

## Delivered scope

### Blocking — Review E P1s

- [x] **E1.** Undo of a copy-with-merge validates the copy's output before
  removing the merge's additions; any failure after those removals is
  `Discarded` with the removals reported, never `Unchanged`.
- [x] **E2.** `open_regular_file` guards every open in `preview.rs`,
  `image_preview.rs`, `thumbnails.rs`, and `system_open.rs`; non-regular files
  get a metadata preview instead of an open.
- [x] **E3.** `BookmarkStore` is read-only after a failed load, during the
  load, and when the file holds unrepresentable lines, with the reason
  reported on the asking window; the failed-save path re-checks for edits made
  during the save.
- [x] **E4.** The watcher starts with the load; mid-stream events defer as
  paths and are re-validated at `Done`; a mid-stream rescan request defers the
  same way.
- [x] **E5.** Operation results arriving mid-stream join the same deferred
  catch-up instead of being applied against a half-streamed listing.
- [x] **E6.** `MAX_LIVE_WINDOWS` bounds the registry; a refused `Open` batch
  reports itself.
- [x] **E7.** Reveal scrolls the revealed item into view; navigation resets
  the scroll offset while a same-folder refresh keeps it; the Trash view
  resets on entry the same way.
- [x] **E8.** `reconcile_selection` emits a preview for a promoted primary.
- [x] **E9.** Incremental Trash rows go through `set_name`, so sorting,
  filtering, and non-UTF-8 display agree with the name on screen.
- [x] **E10.** Bookmark removal and reordering verify the bookmark's path; a
  stale menu hides itself.

### Blocking — Review E P2s

- [x] **E11.** The generic name is requested after `build()`; its refusal is
  survived. The private-bus integration test gains the scenario: a foreign
  owner of `FileManager1`, Marcel still starting as primary, the foreign owner
  keeping the name.
- [x] **E12.** PDF page counts: last-line parse, 50,000 cap, cache re-check.
- [x] **E13.** A purge outcome names the exact records it purged; the Trash
  view reconciles by entry, so a surviving twin of a purged original stays
  listed.
- [x] **E14.** Applied changes re-stat removed paths as well as upserted ones.
- [x] **E15.** Only an operation that carries a reveal may touch the pending
  reveal set.
- [x] **E16.** `can_cancel_from` gates the Escape shortcut by operation
  origin.
- [x] **E17.** A cold bus activation defers its initial window until the
  request arrives; an `Activate` with no window opens one.
- [x] **E18.** Mutations racing the bookmark load are refused with a report
  rather than silently losing whichever side finished last.
- [x] **E19.** Bookmark and state saves resolve a symlinked file to its target
  before the temp-and-rename.
- [x] **E20.** Interrupted-deletion guidance checks the quarantine owner's
  liveness; another live instance mid-delete is not an interruption.

### Smaller items, fixed here

- [x] WebP animation decode sets the same limits as GIF.
- [x] The watcher's raw-event drain is capped (`MAX_RAW_EVENTS_PER_BATCH`).
- [x] A relative `XDG_CONFIG_HOME` is ignored per the base-directory spec, in
  bookmarks, state, and places.
- [x] List-view Left/Right with no selection are no-ops instead of jumping to
  the end of the folder; grid Up in the top row stays put.
- [x] Copy/Cut stage the clipboard even while an operation runs; Paste still
  waits for the busy lock.
- [x] A rename input that loses focus holding an invalid name cancels instead
  of pulling focus back on every click.

### Documentation

- [x] [`review-2026-08-20.md`](../review-2026-08-20.md) records the review:
  verdict table, evidence, the deferred items with reasons, and the
  confirmed-sound list so a future review does not re-derive it.
- [x] `TODO.md` and `HANDOFF.md` reflect the new state; the four deferred
  items are folded into the backlog with their reasons.

## Acceptance checks

### Automated

- [x] An undo refused before removing anything leaves the merge's additions in
  place and keeps its record; one that removed them discards and reports the
  removals.
- [x] A purge outcome names exactly the purged records, including when two
  Trash entries share one original path.
- [x] A metadata string cannot supply the PDF page count; counts past the
  bound are rejected, cached ones included.
- [x] A load with unrepresentable bookmark lines counts them; saving through a
  symlinked bookmark file updates the target and keeps the link.
- [x] A promoted primary is emitted as a preview; deferred refresh and rescan
  state is scoped to one load.
- [x] Quarantines owned by live processes produce no interruption guidance;
  provably dead owners do.
- [x] The private-bus integration test covers the foreign `FileManager1`
  owner scenario.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets` in the declared development environment, with
  `desktop_integration::tests::private_session_bus_integration` confirmed
  outside the sandbox. 260 library tests plus 1 binary test.

### Manual — needs a graphical session, added to the standing matrix

These join the unrun Sprint 20/21 matrix rather than replacing any of it.

- [ ] Reveal a file deep in a large folder (location bar, D-Bus ShowItems, or
  paste) and confirm the browser scrolls to it.
- [ ] Navigate between two large folders and confirm each opens at the top; a
  watcher-triggered refresh of the same folder keeps your place.
- [ ] Select several items, toggle Show Hidden so the previewed one
  disappears, and confirm the preview follows the new primary.
- [ ] Browse a directory containing a writerless FIFO named `something.png`
  in grid view; confirm thumbnails elsewhere keep arriving and selecting the
  FIFO shows a metadata card.
- [ ] With Nautilus (or any owner of `org.freedesktop.FileManager1`) running,
  launch the packaged FileManager1 variant of Marcel and confirm it opens
  normally without stealing the name.
- [ ] Cold-launch Marcel by opening a folder through desktop activation and
  confirm exactly one window appears, at that folder.
- [ ] Start a transfer in window A; press Escape in window B and confirm the
  transfer continues; press Escape in window A and confirm it cancels; close
  window A mid-transfer and confirm Escape in window B now cancels.
- [ ] Open a bookmark's context menu in window A, remove a different bookmark
  in window B, and confirm the click in A removes the right bookmark or
  nothing.

## Out of scope

Deferred by decision, recorded in [`TODO.md`](../TODO.md) with reasons:

- **Rescan backoff and selection preservation.** A churning directory can
  loop full reloads, each clearing selection and any in-progress rename.
  Keeping the user's place across a reload is a design decision, not a
  cleanup.
- **The Trash view in Back/Forward history.** Needs the planned
  virtual-location abstraction; wedging a special case into
  `NavigationHistory` would fight it.
- **Bounding the folder preview.** Its merge is quadratic on the foreground
  executor and its retention unbounded, but deciding what a bounded glanceable
  preview shows is a product question.
- **Thumbnail cache keying per the freedesktop spec** for symlinked paths.
  Interop only; no data at risk.
- **The graphical acceptance matrix.** Still the gate between this tree and a
  release-readiness decision, now with the additions above.
