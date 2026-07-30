# Sprint 10: archive operations

**Status:** Active — backend, safe publication, ZIP creation, extraction,
context actions, progress/cancellation, incremental results, and Undo/Redo are
implemented. Portable release packaging and manual UI acceptance remain.

## Goal

Add conventional archive creation and extraction without assuming that a
desktop archive manager or format-specific command happens to be installed.
Archive work uses Marcel's shared background-operation, cancellation, progress,
incremental-directory-update, and Undo foundations.

This sprint does not add archive-as-folder navigation, an extraction
destination chooser, password entry, encrypted archive creation, or interactive
conflict decisions.

## Product contract

- `Compress…` accepts files, directories, and a preserved multi-selection.
- The first creation format is ZIP. The naming dialog proposes `<name>.zip` for
  a single item and `Archive.zip` for multiple items.
- An occupied archive destination is refused; Marcel never overwrites or
  silently chooses another name.
- `Extract` is available for exactly one supported archive in an ordinary
  filesystem location.
- Extraction always publishes beside the archive. There is no `Extract To…`
  action or destination dialog.
- Double-click and `Open` retain normal MIME-default activation. Extraction is
  an explicit write action.
- Archive content preview is a later, read-only preview-provider slice.
- Password-protected input and encrypted output report a clear unsupported
  error in the first slice.

## Backend and distribution contract

Marcel owns an `ArchiveBackend` boundary and initially implements it with
official 7-Zip's standalone console program. Runtime discovery is:

1. `MARCEL_7ZZ`, for explicit development and test overrides;
2. Marcel's private `libexec/marcel/7zz`;
3. `7zz` on `PATH`;
4. legacy `7z` on `PATH`.

Distribution artifacts provide the backend instead of making the user's
ambient `PATH` the product contract:

- Nix uses nixpkgs' free `_7zz` variant. RAR and CBR actions stay unavailable
  unless the user explicitly supplies a capable non-free backend and enables
  `MARCEL_ENABLE_RAR`. Debian, RPM, and Arch packages should follow the same
  free-by-default contract;
- Flatpak builds 7-Zip as a module;
- AppImage and portable tarball artifacts bundle only the architecture-specific
  static `7zzs` executable as private `libexec/marcel/7zz`, together with all
  required notices.

At 7-Zip 26.02, the x86-64 static executable is approximately 3.59 MiB
installed and 1.29 MiB under `xz -9`. It has no idle memory or startup cost
because Marcel starts it only for archive work.

Marcel, rather than 7-Zip, owns scheduling, cancellation, bounded diagnostics,
staging, validation, conflict policy, publication, progress, operation results,
and Undo/Redo.

## Extraction contract

The extraction flow conceptually follows Yazi's proven staging and tidy model,
audited at commit `319f90e0eab185a231eef5562215ba322e320286`
(`yazi-plugin/preset/plugins/extract.lua` and `archive.lua`). Marcel adds a
stricter no-overwrite and validation policy; no Yazi code is copied.

1. List and preflight the archive before extraction.
2. Reject absolute paths, parent traversal, invalid path components, symlinks,
   and other entries Marcel cannot publish safely.
3. Bound entry count, declared expanded size, actual staged size, subprocess
   output, and nested-container handling.
4. Extract into a private hidden staging directory beside the archive.
5. Revalidate the complete staged tree without following symlinks.
6. If the archive produced one top-level item, publish that item directly.
7. If it produced multiple top-level items, publish one directory named after
   the archive.
8. Refuse an occupied final destination.
9. Publish with no-replace rename and remove staging after success, failure, or
   cancellation.

Common compound tar archives may unwrap one intermediate `.tar` layer. The
first slice does not recursively chase arbitrary nested archives.

## Operation and Undo contract

- Archive subprocess and filesystem work stays off GPUI's foreground executor.
- Cancellation terminates the supervised process and prevents publication.
- The first slice shows a cancellable labelled preparation/progress card while
  `7zz` runs. Structured item/byte progress is deferred until the backend can
  expose trustworthy totals without parsing presentation-oriented output.
- Successful compression and extraction each publish exactly one top-level
  result for incremental directory reconciliation.
- Undo validates the complete published identity tree before removing it.
- Redo revalidates the original input identities and repeats the operation only
  when the original destination remains unoccupied.
- Failed and cancelled work never enters the operation journal.

## Acceptance checks

- [x] Add a Marcel-owned archive backend interface and supervised 7-Zip
  implementation.
- [x] Implement and test the four-level runtime discovery order.
- [x] Replace the initial narrowly allowlisted RAR-enabled dependency with free
  `_7zz` in the Nix development environment without relying on the developer
  machine's ambient `PATH`.
- [x] Record Yazi provenance and bundled 7-Zip licensing in
  `THIRD_PARTY_NOTICES.md`.
- [x] List and preflight supported archives with bounded diagnostics.
- [x] Reject traversal, absolute-path, symlink, oversized, over-count, and
  dishonest-size fixtures.
- [x] Implement cancellable staging, validation, tidy publication, and cleanup.
- [x] Handle one-top-level, multi-top-level, empty, and one-intermediate-tar
  archives.
- [x] Implement ZIP creation for a file, directory, and multi-selection.
- [x] Refuse occupied compression and extraction destinations.
- [x] Add identity-validating Undo/Redo for both operations.
- [x] Activate `Compress…` and conditional single-selection `Extract` menu
  actions.
- [x] Apply exact top-level operation results without replacing the active
  directory session.
- [x] Add an installable Nix package whose wrapper guarantees free 7-Zip in its
  closure as private `libexec/marcel/7zz`.
- [ ] Make future portable artifacts install static `7zzs` as private
  `libexec/marcel/7zz`.
- [ ] Manually verify progress, cancellation, ZIP interoperability, broad
  extraction, large directories, and absence of stale UI results.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets`.
