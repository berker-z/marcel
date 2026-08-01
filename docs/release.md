# Marcel release and distribution plan

This document is the durable release handbook for Marcel. It records what is
packaged today, what must be hardened before the first public release, how a
release is identified, and how Marcel should reach users of NixOS, Arch,
CachyOS, Ubuntu, Mint, Fedora, and other Linux distributions.

Marcel is currently a working Linux-first pre-release alpha. The repository
flake is the only supported installation route. There is not yet a tagged
release, nixpkgs package, AppImage, AUR package, Debian package, RPM package, or
Flatpak.

The release contract is intentionally platform-neutral even though the current
artifact is Nix-only. Public installation documentation should present one
support matrix and consistent behavior guarantees, then mark each route
available only when its real artifact and maintenance process exist. Detailed
public-documentation work is tracked in
[Sprint 16](sprints/016-public-release-presentation.md).

## Branches, tags, and GitHub releases

`master` is the moving development branch. A Git tag is a permanent,
human-readable name attached to one exact commit:

```text
A──B──C──D──E  master
      ↑
    v0.1.0
```

As development continues, `master` moves past commit C while `v0.1.0` continues
to identify exactly the source used for version 0.1.0. A GitHub Release is the
download page, release notes, and compiled artifacts associated with that tag.

Marcel should not use a permanent `prod` branch. Release automation should run
from immutable `v*` tags. A long-lived `release/0.2` branch would only become
useful if Marcel eventually maintains an older release line while `master`
develops a newer one.

The intended release operation is:

```sh
git tag -s v0.1.0
git push origin v0.1.0
```

Before creating the tag, the release commit must contain the final version,
changelog, metadata, lock files, and packaging definitions. Existing release
tags and artifacts must never be silently replaced. A correction receives a
new version.

## Current Nix installation

Run the current `master` package without installing it:

```sh
nix run github:berker-z/marcel -- ~/Downloads
```

Install it into a user profile:

```sh
nix profile install github:berker-z/marcel
```

For reproducible personal use before the first tag, pin a full commit:

```sh
nix profile install \
  github:berker-z/marcel/FULL_COMMIT_HASH
```

Downstream flakes can consume Marcel's package and overlay as documented in
the README. Installing the ordinary package does not make Marcel the default
directory handler and does not take ownership of the generic
`org.freedesktop.FileManager1` D-Bus service. Both remain separate, explicit
system-integration choices.

## Current packaging audit

### Fonts

Marcel bundles its default UI font:

- private regular and semibold Iosevka 34.8.0 subsets use the collision-free
  `Marcel Iosevka` family name;
- cover Latin, Greek, Cyrillic, punctuation, currency, arrows, mathematical,
  box-drawing, and geometric ranges;
- rely on GPUI/cosmic-text system fallback for scripts outside that compact
  coverage;
- load the font bytes into GPUI without installing them in the user's font
  registry;
- use the same selected family for UI and monospace roles;
- allow an exact installed-family override through `MARCEL_FONT_FAMILY`.

The generated faces total approximately 564 KiB. The pinned, hash-verified
generator is `scripts/build_identity_assets.py`; the source bundle retains the
SIL OFL-1.1 license. Marcel deliberately does not bundle the Nerd Font build.

### File and Places icons

Marcel includes a private curated Nordzy 1.8.7 fallback rather than depending
on or copying the complete theme. Twenty scalable Places and MIME SVGs occupy
approximately 180 KiB without their license. Resolution order is:

1. the user's explicit `MARCEL_ICON_THEME`;
2. Marcel's private Nordzy semantic fallback;
3. the active GTK theme for icons absent from the curated Nordzy bundle;
4. the existing generic text glyph.

The private assets retain their GPL-3.0 license, source SVG form, provenance,
and upstream version. The Nix package installs them only below
`share/marcel/icons/nordzy`, never as a system-wide freedesktop theme. Layer
ordering has unit coverage; a clean installed-package visual smoke test remains
part of the release gate.

### Declarative package settings

The flake exports `homeManagerModules.default` and `nixosModules.default`.
Both install a configured Marcel wrapper through `programs.marcel.settings`;
the supported settings are initial palette, explicit icon theme, and UI font
family. The overlay's `pkgs.marcel.withSettings` constructor exposes the same
mechanism without a module. The configured application-specific D-Bus service
points at the wrapper so desktop activation receives the same settings as a
shell launch. Neither module changes MIME associations nor claims generic
FileManager1 ownership.

List/grid view and hidden-file visibility are interaction state rather than
declarative package configuration. Marcel loads them from
`$XDG_CONFIG_HOME/marcel/state.conf` (falling back to
`~/.config/marcel/state.conf`) and serially writes a tiny versioned replacement
file after each switch. A missing or invalid file recovers to grid view with
hidden files visible.

### Marcel application icon

Marcel ships an original three-pane application icon under the reverse-DNS name
`io.github.berker_z.Marcel`. The SVG source and 16, 24, 32, 48, 64, 128, 256,
and 512 pixel hicolor assets are installed by the Nix package and used by:

- `io.github.berker_z.Marcel.desktop`;
- X11 window metadata;
- future AppStream metadata;
- AppImage desktop integration;
- Debian, RPM, AUR, and nixpkgs packages.

The icon name should remain `io.github.berker_z.Marcel`.

### AppStream metadata

Marcel does not yet install
`io.github.berker_z.Marcel.metainfo.xml`. The first release needs valid
AppStream metadata containing:

- the application ID and launchable desktop ID;
- name, summary, and full description;
- MIT project license and metadata license;
- project, issue tracker, and source URLs;
- content rating;
- supported release/version entries;
- representative screenshots;
- appropriate developer identity.

The file should be validated with `appstreamcli` and the relevant package-format
linters.

### 7-Zip and RAR

The repository flake packages the free `_7zz` derivation and places `7zz`
privately under `libexec/marcel`. RAR and CBR extraction actions are disabled
by default because the RAR decoder contains code under the non-free unRAR
license. The ordinary flake and downstream overlay therefore require no
`allowUnfree` configuration.

The release policy should be:

- use free `_7zz` in the default Marcel package;
- keep archive creation and all free extraction formats working normally;
- keep RAR and CBR extraction unavailable by default;
- optionally expose a separately named or overridden RAR-capable package for
  users who explicitly allow the non-free dependency; until then, advanced
  users may supply a capable backend and set `MARCEL_ENABLE_RAR=1`;
- record the backend and licenses in third-party notices and package metadata.

The default package no longer depends on `_7zz-rar`, so it can follow the
ordinary free-package evaluation and build/cache path.

### Runtime programs and libraries

The Nix package currently supplies or wraps:

- GIO/GLib tools for default application opening;
- Poppler tools for PDF inspection and rasterization;
- Marcel's private `7zz` archive backend;
- Fontconfig and Freetype;
- Wayland and X11 client libraries;
- graphics, Vulkan/OpenGL, keyboard, audio, and GPUI runtime libraries;
- branded and optional generic D-Bus activation files;
- desktop metadata.

Portable and native packages must audit the same runtime contract rather than
assuming tools happen to exist on the host. The audit must cover PATH lookup,
dynamic libraries, D-Bus search paths, `XDG_DATA_DIRS`, portals, icon lookup,
thumbnail caches, Trash paths, and subprocess behavior.

### Pinned GPUI

Marcel consumes GPUI's upstream native external-drag API directly. The exact
Zed and gpui-component revisions are recorded in `Cargo.lock`; Marcel no longer
ships a locally modified GPUI tree. Release and downstream builds must fetch or
prefetch those immutable Git sources and retain their upstream license notices.

## Distribution targets

### Repository flake

This is implemented and is the current supported route. Before `v0.1.0`, make
the default package free, add application resources and metadata, and verify a
clean downstream installation from the tag.

### nixpkgs

The first nixpkgs submission should follow the tagged `v0.1.0` release rather
than package an arbitrary `master` snapshot.

The package should live under the appropriate `pkgs/by-name` path and:

- fetch the immutable GitHub tag with a fixed source hash;
- use `rustPlatform.buildRustPackage` and the committed Cargo lock;
- prefetch the Git dependencies pinned by `Cargo.lock`;
- declare every build and runtime dependency;
- use free `7zz` by default;
- install the desktop entry, branded icon, AppStream file, D-Bus service, and
  interfaces;
- declare a Marcel maintainer, license, homepage, source, main program, and
  Linux platforms;
- include a consumer-facing package test where practical.

Before opening the PR, run `nixfmt`, build the package, run its tests, and use
`nixpkgs-review`. The conventional initial-package commit and PR title is:

```text
marcel: init at 0.1.0
```

After review and merge, nixpkgs infrastructure can build it for supported
channels and serve substitutes from the normal binary cache.

### AppImage

AppImage should be Marcel's first broad, distribution-neutral artifact:

```text
Marcel-0.1.0-x86_64.AppImage
Marcel-0.1.0-aarch64.AppImage
```

Build a conventional AppDir containing the Marcel binary, private helpers,
required non-base libraries, icons, desktop file, AppStream metadata, and
licenses. Use `linuxdeploy`/`appimagetool` or another reproducible AppDir tool
and test the result across representative old and current distributions.

AppImage is suitable for download-and-run evaluation, but merely downloading
one does not reliably install its desktop entry or D-Bus activation files.
Marcel should provide an explicit, reversible per-user integration command that
installs the AppImage and metadata under `~/.local`. Becoming the default file
manager must remain a separate action.

The AppImage should be built on a sufficiently old supported glibc baseline,
avoid bundling host GPU drivers and other ABI-sensitive base components, and
include every non-library runtime resource that dependency scanners cannot
discover automatically.

### Arch Linux, CachyOS, and related distributions

Publish a stable source package named `marcel` in the Arch User Repository.
The AUR stores the `PKGBUILD` and related packaging files, not the built
application. It should download the signed release tag, verify its checksum,
build with Cargo, and install the complete desktop integration contract.

This makes the ordinary user experience:

```sh
yay -S marcel
```

or the equivalent operation with another AUR helper. CachyOS, EndeavourOS, and
other Arch-compatible distributions can consume the same package.

Optional `marcel-bin` or `marcel-git` packages can be considered later. The
tagged source package should remain canonical.

### Ubuntu, Mint, Debian, and related distributions

Initially attach architecture-specific `.deb` files to each GitHub Release:

```text
marcel_0.1.0_amd64.deb
marcel_0.1.0_arm64.deb
```

Users can install one directly:

```sh
sudo apt install ./marcel_0.1.0_amd64.deb
```

Linux Mint and other Ubuntu derivatives should be tested according to their
Ubuntu base release. A direct downloaded package does not provide automatic
updates.

Once Marcel has regular releases, publish source packages through a Launchpad
PPA. Launchpad builds, signs, and hosts an apt repository, but accepts source
packages rather than prebuilt release binaries. Its isolated build cannot
download arbitrary Cargo dependencies during compilation, so Marcel needs an
offline-capable vendored release source or a complete distribution dependency
strategy.

### Fedora, RPM distributions, and COPR

Initially attach `.rpm` files to GitHub Releases:

```text
marcel-0.1.0-1.x86_64.rpm
marcel-0.1.0-1.aarch64.rpm
```

Users can install them directly with their distribution package manager. Once
updates are regular, a Fedora COPR project can build source packages and expose
an installable third-party repository. The same offline-capable Cargo release
source used for PPA builds should support COPR.

openSUSE and other RPM distributions may require distribution-specific
dependency names or an Open Build Service recipe even when the release artifact
format is also RPM. Compatibility must be tested rather than assumed.

### Flatpak and Flathub

Flatpak is a separate compatibility project, not merely another output format.
A useful file manager requires broad host filesystem access and must preserve:

- native Trash behavior;
- opening and choosing host applications;
- external file drag-and-drop;
- D-Bus and single-instance activation;
- file-manager service integration;
- Poppler and archive subprocesses;
- icon-theme discovery;
- desktop portal behavior.

An independently hosted Flatpak repository or bundle is technically possible,
but should not be advertised until those semantics match the native package.

Flathub's current requirements treat broad-scope file managers as exceptional
because of sandbox limitations. Its current policy also disallows
AI-assisted application content and AI-generated submission work. Marcel is
therefore not currently eligible for a normal Flathub submission unless the
policy changes or Flathub explicitly grants an applicable exception. Recheck
the live requirements before doing any submission work.

## One installation contract

Packaging formats should not independently invent where Marcel's resources
live. Define one staged installation tree and convert it into the different
artifacts:

```text
usr/
├── bin/marcel
├── libexec/marcel/7zz
└── share/
    ├── applications/io.github.berker_z.Marcel.desktop
    ├── dbus-1/services/io.github.berker_z.Marcel.service
    ├── dbus-1/interfaces/org.freedesktop.FileManager1.xml
    ├── icons/hicolor/.../apps/io.github.berker_z.Marcel.*
    ├── metainfo/io.github.berker_z.Marcel.metainfo.xml
    └── licenses/...
```

The package may additionally expose the generic
`org.freedesktop.FileManager1.service`, but installing the ordinary application
must not take that generic name automatically.

The AppImage, Debian, and RPM jobs should begin from this same staged contract.
Nix and AUR build from source independently but install equivalent files.

## Release automation

The eventual `.github/workflows/release.yml` should trigger from pushed `v*`
tags, not from every `master` commit. The first implementation can be modest and
grow as artifact formats become ready.

For every release it should:

1. Check out the exact tag.
2. Verify that the tag, `Cargo.toml`, package expressions, AppStream release,
   and changelog versions agree.
3. Run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-targets
   ```

4. Run the release-only Nix package build and flake checks.
5. Build and smoke-test all declared architectures.
6. Produce the currently supported artifacts.
7. Generate SHA-256 checksums and a software bill of materials where practical.
8. Create a draft GitHub Release with release notes.
9. Upload artifacts without mutating an existing release.
10. Require deliberate approval before publishing the release.

Artifact jobs should use pinned actions and tools. Releases should eventually
be signed or attested, but a simple, understandable checksum and provenance
story is preferable to premature complicated signing infrastructure.

Package-repository updates should be separate jobs or follow-up workflows so a
failure in AUR, PPA, COPR, or nixpkgs maintenance cannot alter the already
published source tag.

## `v0.1.0` release gate

Before creating the first tag:

- [x] Make free `7zz` the default and define the optional RAR policy.
- [x] Add Marcel's application icon in required sizes.
- [ ] Add and validate AppStream metadata.
- [x] Bundle and verify the private curated Nordzy semantic fallback.
- [x] Bundle and verify Marcel's private regular/semibold Iosevka subsets and
      explicit installed-font override.
- [ ] Complete Sprint 16's public README, visual media, and platform support
      matrix.
- [ ] Audit runtime programs, libraries, metadata, and XDG/D-Bus paths.
- [ ] Add a changelog and write `0.1.0` release notes.
- [ ] Verify the version is consistent everywhere.
- [ ] Run all Rust quality checks.
- [ ] Run the release-only Nix build and flake check.
- [ ] Install and launch from a clean committed revision.
- [ ] Exercise a minimal-environment smoke test covering directory browsing,
      baseline icons/fonts, one PDF, one free archive, desktop metadata, and
      D-Bus activation.
- [ ] Validate `x86_64-linux` and `aarch64-linux`, either on native builders or
      trustworthy CI builders.
- [ ] Confirm installation does not silently become the default directory
      handler or own generic FileManager1 activation.
- [ ] Create and push the signed `v0.1.0` tag.
- [ ] Publish checksums and release notes with every artifact.

The first release does not need every planned package format. It is acceptable
for `v0.1.0` to ship the hardened Nix flake and an AppImage, then add AUR,
Debian, and RPM delivery incrementally. Every advertised artifact must,
however, satisfy the same safety and desktop-integration expectations.

## References

- [Nixpkgs manual and contribution guidance](https://nixos.org/manual/nixpkgs/stable/)
- [AppImage packaging guide](https://docs.appimage.org/packaging-guide/index.html)
- [Arch User Repository](https://wiki.archlinux.org/title/Arch_User_Repository)
- [Launchpad PPA documentation](https://documentation.ubuntu.com/launchpad/user/reference/packaging/ppas/ppa/)
- [Fedora COPR documentation](https://docs.pagure.org/copr.copr/)
- [Flatpak manifest documentation](https://docs.flatpak.org/en/latest/manifests.html)
- [Flathub requirements](https://docs.flathub.org/docs/for-app-authors/requirements)
