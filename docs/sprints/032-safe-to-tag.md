# Sprint 32: Safe to tag

**Status:** Code done 2026-09-27; the tag waits for the hand checks in
[`release.md`](../release.md#v010-release-gate), which need a person, a stick,
and a share. First of three cleanup sprints (32, 33, 34) that come out of
[`../review-2026-09-27.md`](../review-2026-09-27.md). No new features in any
of them. The gate is green at 467 tests (449 before it), both bus tests run
unsandboxed.

## Goal

Fix what can lose data or show the wrong thing, bring the bookkeeping back in
line with the tree, and tag v0.1.0.

The tag has waited since Sprint 26. In the meantime drives, network shares,
the desktop clipboard, and HEIF landed and were pushed, and the CHANGELOG
already lists them under 0.1.0. So 0.1.0 is what is on master at the end of
this sprint, and the release gate grows to cover what it now contains. The
alternative (tagging `7a5ab84` and calling the rest unreleased) would tag a
build nobody runs.

Sprint 33 rewrites how Marcel knows where it is. That is the right change and
also the riskiest one of the three sprints, so the tag goes in before it,
on a tree whose known bugs are fixed.

## Where the code is now

The cross-device move from Sprint 29 gets the big things right. Everything is
fsynced before the source goes, the source is removed by one rename into
quarantine, and undo publishes with `RENAME_NOREPLACE`. What it does not do
is carry Sprint 28's descriptor discipline all the way through.
`verify_copied_tree` (`fsops/transfer.rs:837`) re-reads both trees by path and
compares kind and length. Then `delete_paths(&[source])` removes the source
with no identity key. A file rewritten in place between the copy and the
removal, same size, loses the new writes.

The rest of the list is smaller but real. The review has file and line for
each; the numbers below are its bug numbers.

## Work

### Data safety in `fsops`

- **(1) Keep the newest writes on a cross-device move.** The Copier already
  holds each source through a descriptor and has its metadata. Record
  `(dev, ino, size, mtime, ctime)` per regular file as it is copied, and have
  verification compare the source against that record, not against a fresh
  path lookup. Any difference keeps both copies and reports it. Removal of the
  root takes the root's `ObjectKey`, as `delete_trash_backings` does
  (`delete.rs:137`). Verification then lives in the copier, where the
  descriptors are, and `verify_copied_tree` goes.
- **(3) Do not lose links on a move to FAT.** In Move mode, a tree where the
  copier skipped links (`LinksSkipped`) keeps its source and says why. A copy
  still drops them with the note it gives today. Undo onto a source that had
  modes FAT could not hold restores the modes recorded at copy time, not what
  the stick reports.
- **(4) Hardlinks on FAT.** `fs::hard_link` (`copy.rs:491`) goes through
  `filesystem_cannot_hold`; on EPERM it copies the bytes and records one loss.
  The FAT fault hook gains `hard_link` and `set_times`.
- **(5) Say what happened when a failure comes after the copy is published.**
  `move_across_devices` returns a typed error that says whether the copy is
  published. Undo whose copy-back is published but whose stick copy cannot be
  removed reports a changed tree (the view upserts the source) instead of
  `unchanged`. A Replace that fails after publishing removes the fresh copy
  first (the source is intact) and then restores what it displaced, so nothing
  ends up in recovery storage. A cancel during the copy phase reports as a
  cancel, as merge already does.
- **(11) A read-only source is refused before copying.** `statvfs` with
  `ST_RDONLY` on the source's filesystem, checked where the device boundary is
  detected.

### What the user sees wrong

- **(2) 10-bit AVIF and HEIC.** The libheif hook in `libheif-rs` 3.0 asks for
  `HdrRgb(a)Le` and copies samples that stay in 0..1023 into a 16-bit buffer,
  so `to_rgba8` makes them nearly black. Decode HEIF through `heif::decode_rgba`
  (8-bit RGBA, libheif converts) in both `preview/image.rs` and
  `preview/thumbnails.rs`, and drop the hook for HEIF.
- **(14) HEIF bounds.** The same path checks `w × h × 4` against
  `MAX_DECODE_BYTES` before decoding and wires libheif's cancel callback to the
  thumbnail's `cancelled` flag.
- **(6) Automounted shares read as local.** `remoteness::of` keeps the first
  of two mounts at the same point (`depth > deepest`), and `autofs` comes
  first. Use `>=`, so the last mount on a point wins, as it does in the kernel.
- **(7) Add to Network on MTP and friends.** Offer Add only when
  `Location::parse(&loc.to_uri())` gives the location back, and list in the
  Network section only the backend kinds `parse` knows.

### The uncommitted Trash change

Empty Trash failed for everything when one read-only NTFS partition had a
`.Trash-1000`. The working tree fixes that by showing only the home Trash.
Commit it as a stopgap, labelled as one, with a test for the new read-only
delete message (`delete.rs`). Sprint 33 replaces it with a Trash per root.
While here, `no_trash_reason` (`trash.rs:486`) keeps the errno, so a
read-only stick says "read-only" instead of offering Delete Permanently
(bug 13).

### Bookkeeping

- `docs/TODO.md`: the latest-sprint pointer, Sprint 31's two open checks
  carried over, and the "Before v0.1.0" list rewritten as below.
- The 0xx doc's status says which parts landed in 29 to 31 and that `Location`
  is Sprint 33.
- Sprint 29: annotate the Trash check at `029:167` (no longer true) and untick
  the partial one at `029:153`.
- CHANGELOG and README: Sprint 31's defaults (`thumbnails`, `folder_sizes`,
  and `thumbnail_limit_mb`, all `local-only`/50 MB by default, remote files not
  thumbnailed or previewed until the selection settles).
- Module maps in `AGENTS.md`, `src/app/mod.rs`, and `src/desktop/mod.rs`.
  `desktop::gvfs` and `desktop::volumes` become `pub(crate)`.
- `CLAUDE.md` names both bus tests that need an unsandboxed run.
- `{error:#}` wherever an anyhow error reaches the user (bug 19). It is a
  one-line change at each of six sites, and it makes every other failure in
  these three sprints readable.

### The tag

The v0.1.0 gate in [`release.md`](../release.md#v010-release-gate) gains:

- Hand checks for what landed since Sprint 26: a USB stick mounted, browsed,
  moved onto, undone, ejected; an SFTP share connected, browsed, and
  disconnected; a file copied in Nautilus and pasted in Marcel and the other
  way round; a HEIC and a 10-bit AVIF previewed.
- The Sprint 26 checks still open (marquee with the list header, copying
  `~/.ssh`) and one restart cycle of the release build for `state.conf`.
- A new screenshot, then `nix build`, `nix flake check`, a `ci.yml` dispatch,
  `scripts/check_version.sh v0.1.0`, and `git tag -s v0.1.0`.

## Where it came out differently

- **(1)** Verification did not move into the copier. The copier records what
  it read (`ObservedSource`: identity with ctime, length, mode, kind, in walk
  order), and `ensure_source_unchanged` walks the source again in the same
  order and compares. `verify_copied_tree` stays for the destination side.
  A source that changed does not keep both copies: the copy is of something
  that no longer exists, so it is withdrawn and the report says why. Removal
  is not keyed by the root's `ObjectKey`; the walk runs immediately before
  it, which leaves the window a rename into quarantine takes, the same one
  `mv` has.
- **(3)** The modes come back through `MoveRecord::source_modes`, filled only
  when the destination lost permissions, and applied deepest first after the
  copy back.
- **(5)** `MoveFailure { error, copy_kept }` is the typed error, and
  `TransferOutcome::kept_copies` is how a kept copy reaches the view.
- **(14)** libheif-rs 3.0 exposes no cancel callback, so a HEIF decode checks
  the flag before and after, not during. The size check happens before
  anything is decoded. The hook stays registered, because Properties reads a
  HEIF file's dimensions through `image`; pixels no longer go through it. A
  HEIF is recognised by its first bytes (`heif::is_heif`), not its name.
- **(7)** MTP phones, cameras, and Google Drive stay listed in the Network
  section, because they browse and disconnect fine. Only Add to Network is
  withheld, and the store refuses to save them whatever asks.
- **(13)** `trash_unavailable_for` returns a `NoTrash { reason, read_only }`,
  and the window reports a read-only drive instead of offering a permanent
  delete.
- `{error:#}` went to eleven sites, not six: every `Report::Error` built from
  an anyhow error.

## Not in scope

- `Location`, the mount table, and a Trash per drive: Sprint 33.
- The GVfs and UDisks robustness items (subscriptions, cancel, SMB port,
  reconnect, clipboard pipes): Sprint 34.
- Anything new.

## Acceptance checks

- [x] A regular file rewritten in place, same size, during a cross-device
      move: the source is kept with its new bytes, the copy withdrawn, and
      the report says the source changed. Same for a file replaced by an
      atomic save after the copier read it.
- [x] A tree with a link to a folder moved to FAT keeps its source and says
      why; copied, it drops the link with a note.
- [x] A tree with hardlinks copies to FAT, with one loss reported.
- [x] Source removal failing after the copy: the destination is reported and
      shown, and no removal is journalled.
- [x] Undo whose copy-back succeeds and whose stick copy cannot be removed
      reports the restored source, and the view shows it.
- [x] Undo of a move to FAT restores the modes the tree left with, and redo
      still validates afterwards.
- [x] Replace, then a failure after publication: the displaced item is back
      where it was and nothing is in recovery storage.
- [x] Cancel during the copy phase reports as cancelled.
- [x] A two-item cross-device move whose second item fails leaves the first
      moved and journalled and the second untouched.
- [x] A move from a read-only filesystem is refused before anything is
      copied (tested against `/nix/store`, which is a read-only mount here).
- [x] The 10-bit AVIF fixture previews and thumbnails red where it is red
      (both tests assert the pixel), and a HEIC named `.jpg` thumbnails.
- [x] A stacked `autofs` + `nfs4` mountinfo reads as remote.
- [x] An MTP location is not saveable, and the menu offers Add to Network
      only for saveable ones. (Seen in a unit test; no phone was plugged in.)
- [x] Permanent deletion on a read-only mount says "read-only filesystem";
      trashing there says the same and offers no permanent delete. (The
      message and the classification are tested; the dialog is not.)
- [x] TODO, the 0xx doc, Sprint 29, CHANGELOG, README, AGENTS.md, and
      CLAUDE.md match the tree.
- [x] The gate is green, with both bus tests run unsandboxed.
- [ ] The release gate passes and `v0.1.0` is tagged. Waiting on the hand
      checks; see TODO.
