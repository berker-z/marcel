# Sprint 23: Being the file picker

**Status:** Implemented. The local quality gate is green (285 library tests
plus 1 binary test, up from 260), the private-bus integration test covers the
D-Bus surface, and the graphical checks below were run by hand once against
the debug build. The design note this sprint started from is now the
description of what shipped: [`file-chooser-portal.md`](../file-chooser-portal.md).

## Goal

Answer `org.freedesktop.impl.portal.FileChooser`, so the open-file and
save-file dialogs of every application that goes through xdg-desktop-portal
are Marcel windows rather than the GTK backend's. With `FileManager1` and the
directory handler already in place, this was the last piece of "everything
file manager" a desktop asks for.

## What was built

A D-Bus backend (`file_chooser.rs`), a request model with no D-Bus in it
(`picker.rs`), a picker mode for the existing pane (`picker_state.rs`,
`Marcel::new_picker`, `window::open_picker`), a Nix variant that claims the
backend name and ships the portal file (`nix/file-chooser-portal.nix`), and a
module flag that wires it into `xdg.portal`. The picker is the ordinary Marcel
pane with a bar along the bottom: preview, bookmarks, type-to-filter, and
file operations all keep working while picking.

## Decisions taken

- **The pane is the dialog.** No second file browser for pickers; the window
  is `Marcel` with `picker: Some(_)`. What changes is what activating a file
  means, what the selection model allows, and which places are offered.
- **Filters match case-insensitively.** Applications write `*.[jJ][pP][gG]`
  because GTK is case-sensitive; on a filesystem where the user never chose
  the case, hiding `Photo.JPG` behind `*.jpg` is a bug, not fidelity.
- **One answer per request, always.** `PickerState` sends "cancelled" on drop
  if nothing was sent, so closing the window by any route answers the caller.
  The D-Bus side treats a dropped request that never reached a window as an
  error (2), not a cancel (1).
- **The process waits for its reply to leave.** GPUI quits on the last window,
  and for a bus-activated Marcel the picker is the last window. `ReplyTracker`
  counts requests until zbus reports the reply dispatched, and the quit hook
  drains it. Confirmed by hand: a cold-started Marcel answers, exits, and the
  caller gets the reply.
- **The name claim is opt-in and never a startup condition**, the same rule
  `FileManager1` follows. The installed variant sets
  `MARCEL_CLAIM_FILE_CHOOSER=1`; refusal is logged and survived.
- **`portals.conf` names Marcel alone.** The frontend takes the first
  configured name whose `.portal` file it loaded and never checks the bus, so
  a second entry is not a runtime fallback. The design note had claimed
  otherwise; it is corrected.
- **Every caller-supplied value is bounded and type-checked**, and `SaveFiles`
  names go through the Rename name rule, so an application cannot hand over
  `../escape.txt` and have it written outside the chosen folder.
- **Pickers are a separate list in the window registry.** A reveal must never
  navigate a dialog somebody is answering; an `Activate` must not raise one as
  "the Marcel I have". Bounded at eight.

## Delivered scope

- [x] `org.freedesktop.impl.portal.FileChooser` version 4: `OpenFile`,
  `SaveFile`, `SaveFiles`, the `version` property, and the per-request
  `Request` object with `Close`.
- [x] Options: `multiple`, `directory`, `filters`, `current_filter`,
  `accept_label` (mnemonic stripped), `current_name`, `current_folder`,
  `current_file`, `files`. Results: `uris`, `current_filter`.
- [x] Picker bar with name field, filter dropdown, Cancel, and the accept
  button; Enter in the name field confirms; Escape cancels from the browser
  and from the name field.
- [x] Single-selection mode on `SelectionModel` for pickers that asked for
  one file; a content filter on `DirectorySession` under the fuzzy filter.
- [x] Save: clicking a file proposes its name, typing a folder's name goes into
  it, an existing target asks before it is replaced. The existence check runs
  on a background thread.
- [x] Directory modes pick the selected folder or the current one.
- [x] `nix/file-chooser-portal.nix`, `nix/marcel.portal`, the activation file,
  `packages.<system>.file-chooser-portal`, overlay `marcelFileChooserPortal`,
  and `programs.marcel.fileChooserPortal` in both modules. The three wrappers
  stack in any combination and each re-points every activation file beneath
  it.
- [x] README, the portal note, and `TODO.md` updated.

## Acceptance checks

### Automated

- [x] Options decode: defaults, every option, wrong types rejected, traversal
  in `SaveFiles` names rejected, `multiple` ignored on save.
- [x] Filters: globs regardless of case, MIME families, empty filter hides
  every file, malformed glob taken literally, folders always pass.
- [x] Responses encode `uris` and echo the active filter; cancel and close map
  to 1 and 2.
- [x] `PickerState` answers once, and answers "cancelled" on drop.
- [x] `SelectionModel` in single mode collapses every growing gesture.
- [x] `DirectorySession` projects the content filter under the fuzzy filter
  and reconciles the selection.
- [x] Private bus: the backend name is owned, `version` reads 4, a call is
  answered through the channel and returns the URI, `Close` withdraws a
  pending call and the request object disappears afterwards, a dropped
  request returns 2, and the reply tracker drains to zero.
- [x] The three Nix wrappers stacked over a stub package produce activation
  files that all `Exec` the outermost wrapper, with the portal file in place.
- [x] `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-targets`, with the D-Bus test confirmed outside the sandbox.

### Manual, run once on 2026-09-16 against the debug build

- [x] Cold start: `MARCEL_CLAIM_FILE_CHOOSER=1` with the starter-bus variable
  set opens no browsing window; an `OpenFile` call opens the picker titled by
  the caller, at `current_folder`, with the `Images` filter hiding `notes.txt`
  and admitting `photo.JPG` under `*.jpg`; Trash is absent from Places.
- [x] Cancel returns `1`; the cold-started process then exits and the caller
  still receives the reply.
- [x] `SaveFile` seeded with `current_name` opens with the stem selected, the
  `Text` filter applied, and `_Export` shown as "Export"; Enter on an existing
  name shows the Replace prompt; Replace returns `0` with the URI and
  `current_filter`.
- [x] Warm start with a browsing window open: Escape in the picker returns
  `1` and leaves the browsing window; a second request with `multiple`, two
  files selected by click and shift+Right, and Open returns both URIs in
  visible order.

Found and fixed during those runs: the filter `Select` fills whatever height
it is given and was sitting against the top of the bar (now boxed), and
Escape with the browser focused was dispatched as the `ClearSelection` action
before the window key listener saw it (now cancels from the action path too).

### Through the real frontend, same day

- [x] Installed via the module, `portals.conf` written, frontend restarted;
  Helium's open dialog is a Marcel picker and the pick reaches the page.
- [x] Found and fixed: `dbus-broker` activation sets no `DBUS_STARTER_*`
  variables, so the activated Marcel opened a browsing window before the
  request; the cgroup's transient unit name is now recognised too.

### Not yet run

- [ ] A GTK application and a Firefox-family browser through the frontend.
- [ ] Bus activation from a fully stopped Marcel through the installed
  package, timing the first paint.

## Out of scope

- `choices` (extra combo boxes an application wants in the dialog) is accepted
  and ignored; no `choices` are returned, which the spec allows.
- `parent_window` is ignored. GPUI does not expose xdg_foreign, so the dialog
  is a normal top-level window rather than modal to its caller.
- A `cargo run` Marcel that is the running primary makes the installed
  variant's activation forward and exit without the portal name; documented in
  the note rather than handled.
