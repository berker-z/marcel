# Sprint 25: Properties, New File, Duplicate, Move To

**Status:** Implemented. The quality gate is green (278 tests, the private-bus
integration test run unsandboxed as before). Every path below was driven by
hand once in a running build: the dialog for a file, a folder, an archive, and
a multi-selection; New File and Duplicate from the menus and the keyboard;
Undo of both; and `ShowItemProperties` over `busctl`.

## Goal

Four commands the context menus had shown greyed with a `–` since Sprint 9,
and one inconsistency: extraction refused an occupied destination while every
other way of putting something somewhere asked. Each was cheap once Sprint 24
had put commands in a table and mutations behind one shape, and Properties was
the one most missed now that there is no other file manager to fall back to.

## What was built

### New File

`create_file` in `fsops/mutations.rs` opens the path with `O_EXCL`, so the
creation is the one commit: it either made the file or it changed nothing.
The record is `OperationRecord::CreateFile`, with the identity read from the
descriptor just opened rather than from a second `stat` of the path. Undo
removes the file only while it is still a regular file, still empty, and
still that identity. Anything the user has typed into it since is theirs,
and Undo refuses rather than deleting it. Redo recreates it.

The dialog is the New Folder one with different words. It sits in the
directory menu under New Folder, with no shortcut; the other file managers
do not bind one either.

### Duplicate

`Ctrl+D`, and the item menu. It is `start_transfer` of the selection into
the folder it lives in. The transfer already had an answer for "copy this
onto itself": pick the next free name, no question asked. So Duplicate is
the transfer with no new code on the filesystem side, and it inherits the
progress card, cancellation, and undo of a copy. The report says "Copied 1
item(s)", which is what happened.

### Move To

The item menu. It opens the folder chooser, the same window the portal
backend shows other applications, with "Move" on the button and the current
folder as the starting point, and moves the selection to whatever folder is
chosen. The answer comes back on the picker's reply channel and the move
starts through the application's operation owner, so it still happens if the
window that asked has gone. The transfer, its conflict questions, and its
undo are cut and paste's; a move across filesystems is refused the same way.

### Extraction meets an occupied name

`publish_extracted` in `fsops/archive.rs` now plans the publish through the
transfer's `plan_source`, so the occupied case gets the copy conflict dialog:
skip, a typed name, a free name, replace, or merge when both are folders.
Replace moves the occupant aside with the same quarantine a transfer uses and
puts it back if the rename fails; merge copies the staged tree into the
existing folder through `merge_directories`, keeping what is already there.
The `ArchiveExtract` record carries `replaced` and `merged` like a `Copy`
record, and undo runs through the same branch, so undoing a replacing
extraction removes the output and restores the occupant. That undo is not
redoable, as with a copy. A redo of a plain extraction asks nobody and
refuses an occupied name.

### Properties

`Ctrl+I`, `Alt+Enter`, both context menus, and `ShowItemProperties` on the
bus. With nothing selected it describes the folder shown. The command table
learned to bind more than one key to a command for this.

The design is the one from the earlier discussion: a common section every
object has, and a details section supplied by the loaders the preview pane
already uses, so the dialog and the pane cannot disagree about a file.

- `preview/details.rs` reads everything. `inspect` gives the common facts
  (kind from a content sniff before the name, size, symlink target, owner
  and group, mode, mtime/atime/btime, free space for a folder) and the
  type details: image dimensions from the header without decoding, PDF page
  count through the same cached `pdfinfo` path, line count of the first
  256 KiB of a text file, and entry count plus unpacked size of an archive
  through the same `7zz` listing extraction uses. `measure_tree` walks a
  folder without following links, publishing running totals every 100 ms,
  stopping at five million entries and saying so.
- `app/properties.rs` is the dialog. It is a GPUI view of its own so the
  facts can arrive after it opens; a folder's totals visibly grow. Dropping
  the view sets the cancel flag, and closing the dialog drops the view.
  A multi-selection gets a summary: what was selected, what lies inside
  the selected folders, and the total size.

Owner and group names come from `/etc/passwd` and `/etc/group` read directly.
`getpwuid` would need `libc` and `unsafe`, and the crate has neither. An
account that only exists through NSS (LDAP, systemd-homed) shows as a number.

Timestamps are formatted with `chrono`, which was already in the dependency
graph through GPUI; Marcel now names it directly.

### The bus

`show_item_properties` validates and enqueues like the other three methods
instead of returning `NotSupported`. The request counts as a reveal, so it
opens on the window the user is looking at, or on a new window at the first
item's folder when there is none. One thing bit here: a `WindowHandle<Root>`
leases the root view for the length of its `update` closure, and opening a
dialog needs to update that same root, which panicked. The untyped
`AnyWindowHandle` does not lease, which is what `surface::deliver` had been
relying on all along.

## Not built

Permissions are shown, not edited. An editable rwx grid is one journalled
`chmod` away and is the obvious next slice of this dialog; it is on the TODO
list. Merging while moving is still the deliberate gap it was, so Move To
refuses to merge a folder into an existing one, as cut and paste does.

## Acceptance checks

- `cargo fmt --check && cargo clippy --all-targets --all-features -- -D
  warnings && cargo test --all-targets` passes, with
  `desktop::bus::tests::private_session_bus_integration` run unsandboxed.
- [x] New File creates an empty file, reveals it, and Undo removes it; Undo
  after writing to the file refuses and leaves the content.
- [x] Duplicate of a file lands beside it as `name (2).ext` and Undo removes
  the copy.
- [x] Properties of a file, a folder (totals grow, then settle), a ZIP
  (entries and unpacked size), and a multi-selection.
- [x] `Ctrl+I` opens the dialog without putting a character in the filter.
- [x] Move To… moves a file into a chosen folder through the picker, and
  Undo brings it back.
- [x] Extracting over an existing file offers the conflict dialog; Replace
  publishes the archive's copy and Undo restores the original.
- [x] `busctl --user call io.github.berker_z.Marcel /org/freedesktop/FileManager1
  org.freedesktop.FileManager1 ShowItemProperties ass 2 file:///… file:///… ""`
  opens the summary on the current window.
- [ ] Properties of a symbolic link, a socket, and a file with mode `0000`.
- [ ] Properties of a folder with more than five million entries, to see the
  cap in a real dialog.
