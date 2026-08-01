# Sprint 12: Nix distribution and desktop launch

**Status:** Implemented — automated acceptance is complete and the package
installs and launches in the user's real NixOS environment. Default-handler
configuration remains intentionally deferred.

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
- The package installs nixpkgs' free `7zz` at the private discovery path
  `$out/libexec/marcel/7zz`; archive behavior does not depend on ambient PATH.
- `marcel.desktop` uses `Exec=marcel %U`, advertises `inode/directory`, and
  deliberately claims no archive MIME types. This sprint initially used the
  standard `system-file-manager` icon; the later pulled-forward Sprint 16
  identity slice replaced it with Marcel's branded icon.

## Acceptance checks

- [x] Parse local paths and percent-decoded `file://` URIs with unit coverage.
- [x] Preserve no-argument startup in the process working directory.
- [x] Export installable packages, runnable apps, a downstream overlay, and a
  package check from the flake.
- [x] Build only the Rust package sources rather than the repository or a
  generated `result` link.
- [x] Wrap every runtime dependency required for opening files and rendering
  PDF previews.
- [x] Install private free 7-Zip at Marcel's existing discovery path.
- [x] Install a freedesktop desktop entry for directories without claiming
  archives.
- [x] Document direct use, downstream flake wiring, and the Home Manager MIME
  association.
- [x] Install the package from the user's dotfiles and confirm Marcel launches.
- [ ] Manually confirm desktop launcher activation, directory and file-target
  opening, PDF preview, and archive operations through the installed package.
- [x] Add branded scalable and raster application icons. Delivered later as a
  deliberately pulled-forward Sprint 16 identity slice.
- [x] Add single-instance routing and `org.freedesktop.FileManager1`
  (delivered in Sprint 14).
- [ ] Publish versioned release artifacts and optional binary-cache output.
