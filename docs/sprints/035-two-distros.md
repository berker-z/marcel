# Sprint 35: Two distros

**Status:** Built 2026-09-29; the `arch` CI job and one hand check are still
to run. Goes in before `v0.1.0`, so the tag can be installed on Arch as well
as from the flake.

## Goal

The Rust side was never tied to Nix. There are no store paths in it, the font
is compiled in, the icons are found beside the executable, and 7-Zip and
Poppler come from `PATH`. What made Marcel Nix-only was everything around the
binary:

- The two desktop entries existed only as `makeDesktopItem` calls in
  `nix/package.nix`.
- The install layout (hicolor icons, the Nordzy subset, metainfo, the D-Bus
  and portal files with `@marcel@` filled in, licenses) was written down
  nowhere but `postInstall`.
- The lld linker and the 32 MiB `RUST_MIN_STACK` that keep the thin-LTO
  release build alive were set only in the derivation. A plain
  `cargo build --release` would hit both failures again.
- The FileManager1 and file-chooser roles were switched on by environment
  variables that only the Nix wrappers set. Without a wrapper, the portal
  could start Marcel for a dialog and nothing would answer it.
- CI only ever built through Nix, so nothing noticed any of this.

After this sprint there is one install contract that both packages use, and
a CI job that builds it on Arch.

## Work

- [x] `packaging/` holds the data files as plain files: both desktop entries,
      the three D-Bus activation files, the portal file, the FileManager1
      interface XML, and the metainfo. `nix/` keeps only Nix expressions.
- [x] A top-level `Makefile` with `install` (`PREFIX`, `DESTDIR`, `BINARY`)
      and `install-data`. `nix/package.nix` calls `install-data`, so the two
      layouts cannot drift. `SEVENZIP=` links a private 7-Zip into
      `libexec/marcel`; `PORTAL=0` leaves the portal files to the Nix variant.
      The generic `FileManager1` activation file goes to `share/marcel`, never
      to `share/dbus-1/services`: Nautilus's Arch package owns that path, and
      taking the name has to stay opt-in anyway.
- [x] `.cargo/config.toml` carries lld and `RUST_MIN_STACK`. The derivation
      stops setting them itself. The dev shell gets lld.
- [x] `~/.config/marcel/desktop.conf` (`file_manager1=true`,
      `file_chooser=true`) turns the roles on, alongside the existing
      environment variables. Marcel never writes the file.
- [x] The archive tests look for 7-Zip the way Marcel does (`7zz`, then
      `7z`). Arch's `7zip` package ships `7z` only, and the tests would
      otherwise skip there.
- [x] `packaging/arch/PKGBUILD` for `marcel-rs`, building the tag's tarball.
- [x] A `ci.yml` job in an `archlinux` container that builds and tests the
      PKGBUILD against the checked-out tree, then validates what it
      installed.
- [x] README: installing on Arch, and the Hyprland (and so Omarchy) steps for
      the portal, show-in-folder, and the directory handler. `release.md`,
      `TODO.md`, and the changelog follow.

## Checks

- [x] Gate green at 489, both bus tests unsandboxed. The first two runs died
      on rustc segfaults (null reads in libLLVM and in the front end, different
      crates each time) during the full rebuild the new rustflags forced; the
      third went through. Same crash family as the one `RUST_MIN_STACK` was
      added for; the kernel log has libLLVM crashing on Jul 29, Aug 1, and
      Aug 20 too.
- [x] `nix build .#marcel-rs` installs the same tree as before, plus
      `share/marcel/org.freedesktop.FileManager1.service`; its tests and
      install check pass, and the thin-LTO link goes through on LLD from
      `.cargo/config.toml`. Both variants still point every activation file
      at their own wrapper.
- [ ] The Arch job passes on a `ci.yml` dispatch.
- [ ] By hand on the release build: a picker comes up through `desktop.conf`
      with the environment variables unset.
