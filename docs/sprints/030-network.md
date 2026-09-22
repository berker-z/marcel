# Sprint 30: Network

**Status:** Implemented. The gate is green at 422 tests (from 407), with
`private_session_bus_integration` run outside the sandbox, and the mount,
browse, disconnect, and host-key flows checked by hand against a real SFTP
host.

## Goal

Marcel can open a folder on another machine: type an address or click a
saved server, answer whatever the server asks, and browse it like any other
folder. Servers the user cares about are kept in a file and are always in the
sidebar, connected or not. This is the second slice of
[`0xx-every-place-a-file-lives.md`](0xx-every-place-a-file-lives.md), taken
before search and, like drives, without the `Location` refactor: a share
mounted through GVfs is a directory under `/run/user/<uid>/gvfs/`, and a
directory is what every part of Marcel already understands.

## Decisions, and where they come from

**GVfs, not a client of Marcel's own.** GVfs has an SFTP backend that
spawns the real `ssh` (so `~/.ssh/config` aliases, keys, and the agent all
apply), an SMB backend on libsmbclient, DAV and FTP, a keyring for saved
passwords, and `gvfsd-fuse` to make each mount a directory. Reimplementing
any of that would be worse than what every GNOME application already shares.
The cost is a dependency on a session daemon that not every system runs;
Marcel's answer is the same as for UDisks2: without it there is no Network
section and nothing else changes.

**GVfs's own D-Bus protocol, not GIO.** The earlier plan offered the D-Bus
route with `gio mount` as a fallback. The D-Bus route turned out to be small:
`org.gtk.vfs.MountTracker` on `org.gtk.vfs.Daemon` has `ListMounts2`,
`MountLocation`, and `Mounted`/`Unmounted` signals; the mount's own
`org.gtk.vfs.Mount` has `Unmount`. The one piece with any shape to it is the
callback: `MountLocation` takes a `(bus name, object path)` pair naming an
`org.gtk.vfs.MountOperation` the client exports, and the backend calls
`AskPassword`, `AskQuestion`, and `ShowProcesses` on it. Marcel exports one
such object per call, with a channel to whoever puts dialogs on screen
(`network/prompts.rs`, shaped like the conflict dialog). The signatures were
confirmed against the interface strings in `libgvfscommon.so` and then
against the live daemon; `Unmount` takes its source flattened (`sou`) where
`MountLocation` takes it as a struct (`(so)`), which is the kind of thing only
a live call tells you. Linking GIO would have meant a GLib main loop in a
GPUI process for a dozen calls; `gio mount` cannot answer a prompt from a
non-terminal.

**Mapping a URI to a mount spec** follows GVfs's own mappers so a URI
Nautilus accepts lands on the same mount: the generic host/user/port rule for
SFTP and FTP, `client/smburi.c` for the three SMB backends (`smb://` is the
network, `smb://server/` a server, `smb://server/share` a share), and
`client/httpuri.c` for DAV, where the URI path is the mount prefix and the
backend may shorten it once it finds the DAV root. A saved server is matched
to a live mount by spec rather than by string: same backend, every item the
saved spec names present and equal in the mount's, and the mount's prefix
above the saved one. A bare hostname is accepted as SFTP because someone who
types `wired` means `ssh wired`; the file, which Marcel writes, holds URIs
only, so a stray word in it is a rejected line and not a server.

**The servers file** is `~/.config/marcel/servers`, a sibling of `bookmarks`
in the GTK bookmarks shape: a URI, then optionally a space and a name. The
store is the bookmark store's single-writer pattern: loaded once, refuses to
save while loading or when the file held lines it did not understand, and
coalesces saves. Nautilus keeps recent servers in an XBEL file under
`gtk-3.0`; a plain line per server is what the user asked for and is what a
person edits by hand.

**Where a click lands** is the path in the URI, and with none, wherever the
server puts a visitor: GVfs reports a `default_location` per mount, which for
SFTP is the home directory, so `sftp://wired/` opens at `/home/<user>` the
way it does in Nautilus.

**Prompts** are GVfs's text in Marcel's dialogs. A backend's message comes as
a headline, a blank line, and an explanation; the headline is the title. For
a question, choice 0 is what the backend treats as "go ahead" ("Log In
Anyway" before "Cancel Login" in the SFTP backend) and GTK's mount operation
makes it the default and puts it on the right, so Marcel does the same. A
password dialog shows only the fields the backend's flags ask for, offers
"Remember this password" when the backend says saving is supported (GVfs
then writes the keyring itself), and "Connect Anonymously" when the backend
allows it. Closing either dialog answers "aborted", which the backend turns
into a failure Marcel does not report. Choosing "Cancel Login" is a choice,
not an abort, and the "Host key verification failed" that follows is
reported, as Nautilus does.

**Breadcrumbs** on a share or a drive start at the mount ("wired / home /
me") rather than at `/run/user/1000/gvfs/sftp:host=wired`, which is a place
nobody chose. The same crumb rule now applies to drives.

## What is built

### `desktop/gvfs.rs`

`MountSpec` and `Location` (a spec plus a path inside it), with `parse`,
`to_uri`, `label`, and `is_served_by`; `Mount` as the tracker reports it,
with `directory_for(path)` resolving through the FUSE root and the mount
prefix; `GvfsClient` with `connect`, `mounts`, `changed`, `mount`, and
`unmount`; the exported `MountOperation` and the `Prompt` enum it produces.
Two opt-in live tests talk to the real daemon:
`MARCEL_TEST_SERVER=wired cargo test --lib desktop::gvfs::live -- --ignored`
mounts, checks the FUSE directory, and unmounts.

### `network/`

`NetworkStore`, a GPUI global like `VolumeStore` and `BookmarkStore` in one:
the saved list with its file, the live mount list re-read on every tracker
signal, `connect` and `disconnect` with a busy set, and `pending` for
connections nothing lists yet. `prompts.rs` serves a mount call's prompts on
whichever window speaks for the origin.

### Sidebar

A Network section between Devices and Bookmarks: saved servers in file
order, then mounts no saved server accounts for, then "Connecting to X…"
for addresses in flight, then Add…, which opens Connect to Server. A
connected row is a place (click to go, drop files on it) with a disconnect button; a disconnected
saved server is muted and connects on click. Right-click: Disconnect,
Rename…, Remove from Network on a saved server; Disconnect and Add to
Network on a bare mount. The row the window is inside is highlighted, with
the deepest saved path winning when two servers share a mount. Disconnecting
the share being browsed goes Home first, as with a drive.

### Location bar

A URI with a scheme other than `file` connects instead of resolving; a bare
word is still a folder name there, because it usually is one.

### Fixed on the way

**The sidebar folds.** The 900 px minimum window width came from three
minimums added up: sidebar, browser, preview. The sidebar now folds to a
strip on `Ctrl+B` or the button beside the gear, and folds on its own below
900 px without writing the preference, so a drag to half the screen is not
remembered as a choice; the preview pane goes below 640 px of workspace and
keeps its width for when the room returns. The floor drops to 640 px, which
is the browser pane, the strip, and a top bar whose fields have minimums.

**The sidebar footer.** In a short window the Show Hidden switch and the
settings gear painted over the last bookmarks, because the sections above
them never scrolled. The switch is now a `.*` button in the top bar beside
the view toggle (the shell's name for dotfiles, lit with the row's hover
fill while they show), the gear keeps the footer to itself, and everything
above it scrolls as one column.

**Trashing on a share.** Every GVfs share is one FUSE filesystem rooted at
`/run/user/<uid>/gvfs`, and that root refuses a `mkdir`, so no `.Trash-<uid>`
can exist for a file on a share and the `trash` crate failed with a raw
`FileSystem { ... NotFound }`. This is the "delete immediately?" fallback
Sprint 29 deferred, and it stopped being theoretical on the first file
deleted over SFTP. `TrashSites::no_trash_reason` now says so in a sentence,
both inside `trash_paths` and through `trash_unavailable_for`, which the
window runs off the foreground before starting a trash; when it answers, the
window asks "No Trash Here … Delete permanently instead?" and, on yes, runs
the permanent delete that Shift+Delete runs. Nautilus asks the same
question. A read-only stick takes the same path.

`VolumeStore::mount` called its continuation synchronously when the volume
was already mounted, which re-entered the window entity from inside its own
update and panicked. Nothing reached it before, since a mounted drive's row
navigates directly; the network store had the same shape and the location
bar reached it on the first try. Both now defer.

## Not in this sprint

- Watching a FUSE mount. A share does not update as it changes; Refresh does.
  Drives left the same gap for `ntfs-3g`, and one polling change should
  close both.
- Browsing the local network (`smb://`, `network://`): the backends exist and
  `smb://` parses, but the listing GVfs produces for them is not a directory
  through FUSE, so nothing shows. Connect by address.
- Reordering saved servers by drag, as bookmarks do.
- A Nix `settings.network` switch. Adding `gvfs` to Marcel's closure would
  not make the daemon activatable; that is `services.gvfs.enable`, which is
  the user's, and the README says so.
- `Location`, search, the Trash union view.

## Acceptance checks

- [x] With GVfs running, a Network section appears with an Add… row.
      Typing `wired` connects through the SSH agent with no prompt and opens
      the remote home; the breadcrumbs read "wired / home / berkerz".
- [x] Add to Network on the connected row writes `sftp://wired/ wired` to
      `~/.config/marcel/servers`; Rename… changes the name in the file;
      the row survives a disconnect, muted, and reconnects on click.
- [x] `sftp://wired/etc` in the location bar opens that folder on the
      already-connected share without a second mount.
- [x] `sftp://nonexistent.invalid/` reports "Could not connect to
      “nonexistent.invalid”: Hostname not known" and leaves nothing behind.
- [x] `sftp://<ip>/` for a host not in `known_hosts` shows GVfs's identity
      question with Log In Anyway as the primary button; Cancel Login fails
      the mount with GVfs's message, Log In Anyway mounts it and lists it as
      a bare mount with its own disconnect button.
- [ ] A password prompt (an SMB share or a password-only SFTP user), and
      Remember this password reaching the keyring. No such server was at
      hand; the dialog is built from the flags GVfs sends and has not been
      seen live.
- [x] Delete on a file on the share asks "No Trash Here … Delete permanently
      instead?" and, on Delete Permanently, removes it from the server.
- [x] The gate is green; `private_session_bus_integration` passes
      unsandboxed.
