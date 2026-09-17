# Sprint 26: Permissions, sorting, a theme that sticks, and the release blockers

**Status:** Implemented. The quality gate is green (294 tests, the private-bus
integration test run unsandboxed as before). Driven by hand once in a running
build: sorting from the heading and the picker, a `chmod` from the dialog and its
undo, and a theme surviving a restart. The rest of the list below is still to
do.

## Goal

Three small features in the vein of Sprint 25, and the five things the
external review said had to be fixed before `v0.1.0` is tagged. Everything
here is half a day or less on its own; together they are what stands between
the tree and a tag.

## What was built

### Editable permissions

The Properties dialog's Owner, Group, and Others rows are checkboxes now,
worded the way the text was (Read, Write, Execute; List, Create and delete,
Enter for a folder). Ticking one calls `fsops::set_mode`, which records the
bits before and after as `OperationRecord::SetMode` and commits with one
`chmod`. Undo puts the old bits back, after validating that the object is
still the one whose bits were read; redo is the same function run the other
way, like a rename. The change goes through the operation coordinator, so it
is refused while another operation runs (the checkboxes are disabled
meanwhile), reported as "Changed permissions of …", and lands in the journal
between everything else.

Symbolic links keep the words. `chmod` follows links, so the bits would land
on the target, which is not what the dialog is describing.

The dialog subscribes to `OperationEvent::Applied` and re-reads its item when
its path is among the changed ones, without re-walking a folder's totals.

### Sorting

`SortOrder` (a key and a direction) lives in `browse/entries.rs` beside the
comparator it drives; `DirectorySession` owns the one in force and every merge
and re-sort honours it. Folders come first whatever the key. The keys are
name, modified time, size, and kind (the folded extension), each falling back
to the name. `FileEntry` gained `modified`, read from the metadata the
enumeration already had.

The list view has a header row: Name, Size, and Modified, the active one
carrying an arrow. Clicking a heading sorts by it in its natural direction
(names forwards, dates and sizes newest and largest first) and clicking it
again reverses. The top bar has two buttons between the location bar and the
filter, acting on what the location bar shows: a view toggle (list or grid,
one click each way) and a sort button that opens the same picker for both
views, with the four keys as checked items and Reverse Order. The picker
uses the context menus' popover shell with a third target, so it looks and
dismisses like them; the one wrinkle is that the shell dismisses on the press
and the click arrives afterwards, so the button records whether the picker
was open when pressed and does not reopen it. The sort items were in the
folder context menu first, then a sidebar button, before landing here; the
sidebar footer keeps only Show Hidden and the Settings gear. The view marks
are drawn (three bars, four squares) because the font's list and grid
glyphs are x-height math operators that sit tiny beside the sort arrows. The
order is kept in `state.conf` as `sort=` and `sort_direction=`.

A batch streamed in one order and merged under another would corrupt the
listing, so `merge_batch` re-sorts each batch under the current order first.
Sorting an already sorted batch is a linear scan.

The Modified column shows the time alone for today, day and month with the
time for this year, and the date otherwise.

### Copy Path

The item menu's last greyed entry. It puts every selected path on the
clipboard, one per line, the way Copy Location does for the folder; it is a
menu `Action` like that one rather than a command, since nothing else needs
to dispatch it.

### Theme persistence

Review B-04. `BrowserState` carries `theme: Option<Palette>`, written only
once someone has picked one in Settings. `theme::chosen()` is that choice;
`theme::init` takes it, and falls back to `MARCEL_THEME` (which the Nix
module sets) and then to Nord. So the Nix option is the default, the dialog
overrides it, and deleting the `theme=` line goes back to the option. The
state file is read once before the first window so the first frame is already
in the right colours. The module option's description and the README say all
this.

### The blockers

- **B-01.** `copy_file_cancellable` creates the destination with mode `0600`
  and `preserve_metadata` widens it to the source's mode at the end. The
  staging directory was already private, so nothing outside the process could
  read the file before; now the file does not depend on that.
- **B-02.** `preserve_metadata` applies extended attributes, then times, then
  the mode, because a mode without the owner's read bit makes the file
  unopenable and applying times needs an open descriptor. A regression test
  drives the function with a `0000` file and a `0300` folder.
- **B-03.** Two changes. `validate_entry_os_name` refuses any name beginning
  with `.marcel-`, so Rename, New Folder, and a typed conflict answer cannot
  produce a name Marcel would later hide or sweep. And replacement
  quarantines carry the boot id:
  `.marcel-replaced-<boot>-<pid>-<seq>-<name>`. The sweep only reclaims names
  from this boot whose process is gone. A name from another boot is neither
  reclaimed nor hidden; the load warning explains it and says it is safe to
  delete. Names in the old shape parse as nobody's, which leaves them alone.
- **B-05.** Not fixable the way the review asked. gpui-component depends on
  the Zed repository without a revision, and cargo refuses a `[patch]` that
  points a git source back at the same repository, so a `rev` on Marcel's
  lines splits the build into two GPUIs. Instead `Cargo.toml` records the
  revision under `package.metadata.marcel.zed-rev`, and
  `scripts/check_version.sh` fails when `Cargo.lock` resolves any Zed crate
  to something else, or to more than one thing. The lock is the pin; the
  check is what makes a stray `cargo update` fail CI.

### CI

`ci.yml` still runs the full gate on tags and on demand. A new `pr.yml` runs
on pull requests: the version and Zed-revision check, AppStream validation,
`cargo fmt --check`, and Clippy. Clippy compiles GPUI, which is why it was
kept off every push; on a pull request it is worth the minutes.

## Not built

Owner and group are shown, not changed; `chown` needs privileges Marcel does
not have. Setuid, setgid, and the sticky bit are shown in the Mode row and
preserved by copies but not editable. The preview pane's folder listing keeps
name order regardless of the browser's sort.

## Acceptance checks

- `cargo fmt --check && cargo clippy --all-targets --all-features -- -D
  warnings && cargo test --all-targets` passes, with
  `desktop::bus::tests::private_session_bus_integration` run unsandboxed.
- `scripts/check_version.sh` passes, and fails when `zed-rev` is edited.
- [x] Tick Execute on a script in Properties; the Mode row updates, `stat`
  agrees, and `Ctrl+Z` puts it back.
- [ ] `Ctrl+Y` sets it again.
- [ ] Tick a bit on a file owned by root: an error notification, nothing
  changed, the checkbox does not stay ticked.
- [ ] Properties of a symbolic link shows words, not checkboxes.
- [x] Click Modified in a folder: newest first, arrow on the heading; restart
  Marcel: the order is kept.
- [ ] Click Modified again: oldest first.
- [x] Pick Modified from the sort button in grid view; the tiles reorder,
  the button says so, and the picker closes on a second click.
- [ ] Sort by Kind; Reverse Order.
- [ ] Type-to-filter and marquee selection still work with a header row.
- [x] Pick Tokyo Night in Settings, quit, start: still Tokyo Night. Delete
  the `theme=` line: `MARCEL_THEME` applies again (the first half is what was
  driven; the second follows from the unit tests).
- [ ] Rename a file to `.marcel-replaced-x`: refused with the reserved-name
  message.
- [ ] Copy `~/.ssh` (with `0600` keys): the copies are `0600`.
- [ ] Copy Path on a two-item selection pastes two lines.

One observation to keep an eye on: once during the hand run, `state.conf`
came back with `view=grid` and `sort=name` after a restart while `theme=`
survived, which is what a window persisting defaults would write. It did not
reproduce through a plain restart, a forwarded launch into a running
instance, or picking from the sort button, all of which kept the file intact.
