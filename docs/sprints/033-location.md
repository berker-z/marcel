# Sprint 33: Location

**Status:** Done 2026-09-27, in five commits (`fb0aed0` to `b04f82e`). The
gate is green at 474 tests, both bus tests run unsandboxed. Three checks
below need a stick in hand. Part one of
[`0xx-every-place-a-file-lives.md`](0xx-every-place-a-file-lives.md), done
late and extended with what Sprints 29 to 31 added. The case for it is in
[`../review-2026-09-27.md`](../review-2026-09-27.md#1-structure).

## Goal

Marcel should know where the user is as one value, worked out once, instead of
re-deriving it from a path every time something asks.

Nothing new appears for the user. Back from the Trash works, Move To lists
drives and shares, and items trashed on a stick can be found again. All three
were broken or missing because of the shape of the code, not because nobody
built them.

## Where the code is now

The plan said to do this before volumes. Sprints 29 and 30 skipped it on the
grounds that a mount is a folder. That is true of listing it, and it is why
volumes and shares needed no new lister. It is not true of everything else
that wants to know what kind of place a folder is. Each of those now has its
own way of asking:

- **The Trash** is `SidebarState::browsing_trash`, read at 28 sites in nine
  files, plus a `Place` whose path is `trash:///` and a `PlaceKind`
  (`desktop/places.rs:26`). History stores `PathBuf` (`browse/history.rs:8`),
  so it cannot record a visit to the Trash, and Back from the Trash skips the
  folder before it. `start_trash_load` is a second copy of
  `start_directory_load` (`app/navigation.rs:98`, `:220`).
- **Drives** are `VolumeStore::volume_containing`, called from three places
  in `app/`.
- **Shares** are `NetworkStore::mount_containing`, called from three more,
  and `active_server` has its own `starts_with`.
- **Remoteness** is `remoteness::of`, which parses `/proc/self/mountinfo`.

`mounted_breadcrumbs` (`app/location.rs:213`) is the only place that decides
which of these wins when they overlap. Under all of it the mount table is
parsed three times (`remoteness.rs`, `TrashSites` in `fsops/trash.rs:560`,
UDisks `MountPoints`), with four copies of the octal unescape and two of
`trim_nul`.

The Trash is where this finally cost a feature. The union view let one
read-only NTFS Trash break Empty Trash for everything, and the fix Sprint 32
commits shows only the home Trash. A Trash per root is the design that fixes
it without losing the stick's items.

## Design

### `Location`

```rust
pub enum Location {
    Folder(PathBuf),
    Trash(TrashScope),
}

pub enum TrashScope {
    Home,
    /// The `.Trash-<uid>` at this mount root.
    Mount(PathBuf),
}
```

`DirectorySession` owns a `Location` in place of `current_dir` and the flag.
`NavigationHistory` stores `Location`. `Place` targets a `Location`, and
`PlaceKind` and `trash:///` go. One `start_load(Location, LoadKind)` picks the
lister with a single `match`: `stream_directory` for a folder,
`list_trash_records(scope)` for a Trash.

The 28 sites become a handful of methods: `as_folder() -> Option<&Path>` for
anything that needs a real directory (Paste, New Folder, Open in Terminal,
Move To's destination), `is_mutable()` and `accepts_drops()` for enablement,
`label()` and `crumbs()` for the location bar. `command_enabled` reads those.

`state.conf`, `ShowFolders`, `ShowItems`, and the picker keep speaking in
paths. The picker refuses the Trash in one place (today it is two,
`app/sidebar.rs:540` and `app/picker.rs:153`).

### One mount table

`src/mounts.rs`: a snapshot of `/proc/self/mountinfo` with mount point,
filesystem type, source, and a deepest-match lookup where the last mount on a
point wins. One unescape, one `trim_nul`. `remoteness`, `TrashSites`, and both
stores read it. It is refreshed on the mount events the stores already get
and read off the UI thread, which settles review item 9 (a procfs read on
every navigation).

### What a place is

```rust
pub struct PlaceInfo {
    pub mount: Option<MountRoot>,
    pub locality: Locality,
}

pub struct MountRoot {
    pub kind: MountKind, // Drive(VolumeId) | Share(ShareId)
    pub label: SharedString,
    pub root: PathBuf,
}
```

Resolved once when the location changes, from the mount table and the two
stores, and kept beside the `Location`. Breadcrumbs, the sidebar highlight,
`leave_volume` / `leave_mount`, preview gating, and Properties read it. The
precedence that `mounted_breadcrumbs` decides today is written once, in the
resolver.

`ShareId` is opaque. `desktop::gvfs::Location` is renamed `ShareAddress`, and
it and `MountSpec` stop appearing under `app/`. `app/location.rs`'s
`is_network_uri` goes; `ShareAddress::parse` returning `NotANetworkUri` is the
one scheme check.

### A Trash per root

`TrashRecord` carries its Trash root. The Trash place in the sidebar is
`Trash(Home)`. While a drive is browsed and its `.Trash-<uid>` has anything in
it, the drive's row offers its Trash (context menu, and a crumb-level
affordance), which is `Trash(Mount(root))`. Restore and Empty act on the
records' own root. A read-only Trash lists but reports Empty as unavailable
there, without affecting anyone else's.

`list_trash_records(scope)` reads only the root it is asked for. It stops
going through `trash::os_limited::list()`, which walks every mount's Trash
first, so a stalled NFS mount cannot hang the Trash view. The listing is
Marcel's own `.trashinfo` reader over one directory; the crate stays for
trashing and restoring.

### Around it

- Move To offers mounted drives and connected shares after the bookmarks, and
  its `is_trash()` filter goes, since a `Location::Trash` is not a folder.
- The four `Option<…Menu>` fields on the sidebar become one
  `Option<SidebarMenu>`.

## Order of commits

1. `mounts.rs`, with `remoteness` and `TrashSites` moved onto it. No
   behaviour change.
2. `Location` and `start_load`, with `TrashScope::Home` only. This is the
   commit the compiler checks; it touches every site and stays reviewable on
   its own.
3. `PlaceInfo`, `ShareId`, and the GVfs types out of `app/`.
4. A Trash per root, replacing Sprint 32's stopgap.
5. Move To and the sidebar menu enum.

## Not in scope

- Search. It is one more `Location` variant after this, and it is not a
  cleanup.
- A union Trash view. A Trash per root is enough, and a union is what broke.

## Where it came out differently

- **`PlaceInfo` has no `locality`.** Whether a folder is a network away stays
  in `DirectorySession`, set in `set_location`, because the preview and
  thumbnail gating read it from the session and it needs no store to work
  out. `PlaceInfo` is the drive or share, which does need the stores, and
  lives on the window.
- **The drive's Trash opens from the drive's context menu only.** No crumb
  affordance: the menu is where Unmount and Eject already are, and a crumb
  that appears only when a hidden directory has content would be a new
  feature.
- **`app/` gets share types from `crate::network`**, which re-exports
  `Mount`, `ShareAddress`, `ShareId`, and `UriError` from the GVfs client.
  The types are the client's; what changed is that `app/` no longer builds
  them from mount specs or reads specs at all, and asks the store instead
  (`mount_for(&ShareId)`, `server_containing`, `connected`).
- **Trashing and the delete guard stopped using the crate's listing too.**
  The plan only named the Trash view, but `trash_paths` listed every Trash
  before and after each placement, and `path_overlaps_system_trash`, which
  every permanent delete runs, enumerated every mount's Trash folders. Both
  now look only at the home Trash, the path's own drive, and drives mounted
  below it.
- **The mount table is still read on navigation, on the UI thread.** It is
  one read of `/proc/self/mountinfo`, a kernel-generated file that cannot
  stall, and `remoteness` does it once per `set_location`. Moving it off the
  thread would mean a location that does not know its locality for a frame;
  not worth it for a procfs read. The two stores do not read `mounts.rs`:
  UDisks2 and GVfs report their own mount points, which is what they are for.
- **No `is_mutable` or `accepts_drops` on `Location`.** Every site that
  wanted one of them wanted a folder, and `as_folder()` answers that.
  `TrashScope::Mount` is called `Drive`, which is what the user calls it.
- **Copy Location is not offered in the Trash.** It copied whatever folder
  had been open before, which the old `current_dir` still held.

## Acceptance checks

- [x] `browsing_trash`, `PlaceKind`, and `trash:///` are gone; `rg
      'starts_with' src/app` finds no place-kind checks.
- [x] Back from the Trash returns to the folder open before it, and Forward
      goes back to the Trash. (The history holds `Location`s and a test
      walks it; not yet driven in the running app.)
- [x] Paste, New Folder, and Open in Terminal are disabled in either Trash and
      enabled on a mounted drive and a share. (By construction:
      `can_mutate_here` asks `as_folder()`.)
- [x] Move To lists mounted drives and connected shares and never a Trash.
- [ ] A file trashed on a stick is listed under that drive's Trash after
      Undo is gone, restores from there, and Empty on that Trash empties only
      it. The listing is covered by the `MARCEL_TEST_OTHER_FS` test; the rest
      needs a stick.
- [x] A read-only `.Trash-<uid>` lists and cannot be emptied, and the home
      Trash still empties. (Driven 2026-09-27 on the Windows partition,
      mounted read-only by Fast Startup: it lists, Empty Trash is greyed.
      The home Trash was not emptied, only read.)
- [x] The Trash view opens with a stalled NFS mount present. No fixture: the
      lister no longer reads the mount table or any Trash but the one shown,
      so there is nothing for a stalled mount to hold up.
- [x] `mounts.rs` has one unescape and one deepest-match lookup, covered by
      the Sprint 31 cases plus the stacked-mount case from Sprint 32.
- [x] No `desktop::gvfs` type is named under `src/app/`.
- [x] The picker never shows either Trash, and refuses it in one place
      (`can_visit`).
- [x] `state.conf` and `ShowFolders` behave as before. (Driven 2026-09-27:
      a quit and start left the file byte-for-byte and restored the view;
      `ShowFolders` over the session bus moved the open window.)
- [x] The gate is green, with both bus tests run unsandboxed.
