# Marcel backlog

What is left, in the order it is likely to happen. Sprint documents under
[`sprints/`](sprints/) turn items from here into bounded work with acceptance
checks; the latest finished one is
[Sprint 34](sprints/034-network-and-devices-hardening.md), and
[Sprint 35](sprints/035-two-distros.md) is under way.
Finished work is recorded there and in [`../CHANGELOG.md`](../CHANGELOG.md),
not here.

## Three cleanup sprints, done

They came out of [`review-2026-09-27.md`](review-2026-09-27.md) and closed on
2026-09-27; the checks they could not run without a stick, a share, or
Nautilus are in the next section.

1. [Sprint 32: Safe to tag](sprints/032-safe-to-tag.md). Data-safety fixes in
   the cross-device move, the HEIF and remoteness bugs, bookkeeping, then
   the v0.1.0 tag on what master holds by then.
2. [Sprint 33: Location](sprints/033-location.md). Part one of the 0.2 plan,
   done late: one `Location`, one mount table, a Trash per root.
3. [Sprint 34: Network and devices hardening](sprints/034-network-and-devices-hardening.md).
   The GVfs, UDisks2, and clipboard long tail.

Search is next, on top of `Location`.

## Before `v0.1.0`

Everything here needs a person at the machine. The plan was to tag after
Sprint 32; the hand checks had not been run when Sprints 33 and 34 were
finished the same day, so the tag goes on master as Sprint 34 left it.

Driven on 2026-09-27 and passed: the clipboard both ways with Nautilus, an
SFTP share connected, browsed, and disconnected across a `gvfs-daemon`
restart, a blackholed connect cancelled and retried, a read-only drive
Trash, Back and Forward through the Trash, `ShowFolders`, a `state.conf`
restart cycle, and every open Sprint 26 check except the marquee. The
sprint docs carry the details.

That run found two bugs, both fixed the same day and checked in the app:
Enter in the rename field also reached the listing's `enter` binding and
opened the file (gpui-component's single-line input propagates Enter), and on
a share a thumbnail-cache miss was recorded as a failure, so every image
carried the red "!" badge.

Still open, and needing hardware or hands:

- A USB stick: moved onto and ejected, pulled while being browsed, a file
  trashed on it found in its Trash after Undo is gone, restored, and that
  Trash emptied alone, and ejecting it while two windows browse it.
- The Windows partition with the fstab line (Sprint 29). It is mounted
  read-only now, which is Fast Startup.
- A HEIC from a phone and a 10-bit AVIF, previewed and thumbnailed.
- Marquee selection with the list header (the harness cannot drag).
- The preview pane reading only where the selection stops, on a share.
- The run above used the dev build. `release.md` asks for the release
  build, so it needs one short pass: launch, a restart for `state.conf`,
  and a share.
- `nix build .#marcel-rs` and `nix flake check` on the release commit, and a
  `ci.yml` dispatch on it so both architectures have built the tree before
  the immutable tag exists.
- A fresh screenshot for the README and the AppStream file. The current one
  is from 2026-08-18, before sorting, the Trash view, audio, video, drives,
  and network shares.
- The Arch package ([Sprint 35](sprints/035-two-distros.md)): the `arch` job
  green on the same `ci.yml` dispatch, and on the release build, a file
  dialog answered through `desktop.conf` with the `MARCEL_CLAIM_*` variables
  unset.
- `scripts/check_version.sh v0.1.0`, then `git tag -s v0.1.0`, push, and a
  GitHub release with the changelog entry. The rest of the gate is in
  [`release.md`](release.md#v010-release-gate).
- The AUR, once the tag is pushed. `marcel-rs` was free on 2026-09-29
  (`marcel` is an unrelated shell).
  1. The checksum of the tag's tarball goes into `sha256sums` in
     `packaging/arch/PKGBUILD`, replacing `SKIP`:
     `curl -sL https://github.com/berker-z/marcel/archive/refs/tags/v0.1.0.tar.gz | sha256sum`.
     Commit it to master. `packaging/arch` is outside the Nix source, so
     this does not rebuild the Nix package.
  2. Dispatch `ci.yml` on that commit. The `arch` job builds against the
     real tarball checksum and uploads the `.SRCINFO` as the `srcinfo`
     artifact (`gh run download <run> -n srcinfo`). The one the tag's own
     run uploads still says `SKIP`.
  3. An AUR account with an SSH key, then
     `git clone ssh://aur@aur.archlinux.org/marcel-rs.git`, copy in the
     PKGBUILD and the `.SRCINFO`, commit, push.
  4. Point the README's Arch section at `yay -S marcel-rs` instead of the
     clone-and-makepkg steps.

## `0.1.x`

Small, and none of them blocks the tag.

- The "no preview available" placeholder neither wraps nor elides in a narrow
  preview pane.
- Per-extent progress in `try_copy_sparse`; a sparse copy shows nothing until
  it is done.
- `check_staged_limits` rescans the whole extraction tree every 200 ms; back
  off, or use `statvfs`.
- An unfree package variant with RAR decoding, instead of asking users to
  assemble the backend and set `MARCEL_ENABLE_RAR=1` themselves.
- Pin the dev-shell toolchain to an exact version instead of
  `stable.latest`, and document the separate update paths for crates, the
  Zed revision, and the toolchain.
- Cache the source device for the life of a drag so a cross-filesystem drop
  reads as refused rather than accepted-then-failed.
- Make the media pane its own GPUI entity. While audio plays it repaints at
  20 fps through `cx.notify()` on the window view, which re-renders the
  sidebar and the listing too; a view of its own would repaint only the
  bars and the clock.
- An Opus decoder for symphonia, when one exists, so voice notes stop
  needing ffmpeg.

## `0.2.0`

- **Location**: done in Sprint 33 ([`sprints/033-location.md`](sprints/033-location.md)),
  with a Trash per drive. Search is the variant still to add; the line of
  work is in
  [`sprints/0xx-every-place-a-file-lives.md`](sprints/0xx-every-place-a-file-lives.md).
- **Removable volumes** through UDisks2 in the sidebar, and cross-filesystem
  move as a verified copy plus removal of the source, journalled as one
  operation: done in Sprint 29 ([`sprints/029-drives.md`](sprints/029-drives.md)).
  Left from it: a Windows partition that needs a password (an fstab line
  with `x-gvfs-show` is the answer for now) and polling for FUSE mounts; the
  "delete immediately?" fallback for a filesystem with no Trash landed in
  Sprint 30.
- **Network shares** through GVfs, with a Network section and a saved
  `servers` file: done in Sprint 30 ([`sprints/030-network.md`](sprints/030-network.md)).
  Left from it: polling for FUSE mounts (shared with `ntfs-3g` above),
  browsing `smb://` and `network://`, and a password prompt seen live.
- **Search.** Recursive find by name, riding `stream_directory`'s ticketed
  cancellation, shown as a location.
- Create Link, the last greyed context-menu item.
- Merge while moving. Move To and cut-and-paste refuse to fold a folder into
  an existing one; copy does it.
- Editable owner and group are not planned: `chown` needs privileges Marcel
  does not have.

## Distribution

In the order set out in [`release.md`](release.md#distribution-targets):
a source tarball on the GitHub release, an AUR package, then the nixpkgs
submission ([`nixpkgs.md`](nixpkgs.md) has the recipe; it still needs a
maintainer entry and `passthru.updateScript`). AppImage and Flatpak wait
for real users; Flatpak also costs the portal backend and D-Bus activation,
and Flathub's policy currently blocks the submission regardless.

Getting the packaging out of Nix was [Sprint 35](sprints/035-two-distros.md):
the data files are plain files under `packaging/`, the `Makefile` is the
install layout both packages use, and `packaging/arch/PKGBUILD` is what goes
to the AUR once it has a real checksum.

Still to do for the package itself: a clean-environment smoke test that
launches Marcel, lists a fixture folder, renders one PDF, extracts one
archive, and checks the installed metadata; and a written audit of the
runtime closure (Poppler, 7-Zip, fontconfig, the graphics and Wayland
libraries, portals, icon and thumbnail paths).

## Hardening, later

- `openat`/dirfd discipline through the copy and delete walkers; both are
  path-based throughout.
- Landlock confinement for `7zz` and `pdftoppm`.
- A journal-wide undo snapshot budget; each transfer has one, the journal
  does not.
- A setuid/setgid/sticky policy for copies, written down in
  [`copy-semantics.md`](copy-semantics.md). Marcel preserves them where
  `cp` does not.
- One process-level coalescing writer for `state.conf`. Each window writes
  the whole file; last writer wins, which is fine for view state and would
  not be for anything more.
- Extract the larger views out of `app/` further; `render_entry_menu` first.

## Parked

- X11 outbound drag. Wayland is the tested target.
- The file clipboard on GNOME and X11. Mutter offers no data-control
  protocol, and a second connection cannot use `wl_data_device` without an
  input serial from its own surface, so it would have to go through GPUI:
  let a `ClipboardItem` carry extra MIME types and have `send` answer each
  with its own bytes (`gpui_linux` `wayland/clipboard.rs` and `client.rs`,
  plus the X11 clipboard). Upstream it or pin a fork. Also parked: pasting
  `sftp://` URIs another program copied, which needs the GVfs URI mapped to
  its FUSE path.
- Bundle ffmpeg, or keep finding it on `PATH`. Decided on 2026-09-18 for
  `PATH` plus the `settings.media` Nix switch: ffmpeg-headless is a 300 MiB
  closure against Marcel's 224, and only video posters, video thumbnails,
  and Opus audio need it. Revisit if "video shows an icon" becomes the
  first thing new users report.
- Video playback in the pane. GPUI has no video element, so it would be
  ffmpeg piping raw frames into a `RenderImage` at 30 fps plus audio sync:
  three to five days, hot without VAAPI, and the poster with a play button
  that hands the file to mpv covers what a file manager needs.
- Ebook previews.
- Accessibility: there is no AT-SPI tree, so screen readers see nothing.
- Editable Places, Open in New Tab. Tabs themselves are not planned.
- Grouping, zoom, and per-folder view settings.
- PDF resize behaviour is working but visually imperfect.
