# Marcel session handoff

**Prepared:** 2026-07-29
**Branch:** `master`
**Workspace:** `/home/berkerz/Projects/marcel`

This document is the starting point for the next development session. Read
`AGENTS.md` first, preserve the current dirty working tree, and continue from
the state below rather than rebuilding or reverting it.

## Repository state

The latest commit is:

```text
69d139a feat: add rename and incremental operation updates
```

`master` is one commit ahead of `origin/master`; `69d139a` has not been pushed.
The previous remote tip is:

```text
09d86c8 docs: refresh README for alpha
```

The working tree intentionally contains the following uncommitted work:

```text
 M Cargo.toml
 M README.md
 M THIRD_PARTY_NOTICES.md
 M docs/TODO.md
 M docs/interaction-model.md
 M docs/sprints/009-conventional-local-actions.md
 M flake.nix
 M src/app.rs
 M src/commands.rs
 M src/file_ops.rs
 M src/lib.rs
 M src/operations.rs
?? docs/HANDOFF.md
?? docs/sprints/010-archive-operations.md
?? src/archive_ops.rs
?? src/system_terminal.rs
```

Do not discard or overwrite these changes. They form one coherent
context-menu, Open-in-Terminal, and archive-operation working tree. The menu
and terminal portion preceded the archive work; both are waiting for manual UI
testing before commit.

## Last committed slice

Commit `69d139a` contains:

- inline Rename through `F2` and the item menu in list and icon views;
- extension-preserving initial selection;
- Linux `RENAME_NOREPLACE`;
- filesystem-identity-validated Rename Undo/Redo;
- incremental reconciliation for every existing Marcel-owned write operation;
- exact top-level remove/upsert reporting for create, rename, copy/move,
  internal drag-move, Trash/restore, permanent delete, Empty Trash, Undo, and
  Redo;
- preservation of the active watcher, scroll position, and directory session
  instead of a visible full reload;
- full rescan only for explicit Refresh or a correctness fallback.

Berker manually verified that the large-directory reload flash was gone across
the tested operations.

## Current uncommitted implementation

### Visual hierarchy and grid typography

Sprint 11 starts the requested UI-fix pass:

- the Nord top bar, Places pane, and preview pane now share the darker `nord0`
  shell while the center browser uses the lighter `nord1` surface;
- hover rows move to `nord2`, preserving contrast after the surface inversion;
- Place and bookmark names now use the base UI size, and Places width
  measurement uses that same size;
- grid names use base-size text, a compact explicit line height, three visible
  lines, and extension-preserving elision;
- grid visuals grow from 88 to 104 px, fallback icons from 56 to 80 px, and
  tile geometry grows only enough to contain the larger visual and label.

The first manual screenshot confirmed the hierarchy, type-size, and icon-size
direction. It also revealed third-line clipping from the inherited line height;
the explicit line height and 64 px label box were added afterward and still
need a final manual look.

The same sprint now includes Marcel's first Settings modal. A gpui-component
Settings button sits to the right of the list/icon switch, and its theme
dropdown hot-applies twelve built-in palettes. Custom palettes share one
semantic dark-scheme mapper so browser, shell, hover, selection, status, and
component colors change together. Theme changes preserve the active UI and
monospace font choices, sizes, and radii. Selection remains session-only;
versioned XDG settings persistence is still planned.

### Context-menu semantics and typography

The custom entry, empty-space, and bookmark context-menu shells now follow one
visual contract:

- `–` plus muted text means a planned, unimplemented action;
- muted text without `–` means an implemented action that is currently
  unavailable;
- implemented labels never change merely because command state is disabled;
- menu labels inherit semantic `text_sm`;
- shortcut hints deliberately use `text_xs` as secondary metadata.

The old conditional labels such as `– Paste`, `– Undo`, `– Rename`, and
`– Delete` were removed. The `planned` renderer now adds the prefix itself, so
call sites cannot accidentally confuse the two states.

### Open in Terminal

`BrowserCommand::OpenTerminal` and its GPUI action are implemented. The active
empty-space menu row opens the displayed ordinary filesystem directory.

Linux launching lives in `src/system_terminal.rs` and currently resolves a
terminal in this order:

1. proposed `xdg-terminal-exec --dir=…`;
2. the user's `TERMINAL` executable;
3. bounded known-emulator fallbacks:
   Kitty, Ghostty, Konsole, GNOME Terminal, Console/KGX, Ptyxis, Foot,
   Alacritty, WezTerm, XFCE Terminal, xterm, and urxvt.

Each process receives the desired working directory. Marcel strips
`LD_LIBRARY_PATH` so the Nix development shell cannot poison an external
terminal's native linkage.

The first manual test opened Kitty in Bash even though Berker's login shell is
Fish. This was diagnosed exactly:

```text
host SHELL=/run/current-system/sw/bin/fish
nix SHELL=/nix/store/...-bash-5.3p15/bin/bash
/etc/passwd login shell=/run/current-system/sw/bin/fish
```

The launcher now also removes `SHELL`, allowing Kitty to resolve the real
login shell. This final Fish fix has passed automated tests but still needs a
manual `cargo run` test.

### Documentation already updated

The dirty documentation changes:

- mark Open in Terminal implemented in Sprint 9 and the backlog;
- describe the menu prefix/disabled-state contract;
- describe terminal resolution and Nix environment cleanup;
- remove Open in Terminal from README known gaps;
- record the initial archive portability conclusion.

## Verification

The latest quality gate, run after the archive implementation, passed:

```sh
nix develop -c bash -lc \
  'cargo fmt --check && \
   cargo clippy --all-targets --all-features -- -D warnings && \
   cargo test --all-targets'
```

Result:

```text
122 passed; 0 failed
```

`nix flake check` also passed for x86-64 Linux. The only emitted Rust warning
was the existing future-incompatibility notice for `proc-macro-error2 v2.0.1`.

## Immediate manual test

Run Marcel from the Nix development shell, right-click browser empty space,
and verify:

1. all ordinary menu labels have one size;
2. shortcut hints are consistently smaller;
3. only genuinely unimplemented rows have `–`;
4. Paste/Undo/Redo and other implemented disabled actions have no `–`;
5. Open in Terminal launches Kitty in the displayed directory;
6. the launched interactive shell is Fish, not Bash.
7. Compress one file, one directory, and a multi-selection to ZIP.
8. Extract ZIP, 7z, and one compound tar archive beside each source.
9. Verify one-top-level and multi-top-level tidy behavior.
10. Verify occupied compression and extraction destinations are refused.
11. Cancel a larger archive operation and confirm no published partial result.
12. Undo and Redo both compression and extraction.

If those pass, commit the dirty work deliberately. The earlier menu/terminal
message remains suitable for that portion:

```text
feat: normalize menus and open directories in terminal
```

Do not push unless Berker asks; the prior `69d139a` commit is also still local.

## Archive implementation

The archive slice is now implemented in `src/archive_ops.rs`, `src/file_ops.rs`,
`src/operations.rs`, and `src/app.rs`. No Cargo archive-format dependency was
added; Marcel supervises official `7zz` behind its own backend.

Important conclusions:

- MIME types identify archives and select applications; they do not expose a
  portable compress/extract operation.
- Rust's `zip` crate handles ZIP only. It does not make 7z or RAR work.
- A pure-Rust 7z backend exists, but RAR support introduces separate format,
  maintenance, and license concerns.
- Yazi's proven broad-format implementation invokes `7zz`/`7z`, extracts into
  staging, tidies the result, and then publishes it.
- Official 7-Zip can create and extract ZIP and 7z, and can extract RAR/RAR5
  plus many other formats. It cannot create RAR.

The distro-agnostic implementation puts `7zz` behind a Marcel-owned archive
backend and requires each distribution artifact to supply it automatically:

- Nix/nixpkgs: use the narrowly allowlisted `_7zz-rar` runtime dependency so
  RAR extraction is actually present, and wrap Marcel's PATH;
- Debian/RPM/Arch packages: depend on the distribution's 7-Zip package;
- Flatpak: build 7-Zip as a module in the Flatpak;
- AppImage/portable tarball: ship a private architecture-specific
  `libexec/marcel/7zz`;
- source/development builds: discover `7zz` or legacy `7z` on PATH.

Recommended runtime resolution:

1. `MARCEL_7ZZ` override for tests/development;
2. Marcel's private `libexec/marcel/7zz`;
3. `7zz` on PATH;
4. legacy `7z` on PATH.

Marcel—not 7-Zip—must own scheduling, cancellation, bounded diagnostics,
staging, output validation, no-overwrite publication, operation results,
progress presentation, and Undo integration. Required LGPL/BSD/UnRAR notices
must be shipped for artifacts that bundle 7-Zip.

### Implemented archive UX

Berker confirmed these points on 2026-07-29:

- `Compress…` works for files, directories, and multi-selection.
- The initial creation format is ZIP, even though `7zz` leaves room for a
  later format selector.
- Supported archive selections expose `Extract`.
- Extraction always publishes beside the archive. Marcel does not expose an
  `Extract To…` action or destination dialog.
- Double-click does **not** extract. It remains read-only/default-application
  activation because extraction is a write operation.
- A future preview should list archive contents without mutating the
  filesystem.
- Extraction follows Yazi's staging/tidy model:
  - extract into a private hidden staging directory;
  - if output has one top-level item, publish that item;
  - if it has multiple top-level items, publish a directory named after the
    archive;
  - validate the complete staged tree;
  - refuse occupied destinations until conflict UI exists;
  - atomically publish and clean partial staging on error/cancellation.

Portable artifacts bundle only the static architecture-specific executable
under the private name `libexec/marcel/7zz`, plus required notices. For
official 7-Zip 26.02 x86-64, that is approximately 3.59 MiB installed and
1.29 MiB compressed. Package-managed artifacts may use their maintained 7-Zip
package instead.

The bounded implementation and remaining acceptance contract lives in
[`Sprint 10`](sprints/010-archive-operations.md).

Pinned Yazi audit source:

```text
commit 319f90e0eab185a231eef5562215ba322e320286
yazi-plugin/preset/plugins/extract.lua
yazi-plugin/preset/plugins/archive.lua
```

A detached audit checkout currently exists at:

```text
/tmp/marcel-yazi-rename-audit-319f90e
```

Temporary paths may not survive a machine reboot; recreate the checkout at the
pinned commit if needed.

## Recommended next sequence

1. Manually test the menu and Fish terminal fix.
2. Commit the current dirty slice if accepted.
3. Manually test archive creation, extraction, progress/cancellation, and
   Undo/Redo from both browser views.
4. Add the future installable Nix/portable packages with guaranteed private or
   wrapped 7-Zip availability.
5. Run the remaining manual large-directory/archive acceptance tests recorded
   in Sprint 10.

Sprint 9 still has New File and Properties remaining. The existing sprint says
to run the dedicated UI-fix pass after those conventional actions; archives
are a separate operation slice and should not silently expand Sprint 9.

## Contributor constraints to retain

- Use gpui-component by default unless a concrete limitation is documented.
- Keep filesystem enumeration, decoding, archive work, and subprocess I/O off
  GPUI's foreground executor.
- Make long-running work cancellable or safely superseded and memory-bounded.
- Preserve exact Yazi/other-upstream provenance in comments and
  `THIRD_PARTY_NOTICES.md`.
- Do not copy Zed GPL application code; use it only as GPUI documentation
  unless a specific file is compatibly licensed.
- Preserve user changes in the dirty worktree.
- Use `apply_patch` for manual edits.
- Before completing code changes, run formatting, strict Clippy, and all
  tests exactly as required by `AGENTS.md`.
