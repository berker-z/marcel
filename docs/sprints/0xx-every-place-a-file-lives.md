# Sprint ##: Every place a file lives

**Status:** Specified, not scheduled. This is the 0.2.0 line of work the
pre-tag review pointed at. It is written as one document because the pieces
depend on each other in a fixed order; when it is picked up, the parts split
into sprints along the lines drawn at the end.

## Goal

Marcel browses local folders and the Trash. A Nautilus or Dolphin user misses,
in roughly the order they notice, a USB stick in the sidebar, the ability to
move anything onto it, a network share, and search. Each of those is a place
that is not a local folder, and Marcel has no way to say "the user is looking
at a place" except a `PathBuf` and a bool. This work gives it one, then adds
the places.

## Where the code is now

"Where am I" is four values that have to agree: `DirectorySession::current_dir`
(a `PathBuf`), `SidebarState::browsing_trash` (a bool), a `Place` whose path is
the literal string `trash:///`, and a `NavigationHistory` that stores
`PathBuf`s and so cannot record a visit to the Trash. `browsing_trash` is read
in 27 places across nine files under `app/`, each re-deriving the answer. That
is how the Trash chip got into Move To (fixed in Sprint 28 with a filter, not
a design), why Back cannot return to the Trash, and why `start_trash_load` is
a second copy of `start_directory_load` with a different lister.

Cross-filesystem moves are refused on purpose (`transfer.rs`, "cross-filesystem
moves are not supported yet"), and since Sprint 28 so is trashing an item
whose Trash would be on another device. Both refusals are correct given what
the code can promise today, and both are exactly what a USB stick needs.

## Part one: Location

```rust
pub enum Location {
    Folder(PathBuf),
    Trash,
    // Later parts add: Volume, Search.
}
```

`DirectorySession` owns a `Location` in place of `current_dir` and the flag.
`NavigationHistory` stores `Location`, so Back and Forward through the Trash
work without a special case. `Place` carries a `Location` target instead of a
path; `PlaceKind` goes. `start_directory_load` and `start_trash_load` become
one `start_load(Location, LoadKind)` whose only `match` picks the lister:
`stream_directory` for a folder, `list_trash_records` for the Trash.

The 27 sites become `match` arms, and most of them collapse into a few
methods on `Location`: `as_folder() -> Option<&Path>` for everything that
needs a real directory (Move To's destination, Open in Terminal, New Folder,
paste), `label()` and `crumbs()` for the location bar, `accepts_drops()` and
`is_mutable()` for enablement. `command_enabled` reads those; nothing under
`app/` compares against `trash:///` again. The compiler then reports every
site a new variant has to consider, which is the point of doing this before
volumes rather than after.

Persisted state (`state.conf`) and the D-Bus surface keep speaking in paths.
`ShowFolders` and `ShowItems` hand in file URIs and get `Location::Folder`;
the picker never shows the Trash and keeps refusing it in one place, as
`AGENTS.md` asks. A picker's `current_dir` becomes `location.as_folder()`
with the Trash unreachable, so the `Option` is never `None` there.

Nothing is visible to a user except Back from the Trash working and Move To
having lost its filter in favour of not offering the Trash at all. It lands
alone, as one commit; it touches everything and should be reviewable on its
own.

## Part two: volumes

Two problems that look like one.

### Seeing them

UDisks2 on the system bus (zbus is already a dependency) is the source of
truth for block devices: `org.freedesktop.UDisks2.Block` for the device,
`.Filesystem` for whether and where it is mounted, `.Drive` for removable,
ejectable, and the model name that becomes the label when the filesystem has
none. Subscribe to `InterfacesAdded` and `InterfacesRemoved` on the object
manager and to `PropertiesChanged` on `Filesystem.MountPoints`; the sidebar
gains a Devices section that follows what is plugged in. Hidden partitions
(`HintIgnore`, the EFI system partition, swap, anything without a
filesystem) stay out, the way every other file manager filters them.

`Location::Volume { device: ObjectPath, mount: Option<PathBuf> }`. A mounted
volume browses as its mount point with no new listing code; an unmounted one
sits in the sidebar and mounts on click through `Filesystem.Mount`, which
goes through polkit and needs no privileges on a desktop session. The
context menu on a device offers Unmount and, for removable drives, Eject
(`Drive.Eject`); unmounting the volume being browsed navigates Home first.
A pulled stick removes its place and, if it was being browsed, the listing
shows the error the watcher already produces for a vanished folder.

All of this lives in `desktop/volumes.rs` behind a `VolumeEvent` stream the
app reads the way it reads directory events; nothing under `app/` names
UDisks2. The desktop module already has the pattern in `desktop/bus.rs`.
Without UDisks2 on the bus (a container, a BSD one day) the section is
absent and everything else works.

### Moving things onto them

This is the feature people mean by "USB stick support", and it is `fsops`,
not UI.

A cross-device move becomes copy, verify, then remove the source, journalled
as one operation. `TransferMode` gains a variant, or `Move` learns to detect
the device boundary the way the refusal does today and switch strategy per
item. Per item, in order: `copy_one` into staging on the destination (Sprint
28's descriptor-based, fsynced copy, unchanged), publish, verify the
published tree against the source by size and, for regular files, content
identity where the cost is acceptable (a size check plus the fsync is what
`mv` gives you; a full read-back is a setting, off by default), then remove
the source. Removal is `trash` when the source device has a Trash and
`delete` otherwise, and the report says which. Cancellation between copy and
removal leaves both copies and reports it; the journal never records a
removal it did not perform.

Undo of such a move is the reverse in the reverse order: copy the item back
from the destination to its original path (the identity checks that guard
every restore apply), verify, then remove the destination copy. It is a
second cross-device move, so it costs what the first did, and the report
says so before it starts for anything over a threshold.

FAT and exFAT (most sticks) have no modes, no ownership, no xattrs, no
symlinks, and two-second timestamps. `preserve_metadata` gets a "what this
filesystem can hold" mode: it applies what it can, notes what it dropped once
per operation ("permissions and links were not kept: the destination is
exFAT"), and does not fail. A symlink in a tree headed for FAT is copied as
its target by default, with the alternative (skip, with a note) as the
conflict dialog's fourth answer. `statfs` on the destination tells the code
which case it is in.

The conflict dialog treats a cross-device move like a move: replace, rename,
skip, merge, one answer for the rest. The progress row reports the copy and
the removal as one operation with one cancel.

Trash on a volume: freedesktop says `.Trash-<uid>` at the mount root, and
the `trash` crate already does it. Sprint 28's cross-device refusal in
`trash.rs` stays; what changes is that the Trash place can show the union of
the home Trash and every mounted volume's `.Trash-<uid>`, each record
carrying which Trash it is in so Restore and Empty act on the right one.
This is optional and can trail the rest.

## Part three: network places

Marcel does not get its own SFTP or SMB client. GVfs mounts a share as FUSE
under `/run/user/<uid>/gvfs/<scheme>:host=<host>[,user=<user>]/`, and from
then on it is a directory: `Location::Folder`, `std::fs`, all of `fsops`
unchanged. Credentials, keyring storage, and the "trust this host" prompt are
GVfs's, shown through the desktop's own dialogs, and Marcel does not
reimplement them.

What Marcel adds: a Connect to Server entry that takes a URI (`sftp://`,
`smb://`, `ftp://`, `dav://`, `davs://`, and whatever else the installed
GVfs backends list) and asks GIO to mount it. The mount call goes over D-Bus
to `org.gtk.vfs.MountTracker` on the session bus, or, if that proves fiddly,
through `gio mount` the way Open With already goes through `gio open`. Active
GVfs mounts appear in the sidebar under Network with their display names
from the tracker, with Disconnect in the context menu; they survive a Marcel
restart because they are GVfs's, not Marcel's. The location bar accepts a
URI and mounts it on Enter.

Everything on a network mount can hang. Sprint 28 moved the foreground stats
off the UI thread and gave every load a cancel flag, so a stalled share shows
Loading and can be left; that was half the reason those findings mattered.
What remains is the watcher: inotify does not work on FUSE, so a network
folder polls on a long interval or does not update until Refresh, and the
listing says which.

Without GVfs installed, Connect to Server is absent and the Network section
does not appear. The Nix module gains a `settings.network` switch that adds
`gvfs` to Marcel's closure, off by default like `media`, and the README says
what it is for.

## Part four: search

`Location::Search { root: PathBuf, query: String }`. The lister is a
recursive walk rooted at `root`, riding `stream_directory`'s ticketed
cancellation and batch coalescing, filtering names with the same folded
fuzzy match type-to-filter uses, and honouring the hidden-files setting and
`is_internal_working_name`. Results are `FileEntry`s whose displayed name is
the path relative to `root`, so the existing list and grid draw them, the
preview pane previews them, and every command that takes a selection works
on them (Move To, Trash, Properties, Open, Copy Path). Commands that need
the current folder (New Folder, Paste, Open in Terminal) are disabled by
`as_folder()` returning `None`.

Search starts from the location bar with a prefix (a leading `/`? a
dedicated `Ctrl+Shift+F`?; pick one and put it in the README) and searches
the folder being browsed. It stops at a bound (100,000 entries, or a depth,
or a mount boundary by default so a network share is not walked by accident)
and says so in the footer the way the folder preview says "1,000+ items".
Back from a search returns to the folder it started from because history
holds the `Location`.

Content search is not this sprint. Name search is what the other three ship
and it covers most of what people type.

## Not in scope

- A content indexer (Tracker, Recoll). Name search only.
- Mounting internal partitions that need a password beyond what polkit asks.
- LUKS unlock, MTP phones, and CD/DVD burning. UDisks2 exposes the first two
  and they can come later through the same `volumes.rs`.
- Dual pane. Volumes and Move To together cover the drag-between-two-places
  case for now.
- Trash union across volumes, unless the rest lands early.

## Sequencing

Three sprints, in this order, each shippable:

1. **Location**, alone. One commit, the compiler as the checklist, Back from
   the Trash as the only visible change. Then **volumes in the sidebar** with
   mount, unmount, eject, and browsing them. Nothing in `fsops` moves.
2. **Cross-device move.** `fsops` only, plus the dialog and progress wording.
   Tests: pull-the-plug points between copy and removal, undo of a moved tree,
   FAT metadata loss reported once, a symlink headed for exFAT. Needs a loop
   device or a tmpfs in the sandbox to be a real second filesystem; `tmpfs`
   under `TMPDIR` is enough for the device-boundary logic, a `vfat` image for
   the metadata cases, and the latter can be a test that skips without
   `mkfs.vfat`.
3. **Network and search.** GVfs mounting and the Network section; the search
   location on top of `stream_directory`. Both are `Location` variants plus a
   lister, which is what part one buys.

## Acceptance checks

- [ ] Back from the Trash returns to the folder that was open before it.
- [ ] Move To, Paste, New Folder, and Open in Terminal are disabled in the
      Trash and in search results, and enabled on a mounted volume.
- [ ] A USB stick appears in the sidebar within a second of being plugged in,
      mounts on click, browses, unmounts and ejects from its context menu, and
      disappears when pulled; pulling it while browsing shows the vanished
      folder error, not a hang.
- [ ] Moving a folder from `~` to a FAT stick copies it, removes the source,
      reports once that permissions and links were not kept, and Undo puts it
      back; cancelling between copy and removal leaves both and says so.
- [ ] Connecting to an `sftp://` host mounts through GVfs, prompts for
      credentials through the desktop's dialog, browses, and Disconnect
      unmounts; a stalled share can be navigated away from.
- [ ] Search on `~` returns within the bound, shows relative paths, previews
      a result, and Back returns to `~`.
- [ ] Without UDisks2, GVfs, or both on the bus, the corresponding sidebar
      sections are absent and everything else is unchanged.
- [ ] `state.conf`, `ShowFolders`, and the picker still speak in paths and
      never see the Trash, a volume path they cannot open, or a search.
- [ ] The gate is green and the D-Bus test passes unsandboxed.
