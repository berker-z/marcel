# Sprint 12: Nix distribution and desktop launch

**Status:** Implemented — automated acceptance is complete; installing it as
the user's default directory handler remains a downstream configuration step.

## Goal

Turn the development flake into a directly installable Marcel package with the
runtime tools, desktop metadata, launch arguments, and downstream interface
needed for daily use on NixOS.

This sprint does not add a branded icon, non-Nix artifacts, a binary cache,
single-instance routing, or `org.freedesktop.FileManager1`.

## Product and package contract

- `marcel [PATH|file://URI]` opens the first local target. Relative paths are
  resolved from the process working directory. A file target opens its parent
  through Marcel's existing startup normalization.
- `packages.<system>.default`, `packages.<system>.marcel`,
  `apps.<system>.default`, and `apps.<system>.marcel` are provided for x86-64
  and AArch64 Linux.
- `overlays.default` exposes `pkgs.marcel` to downstream flakes.
- The package wrapper supplies GIO and Poppler tools plus GPUI's runtime
  libraries.
- The package installs nixpkgs' RAR-capable `7zz` at the private discovery path
  `$out/libexec/marcel/7zz`; archive behavior does not depend on ambient PATH.
- `marcel.desktop` uses `Exec=marcel %U`, advertises `inode/directory`, and
  deliberately claims no archive MIME types. Until Marcel has branded artwork,
  it uses the standard `system-file-manager` icon.

## Acceptance checks

- [x] Parse local paths and percent-decoded `file://` URIs with unit coverage.
- [x] Preserve no-argument startup in the process working directory.
- [x] Export installable packages, runnable apps, a downstream overlay, and a
  package check from the flake.
- [x] Build only the Rust package sources rather than the repository or a
  generated `result` link.
- [x] Wrap every runtime dependency required for opening files and rendering
  PDF previews.
- [x] Install private RAR-capable 7-Zip at Marcel's existing discovery path.
- [x] Install a freedesktop desktop entry for directories without claiming
  archives.
- [x] Document direct use, downstream flake wiring, and the Home Manager MIME
  association.
- [ ] Install the package from the user's dotfiles and manually confirm
  launcher, directory double-click, file-target opening, PDF preview, and
  archive operations.
- [ ] Add branded scalable and raster application icons.
- [ ] Add single-instance routing and `org.freedesktop.FileManager1`.
- [ ] Publish versioned release artifacts and optional binary-cache output.
