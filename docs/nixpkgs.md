# Marcel in nixpkgs

This document records Marcel's path into nixpkgs, the expected review and
release workflow, and the maintenance work that begins after acceptance. It
reflects the repository and nixpkgs process as checked on 2026-08-19.

## Current assessment

Marcel is technically close to being packageable. The repository already has
a reproducible Nix package with a free closure, desktop and D-Bus integration,
bundled visual assets with their licenses, and a substantial automated test
suite. On 2026-08-19, a clean `nix build .#marcel --no-link` against the locked
nixpkgs completed successfully on `x86_64-linux`; 253 tests passed.

The main remaining work is release readiness rather than inventing a package
from scratch. nixpkgs asks whether a new package is ready for general use, has
a clear license, is likely to be used by other people, is actively maintained,
and has someone willing to maintain it for at least a complete nixpkgs release
cycle:

- <https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md#quick-start-to-adding-a-package>

Marcel currently describes itself as an alpha with little external use. For a
file manager that mutates user data, completing the outstanding graphical and
destructive-operation acceptance matrix is more important than adding another
feature before submission. A small amount of real external testing would also
make the general-use case easier for a reviewer to accept.

## Naming and the existing `marcel` package

`pkgs.marcel` is already the attribute for an unrelated Python-based shell:

- <https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/ma/marcel/package.nix>

The Marcel file manager therefore cannot use that attribute. The intended
nixpkgs attribute is `marcel-rs`, placed at:

```text
pkgs/by-name/ma/marcel-rs/package.nix
```

The PR and commit title would consequently be:

```text
marcel-rs: init at 0.1.0
```

There is a second, separate collision: the existing shell installs
`bin/marcel`, and the file manager currently installs the same path. Nix store
outputs can coexist, but adding both packages to one profile or NixOS system
environment produces a file collision unless the user assigns priorities.
Nix supports priorities for resolving such collisions, but silently choosing
one of two unrelated programs is a poor default interface:

- <https://nixos.org/manual/nixpkgs/stable/#var-meta-priority>
- <https://github.com/NixOS/nix/blob/master/src/nix/profile.cc>

**Decided and implemented on 2026-08-21.** The collision-free contract below is
what the repository now does, before any release history exists to break. The
installed command is `marcel-rs`, `pname` and `meta.mainProgram` are
`marcel-rs`, the desktop entries use `Exec=marcel-rs %U`, and both D-Bus
service files point at `bin/marcel-rs`.

The flake attribute and the overlay attribute moved with it, which turned out
to matter more than expected: `overlays.default` previously bound `marcel`,
so applying Marcel's overlay silently *replaced* nixpkgs' Python `marcel` for
the whole system rather than adding a package beside it. Anyone following the
README's overlay instructions would have lost the shell without being told.
Both the overlay and `packages.<system>` now expose `marcel-rs`, and
`packages.default` still resolves to the same derivation so
`nix run github:berker-z/marcel` is unchanged.

The visible application name, the `io.github.berker_z.Marcel` application ID,
the icon name, the D-Bus identity, and the `marcel` configuration directory
are all unchanged. Only the command carries the suffix.

The collision-free release contract is therefore:

- use `marcel-rs` as the nixpkgs attribute and package name;
- make `bin/marcel-rs` the installed command across supported distribution
  routes, including the repository flake and nixpkgs;
- set `meta.mainProgram = "marcel-rs"`;
- use `Exec=marcel-rs %U` in installed desktop entries;
- point installed D-Bus service files at the store path to
  `bin/marcel-rs`;
- keep the visible application name, reverse-DNS application ID, icon name,
  configuration directory, and D-Bus identity as Marcel.

Doing this consistently before `v0.1.0` avoids establishing two commands and
removes the collision completely. It requires updating the flake app, desktop
entries, D-Bus service substitutions, configured wrappers, documentation, and
package tests together. The GUI name remains simply Marcel.

If preserving `marcel` as the command is preferred, nixpkgs can still carry
the package, but the inability to install it alongside the existing shell must
be accepted and documented. Shipping both `marcel-rs` and a `marcel` symlink
would reintroduce the same collision. `meta.priority` should be treated as a
user escape hatch, not Marcel's packaging solution.

The final name should be mentioned in the PR description because nixpkgs
normally prefers an attribute and `pname` matching the upstream package name.
`marcel-file-manager` is a more descriptive fallback if reviewers object to
the language suffix, but it does not by itself solve the executable collision.

## Submission gates

### Product and release gates

- Complete the remaining graphical acceptance matrix, especially mounted
  Trash, destructive operations, conflict handling, and multi-window operation
  ownership.
- Fix any correctness or recovery defects that the matrix exposes.
- Add mandatory hosted checks for `cargo fmt --check`, Clippy with warnings
  denied, all-target tests, and the Nix package build.
- Add and validate AppStream metadata and validate the desktop files. AppStream
  is not an absolute nixpkgs requirement, but it is expected release polish for
  a graphical application.
- Test the installed package in a clean desktop environment, including baseline
  icons and fonts, PDF preview, a free archive round trip, desktop launch, and
  D-Bus activation.
- Obtain some external testing beyond the author's daily use.
- Publish an immutable `v0.1.0` tag and GitHub release with release notes.
- Build the tagged source on `x86_64-linux` and `aarch64-linux`.

New File, Properties, X11 outbound drag, Flatpak, AppImage, media playback, a
hero GIF, and an upstream binary cache are not prerequisites for nixpkgs. They
may be useful release or product work, but should not unnecessarily hold the
initial package.

### Package-expression gates

The repository expression in `nix/package.nix` is the starting point, but the
nixpkgs expression must be self-contained and follow nixpkgs conventions:

- fetch `v0.1.0` with `fetchFromGitHub` and a fixed source hash instead of using
  the local repository tree;
- build with `rustPlatform.buildRustPackage` and the committed `Cargo.lock`;
- retain and update the fixed hashes required by Git dependencies;
- document the unusual `RUST_MIN_STACK` and LLD choices;
- add the Marcel maintainer to nixpkgs and set `meta.maintainers`;
- set `meta.homepage`, `meta.changelog`, `meta.mainProgram`, and Linux
  platforms;
- install Marcel's `LICENSE`, `THIRD_PARTY_NOTICES.md`, and the bundled-asset
  license texts;
- add an install check or `passthru.tests` package test;
- check whether the broad `LD_LIBRARY_PATH` wrapper can be replaced by focused
  RPATHs, while preserving GPU-driver discovery;
- add `passthru.updateScript = nix-update-script { };` for routine updates.

The current `meta.license = lib.licenses.mit` is incomplete because different
parts of the installed package have different licenses. It should be expressed
approximately as:

```nix
license = with lib.licenses; [
  mit
  gpl3Only
  ofl
];
```

This covers Marcel's MIT code, the GPL-3.0-only Nordzy icon subset, and the SIL
Open Font License Iosevka subset. nixpkgs explicitly uses a license list when
parts of one package have different licenses:

- <https://nixos.org/manual/nixpkgs/unstable/#sec-meta-license>

### Final package checks

Run the following from a nixpkgs checkout before marking the PR ready:

```sh
nixfmt pkgs/by-name/ma/marcel-rs/package.nix
./ci/nixpkgs-vet.sh master
nix-build -A marcel-rs
nix-shell -p nixpkgs-review --run "nixpkgs-review rev HEAD"
```

Launch and exercise the resulting executable as well. The nixpkgs new-package
review checklist expects the package to build locally and every shipped binary
to be tested:

- <https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md#new-packages>
- <https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/README.md>

## Expected submission time

If the graphical acceptance matrix finds no serious defect, reaching a strong
submission should take roughly:

- three to seven focused days for release hardening, CI, metadata, clean-system
  testing, and the tag;
- half a day to two days to adapt and validate the nixpkgs expression;
- one to eight weeks of calendar time for nixpkgs review and merge, with two to
  six weeks a reasonable planning estimate.

Comparable historical package additions varied widely:

- Yazi was submitted and merged on the same day:
  <https://github.com/NixOS/nixpkgs/pull/250697>
- Spacedrive took about six days:
  <https://github.com/NixOS/nixpkgs/pull/270121>
- COSMIC Files took about eight days:
  <https://github.com/NixOS/nixpkgs/pull/278745>
- Zed took roughly ten weeks and involved substantially more packaging work:
  <https://github.com/NixOS/nixpkgs/pull/284010>

The expression itself is not expected to be the difficult part. Reviewer
availability and demonstrating that a very young project is suitable for
general use are the less predictable parts.

## Updates after acceptance

Updates are not automatically accepted. They can be automatically detected and
proposed, after which they still require successful CI and an explicit merge.

The normal release flow is:

```text
upstream v0.1.1 tag and GitHub release
    -> nixpkgs update bot detects the version
    -> bot opens `marcel-rs: 0.1.0 -> 0.1.1`
    -> source and Cargo dependency hashes are updated
    -> nixpkgs CI builds and tests the package
    -> maintainer reviews the release and CI result
    -> committer or eligible merge bot merges the PR
    -> nixpkgs unstable channels advance to the merged revision
```

The community `r-ryantm` bot periodically attempts to update packages. An
explicit `nix-update-script` update procedure makes tag detection and hash
updates more reliable:

- <https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md#automatic-package-updates>

Marcel's pinned GPUI Git dependencies make it somewhat more likely than a
crates.io-only application that an automatic update will need help. If a
release changes those revisions, the associated Cargo output hashes change as
well. `nix-update` may update them successfully; otherwise the maintainer must
repair the bot PR manually.

For a bot-authored PR under `pkgs/by-name`, a listed package maintainer can use
the streamlined merge path after reviewing it:

```text
@NixOS/nixpkgs-merge-bot merge
```

The bot checks eligibility and CI rather than treating the new version as
implicitly trusted:

- <https://github.com/NixOS/nixpkgs/blob/master/maintainers/README.md#nixpkgs-merge-bot>
- <https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md#how-to-merge-pull-requests-yourself>

Routine releases may therefore require only a short review and a merge command.
Releases that add dependencies, alter integration, change licenses, or stop
building require a normal manual package update.

After a merge to nixpkgs `master`, the package reaches `nixpkgs-unstable` and
`nixos-unstable` when their channels next advance, generally after a lag of a
few days. Stable NixOS releases retain their existing version. Security or
important correctness fixes can be deliberately backported, while ordinary
feature releases normally wait for the next stable NixOS release:

- <https://nixos.org/manual/nixpkgs/unstable/#chap-packageconfig>
- <https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md#how-to-backport-pull-requests>

## Ongoing maintainer responsibility

Acceptance transfers the package recipe into nixpkgs, but it does not transfer
upstream responsibility. Marcel's listed maintainer should continue to:

- review automated version updates and upstream release notes;
- repair source, Cargo, and Git dependency hashes when automation cannot;
- respond when Rust, LLVM, GPUI, or nixpkgs library updates break the build;
- keep runtime dependencies, licenses, platforms, and metadata accurate;
- test packaging changes rather than using Hydra as the first test environment;
- backport urgent data-integrity or security fixes when appropriate;
- find a replacement maintainer if they can no longer maintain the package.

A healthy patch release can require only minutes of maintainer attention plus
CI and merge time. A toolchain or GPUI transition can require a real packaging
change and ordinary review.
