# Marcel session handoff

**Prepared:** 2026-07-29
**Branch:** `master`
**Workspace:** `/home/berkerz/Projects/marcel`

Read `AGENTS.md` first and preserve the working tree.

## Repository state

The current remote tip is:

```text
a064ff4 feat: add archive operations and live themes
```

That commit includes the archive slice, terminal action, menu normalization,
visual hierarchy pass, larger grid typography/icons, the live theme selector,
and the visible Settings glyph fix.

The current uncommitted slice turns the flake into an installable Nix package
for daily use. Its files are:

```text
 M README.md
 M docs/HANDOFF.md
 M docs/TODO.md
 M docs/sprints/010-archive-operations.md
 M docs/sprints/011-visual-hierarchy-polish.md
 M flake.nix
 M src/app.rs
 M src/delete_ops.rs
 M src/lib.rs
 M src/main.rs
 M src/theme.rs
?? docs/sprints/012-nix-distribution-and-desktop-launch.md
?? nix/package.nix
?? src/launch.rs
```

Do not discard these changes.

## Nix distribution slice

The flake now exports, for x86-64 and AArch64 Linux:

- `packages.<system>.marcel` and `packages.<system>.default`;
- `apps.<system>.marcel` and `apps.<system>.default`;
- `overlays.default`, exposing `pkgs.marcel`;
- a package build under `checks`.

`nix/package.nix` uses `buildRustPackage` with a Cargo-only source fileset. The
installed wrapper supplies GPUI runtime libraries, GIO, and Poppler tools. It
also installs nixpkgs' narrowly allowlisted RAR-capable 7-Zip executable at:

```text
$out/libexec/marcel/7zz
```

This is the private path already supported by Marcel's archive backend, so
installed archive operations do not depend on ambient PATH.

The package installs `marcel.desktop` with:

```text
Exec=marcel %U
MimeType=inode/directory;
Icon=system-file-manager
```

It deliberately claims no archive MIME types. ZIP, 7z, RAR, and tar
double-click behavior therefore remains assigned to the user's archive viewer.
A branded Marcel icon remains release work.

## Desktop launch arguments

`src/launch.rs` accepts the first local path or percent-decoded `file://` URI.
Relative paths resolve against the process working directory. Unsupported
non-local URIs are skipped. With no local argument Marcel preserves its old
behavior and opens the working directory. Existing startup normalization opens
the parent when the target is a file.

The parser has unit coverage for no arguments, relative paths, file URIs, and
skipping unsupported URI schemes.

## Context-menu hover fix

The bookmark, entry, and empty-space custom menus used `colors.accent` for
hover. Marcel maps both that token and the popover to the raised surface, so
the hover was invisible. Enabled rows now use `colors.list_active`, the
theme-aware translucent primary tint. Ordinary browser list/grid hover remains
on `colors.list_hover`.

## Verification

The required Rust quality gate passes:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
128 passed; 0 failed
```

The real x86-64 Nix package and its in-package test phase build successfully.
`nix flake check` passes, the wrapped native binary has no missing libraries,
the generated desktop entry claims only `inode/directory`, and private
`libexec/marcel/7zz` runs as 7-Zip 26.02. The complete Nix closure is about
207.9 MiB.

Nix package tests create a private freedesktop Trash under their sterile test
home. This is required because the `trash` crate refuses its safety-root lookup
when no Trash structure exists; deletion policy itself is unchanged.

## Downstream dotfiles contract

The README has the complete example. The expected wiring is:

```nix
inputs.marcel = {
  url = "github:berker-z/marcel";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Then apply:

```nix
nixpkgs.overlays = [inputs.marcel.overlays.default];
environment.systemPackages = [pkgs.marcel];
```

Home Manager can make it the default directory handler with:

```nix
xdg.mimeApps = {
  enable = true;
  associations.added."inode/directory" = ["marcel.desktop"];
  defaultApplications."inode/directory" = ["marcel.desktop"];
};
```

Do not add archive MIME associations.

## Remaining desktop/release work

- Install the package through the user's dotfiles and manually confirm
  launcher startup, directory activation, PDF preview, and archive operations.
- Add a branded scalable icon and raster fallbacks.
- Add `org.freedesktop.FileManager1` and decide single-instance request
  routing.
- Add versioned release automation, non-Nix artifacts, and optionally a binary
  cache.

The bounded contract and acceptance checks are in
[`Sprint 12`](sprints/012-nix-distribution-and-desktop-launch.md).

## Contributor constraints

- Use gpui-component by default unless a concrete limitation is documented.
- Keep filesystem enumeration, decoding, archive work, and subprocess I/O off
  GPUI's foreground executor.
- Make long-running work cancellable or safely superseded and memory-bounded.
- Preserve exact upstream provenance in comments and
  `THIRD_PARTY_NOTICES.md`.
- Do not copy Zed GPL application code.
- Use `apply_patch` for manual edits.
- Before completion, run formatting, strict Clippy, all tests, and the Nix
  package check.
