# Sprint 29: Drives

**Status:** Implemented. The gate is green at 407 tests (from 392), with
`private_session_bus_integration` and the second-filesystem tests run
outside the sandbox against `/dev/shm` and against a real FAT stick.

## Goal

By the end of this sprint Marcel can read and write files on the Windows
partition of a dual-boot machine and on a USB stick: see them in the sidebar,
mount them, browse them, copy and move things onto and off them, and trash
things on them. The fundamentals come first; the sidebar gets the minimum it
needs to reach them.

This is the first slice of
[`0xx-every-place-a-file-lives.md`](0xx-every-place-a-file-lives.md), cut
differently from the order that document proposed. The `Location` refactor
is not needed to show drives: a mounted volume browses as a plain folder, and
a cross-device move is `fsops` work that never touches the browser. So this
sprint takes volumes and the move and leaves `Location` for search.

## Decisions, and where they come from

Before writing anything, Yazi (`014426f`), its `mount.yazi` plugin, Nautilus
(`41533bf`), and the GLib and GVfs code Nautilus delegates to were read for
each question below. The answers follow the field where the field agrees and
the earlier spec where it was already stricter.

**Which volumes are shown** follows GVfs's UDisks2 monitor
(`gvfsudisks2volumemonitor.c`, `should_include_volume`): a block device is
listed when UDisks2 does not set `HintIgnore` on it and it carries a
`Filesystem` interface. On this machine that already hides the EFI partition,
the Windows recovery partition, and `/boot`, and shows the Windows system
partition and the stick. A mounted volume is hidden when its mount point is
system-internal (the GLib exact-match list, plus anything under `/dev`,
`/proc`, `/sys`) or outside `/media`, `/run/media/<user>`, and `$HOME`; an
unmounted one is hidden when its fstab entry points somewhere like that.
`x-gvfs-show` and `x-gvfs-hide` in the fstab options override both, and
`x-gvfs-name` renames. Yazi shows every partition including swap and EFI,
which is right for a modal table and wrong for a sidebar.

**Who mounts** is UDisks2 through `Filesystem.Mount` on the system bus, the
same call GVfs makes. It goes through polkit: a removable stick mounts
without a prompt on a local session, a system-internal partition asks for
the password (`filesystem-mount-system`) unless fstab covers it. The
idiomatic answer for a fixed Windows partition, in Nautilus and now in
Marcel, is an fstab line such as

```
UUID=A8083BDD083BA8E8 /mnt/windows ntfs3 uid=1000,gid=100,noauto,nofail,x-gvfs-show,x-gvfs-name=Windows 0 0
```

which Marcel lists as "Windows" whether or not it is mounted, and mounts on
click through the fstab entry with no prompt. Windows Fast Startup has to be
off, or NTFS is left dirty and `ntfs3` mounts it read-only; no file manager
can fix that. Yazi shells out to `udisksctl` and falls back to `sudo gdbus`;
Marcel already has zbus and a system-bus connection costs nothing.

**A cross-device move is copy, then remove the source.** Both Yazi
(`yazi-scheduler/src/file/file.rs`) and GIO (`g_file_move` falling back to
`g_file_copy` plus `g_file_delete`) do exactly that, per item, with no
read-back. Marcel's copy already fsyncs every file and publishes the tree
with one rename, which is more than either does, so the copy half is
`copy_one` unchanged. After publication the destination tree is compared with
the source by kind and size before the source is removed; a full read-back
was in the earlier spec as a setting and is dropped, since nobody ships it
and it doubles the time. The source is removed with the permanent-delete
path (one rename into quarantine, then erase), not trashed: the spec's idea
of trashing the source is novel, doubles disk use until the Trash is
emptied, and the journal already gives undo. Cancelling between the copy and
the removal keeps both and says so.

**Undo of a cross-device move** is the same move in reverse: validate the
destination tree against the record, copy it back to the source path, verify,
remove the destination. Nautilus does the same (a second `g_file_move`). It
costs what the first move cost.

**Metadata on FAT and NTFS** follows GIO's `g_file_copy_attributes`, which
asks the destination what it can set and ignores the rest ("Failure to copy
metadata is not a hard error", in Nautilus's words). Marcel applies what
fits and notes once per operation what it dropped. Yazi's `.ok()` on every
attribute call is the same policy without the note. A symbolic link headed
for a filesystem that refuses links is copied as its target when the target
is a regular file, and skipped with a note otherwise; Nautilus errors and
offers Skip, which is the conservative reading, but the copied file is what
people plugging in a stick want.

**Trash on a volume** is `.Trash-<uid>` at the mount root, which the `trash`
crate already does and Marcel's Trash view already lists (the crate's
`os_limited::list` walks every Trash it knows). GIO refuses on
system-internal mounts and Nautilus then asks "delete it immediately?";
that dialog is not in this sprint, and a refusal reads as the error it is.

## What is built

### `desktop/volumes.rs`

A UDisks2 client on the system bus. `GetManagedObjects` once, then
`InterfacesAdded`, `InterfacesRemoved`, and `PropertiesChanged` on the
object manager, each of which makes `VolumeStore` (in `src/volumes.rs`, the
GPUI entity every window observes, like the bookmark store) re-read the
objects and publish a fresh `Vec<Volume>`. A `Volume` carries the block object path, the device node, a display
name (label, `x-gvfs-name`, or "268 GB Volume"), the filesystem type, the
mount point if mounted, and whether the drive is removable, ejectable, or
can be powered off. `mount`, `unmount`, and `eject` are async calls on the
same connection; eject is unmount followed by `Drive.Eject` when the drive
says it is ejectable (USB sticks do, and detach until replugged) or
`Drive.PowerOff` otherwise, which is what GVfs does. Without UDisks2 on the
bus the store stays empty and the sidebar has no Devices section.

### Sidebar

A Devices section under Places. Each row is a drive icon from the theme, the
name, and for a mounted removable drive an eject button. Clicking a mounted
volume navigates to its mount point; clicking an unmounted one mounts it and
navigates when the mount returns. Right-click offers Unmount, and Eject for
removable drives. Unmounting the volume being browsed navigates Home first,
because the mount point is about to stop being a directory the user can
stand in. A pulled stick disappears from the list; if it was being browsed
the listing shows the vanished-folder error the watcher already produces.

### `fsops`

`move_one` tries the rename first. On `CrossesDevices` it measures the tree
for progress, runs `copy_one` to the destination, verifies the published
tree against the source, and removes the source through `delete_paths`. The
`MoveRecord` it returns is marked as having crossed devices, and its
`expected_state` describes the destination tree, so `undo_operation`
validates it the way it validates any move and then reverses it with a copy
back rather than a rename. Redo goes through `transfer_paths` as before and
crosses devices again on its own.

`preserve_metadata` reports what it could not apply instead of failing:
permissions, extended attributes, and ownership on a filesystem that
answers `EPERM` or `ENOTSUP`. The copier collects those into a set of
`MetadataLoss` kinds, and `TransferOutcome` carries them as `losses`, which
the report appends once, as a warning rather than a success: "symbolic links
were replaced by copies of their targets: the destination filesystem does
not support them". A symbolic link the destination refuses is replaced by
its target's content when the target is a regular file.

## Not in this sprint

- `Location`. The sidebar highlights a volume by comparing `current_dir`
  with the mount point, which is the same comparison places use.
- Network places, search, the Trash union view, the "delete immediately?"
  dialog, LUKS, MTP, optical burning.
- Watching a FUSE mount. `ntfs3` is a kernel filesystem and inotify works
  on it; `ntfs-3g` mounts do not update until Refresh.

## Acceptance checks

- [x] The stick appears under Devices, mounts on click without a prompt,
      browses, and its eject button navigates Home, unmounts, and powers it
      off (UDisks2 then drops the device until it is replugged, as with
      Nautilus). Pulling it while browsing is not yet checked by hand.
- [ ] With the fstab line above, "Windows" is listed unmounted, mounts on
      click without a password, and its files can be copied out, copied in,
      moved in, renamed, and trashed. (The partition is listed as "268 GB
      Volume" without the line; the fstab line is the user's to add.)
- [x] Moving a folder from `~` to the stick copies it, removes the source,
      reports once what FAT could not keep, and Undo puts it back (as FAT
      held it). Cancelling during the copy leaves the source alone;
      cancelling after publication keeps both and says so (tested with an
      injected boundary).
- [x] A symbolic link to a file, moved to the stick, arrives as the file.
- [x] Trashing a file on another filesystem puts it in `.Trash-1000` at its
      root, and it shows in the Trash place and restores from there
      (`MARCEL_TEST_OTHER_FS`, run against `/dev/shm`).
- [x] Without UDisks2 on the system bus there is no Devices section and
      everything else is unchanged (the store logs why and stays empty).
- [x] The gate is green, `private_session_bus_integration` passes
      unsandboxed, and the cross-device tests that need a second filesystem
      run against `/dev/shm` when it is writable and skip otherwise.
