# Sprint 24: Structure

**Status:** Implemented. The quality gate is green (264 library tests plus 1
binary test; the private-bus integration test needs an unsandboxed run, as
before). No behaviour changed on purpose; every assertion the suite made
before this sprint it still makes, some of them from a merged test.

## Goal

The crate had grown by accretion: `app.rs` held the whole window at 4,000
lines, `file_ops.rs` and `desktop_integration.rs` were similar piles, and the
things that connected them — which module owned the journal, where a shortcut
was declared, what a mutation promised about the disk — had to be recovered by
reading. This sprint gives the crate a shape a reader can hold in their head,
removes what was only there because nothing had removed it, and pulls the
patterns that had been spelled out by hand into one place each.

## What was built

A module tree that reads top-down (`src/lib.rs` carries the map):

- `app/` — the window as one `Marcel` view, split by concern: `state`,
  `actions`, `navigation`, `edits`, `dialogs`, `pointer`, `preview`,
  `browser`, `menu`, `sidebar`, `location`, `picker`, `chrome`.
- `browse/` — the read side: `entries`, `directory_session`, `watcher`,
  `selection`, `history`.
- `fsops/` — everything that changes the disk. `local` is the primitives (the
  one rename that never replaces, the one open that refuses a FIFO),
  `identity` is "still the same object", `journal` is what a mutation records,
  and `transfer`, `copy`, `delete`, `trash`, `archive`, `quarantine`,
  `conflict`, `history`, `mutations` are the mutations themselves.
- `preview/` — decoding. `desktop/` — the bus, the portal, launches, opening
  with other applications, icon themes, places.
- `operations` — the application-wide owner of running work and history.

Within that:

- **Commands are a table.** `browser_commands!` in `app/actions.rs` declares
  each command's action, binding, and `BrowserCommand` once; menus, toolbar
  buttons, and shortcuts all reach `command_enabled` and `execute`.
- **Operations run through one path.** `OperationCoordinator::run` releases
  the busy lock and reports in one place; `run_committing` is the shape every
  single-path mutation takes; `ProgressCard` is how an operation describes
  itself to the progress surface.
- **Dialogs are values.** `Confirm` and `NameDialog` describe a dialog; the
  window opens them. The conflict dialog's buttons share one answer path.
- **Errors name their path once.** `PathContext::at("Could not create", path)`
  replaces the forty `with_context(|| format!(...))` the layer had grown.
- **The palette is one table.** `theme::PALETTES` holds every palette's name,
  label, aliases, and swatches; `Palette::ALL` and lookup derive from it.
- **Tests share a sandbox.** `testing::Sandbox` makes a tree from relative
  paths, so a fixture reads as the shape of the tree rather than the calls
  that built it. Tests that proved one rule from several angles are one test.

## What was removed

- The non-Linux and non-Unix fallbacks (icon lookup, sparse copy, quarantine
  ownership, name display). Marcel needs `RENAME_NOREPLACE`, `/proc`, and the
  session bus, and the README says Linux only.
- The test-only directory-event path in `DirectorySession`, the
  `DesktopRequestError` newtype, the duplicated bus `validate_and_enqueue`,
  and `TransferMode::verb`, which nothing called.
- The bus integration test's child was named by the old module path and would
  have run zero tests after the restructure; it uses `module_path!()` now.

## Numbers

| | lines (`src/**/*.rs`) |
| --- | ---: |
| before the sprint | 28,669 |
| after | 25,789 (−10.0%) |
| after, with `use_small_heuristics = "Max"` in `rustfmt.toml` | 23,895 (−16.7%) |

The 30% the sprint set out for was not reached honestly. What is left is
either GPUI element-builder chains, which rustfmt lays out one call per line,
or code that says something. The rustfmt option is a formatting policy, not a
simplification, and is offered as a separate commit so it can be dropped.

## Acceptance checks

- `cargo fmt --check && cargo clippy --all-targets --all-features -- -D
  warnings && cargo test --all-targets` passes, with
  `desktop::bus::tests::private_session_bus_integration` run unsandboxed.
- Every test name that disappeared is accounted for by a merge whose
  assertions survive (`git diff master --stat -- '*tests*'` plus the merged
  test's doc comment naming what it folds in).
- `AGENTS.md` names where shortcuts live and carries the module map.
