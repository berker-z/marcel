# Marcel backlog

What is left, in the order it is likely to happen. Sprint documents under
[`sprints/`](sprints/) turn items from here into bounded work with acceptance
checks; the latest is [Sprint 26](sprints/026-tag-readiness.md). Finished
work is recorded there and in [`../CHANGELOG.md`](../CHANGELOG.md), not here.

## Before `v0.1.0`

- Hand-run the open checks in
  [Sprint 26](sprints/026-tag-readiness.md#acceptance-checks): redo of a
  permission change, a root-owned file, a symbolic link in Properties, Sort
  by Kind and Reverse Order, filter and marquee with the list header, the
  `.marcel-` rename refusal, copying `~/.ssh`, Copy Path on two items.
- One clean restart cycle of the release build to confirm `state.conf` keeps
  view, sort, hidden files, and theme. Sprint 26 saw one unexplained reset
  during a hand run that did not reproduce.
- `nix build .#marcel-rs` and `nix flake check` on the release commit.
- Add `CONTRIBUTING.md` and `SECURITY.md`. Short; a reviewer clicks them
  first.
- `scripts/check_version.sh v0.1.0`, then `git tag -s v0.1.0`, push, and a
  GitHub release with the changelog entry. The rest of the gate is in
  [`release.md`](release.md#v010-release-gate).

## `0.1.x`

Small, and none of them blocks the tag.

- Route bookmark and state save failures through `surface::Report` instead
  of `eprintln!`, so a full disk or a bad permission is seen.
- The "no preview available" placeholder neither wraps nor elides in a narrow
  preview pane.
- Per-extent progress in `try_copy_sparse`; a sparse copy shows nothing until
  it is done.
- Stop reconciling selection per 512-entry batch during a load; the 50,000
  entry claim is true for browsing, not loading.
- `check_staged_limits` rescans the whole extraction tree every 200 ms; back
  off, or use `statvfs`.
- An unfree package variant with RAR decoding, instead of asking users to
  assemble the backend and set `MARCEL_ENABLE_RAR=1` themselves.
- Pin the dev-shell toolchain to an exact version instead of
  `stable.latest`, and document the separate update paths for crates, the
  Zed revision, and the toolchain.
- Cache the source device for the life of a drag so a cross-filesystem drop
  reads as refused rather than accepted-then-failed.
- One `IconProvider` per watcher instead of one per batch.

## `0.2.0`

- **Location.** `browsing_trash: bool` becomes a location: a local folder,
  the Trash, a volume, or search results. Everything below needs it.
- **Removable volumes** through UDisks2 in the sidebar, and cross-filesystem
  move as a verified copy plus a trash of the source, journalled as one
  operation. Together they are the "Downloads to a USB stick" workflow that
  Marcel refuses today.
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

First, before any of those: get the packaging out of Nix. The program is
portable already (no build script, no store paths, assets in the binary,
`7zz` found beside the exe or on `PATH`, Poppler on `PATH`), but the two
desktop entries exist only as `makeDesktopItem` calls in `nix/package.nix`,
the D-Bus and portal files live under `nix/` with `@marcel@` substitution,
and the install layout (`libexec/marcel/7zz`, `share/marcel/icons`, hicolor,
metainfo) is written down nowhere but `postInstall`. Move them to a
`packaging/` directory as plain files with a `Makefile` (`install PREFIX=
DESTDIR=`), have `nix/package.nix` call that same target so the two cannot
drift, add a `packaging/arch/PKGBUILD` (`cargo build --release`, `make
install`, `depends` on fontconfig, freetype2, wayland, libxkbcommon, libxcb,
poppler, 7zip), and a "Building without Nix" paragraph in the README with
the `pacman` line. About a day. The Home Manager switches have no Arch
equivalent as options: there, the package installs the service and portal
files unconditionally and the user's `portals.conf` and `xdg-mime` decide,
as with every other file manager.

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
- Desktop clipboard interoperability for files.
- Media previews, undecided. Measured on 2026-09-17: the whole Marcel
  closure is 224 MiB, Poppler cost about 20 MiB of it, and `ffmpeg-headless`
  would add about 300 MiB. Three tiers, in rising cost:
  - A poster frame plus duration, codec, resolution, and tags through
    `ffprobe`/`ffmpeg`, as one more loader modelled on `preview/pdf.rs`
    (subprocess, cancellable, cached by identity), feeding the existing
    image preview, the grid thumbnails, and Properties. About a day. The
    open question is only the tool: bundle it like Poppler and double the
    closure, or find it on `PATH` with a `settings.media` Nix switch that
    wraps it in, and say "needs ffmpeg" otherwise.
  - Audio playback with `symphonia` and `cpal`: pure Rust, a few MB of
    binary, alsa-lib in the closure. Two or three days, mostly the
    transport (play, pause, seek, stop when the preview is superseded).
  - Video playback: GPUI has no video element, so it is `ffmpeg-next` or
    `gstreamer-rs` decoding into a `RenderImage` per frame at 30 fps, plus
    audio sync. One to two weeks, a 250 to 300 MiB closure either way, and
    hot without VAAPI. Not worth it; a poster frame and Open in mpv is the
    file-manager answer.
- Ebook previews.
- Accessibility: there is no AT-SPI tree, so screen readers see nothing.
- Editable Places, Open in New Tab. Tabs themselves are not planned.
- Grouping, zoom, and per-folder view settings.
- PDF resize behaviour is working but visually imperfect.
