# Sprint 34: Network and devices hardening

**Status:** Planned. The last of the three cleanup sprints from
[`../review-2026-09-27.md`](../review-2026-09-27.md). After it, the tree is
where search can start from.

## Goal

GVfs, UDisks2, and the desktop clipboard all talk to processes Marcel does not
control. Sprint 30 made sure none of them can freeze the window, and that
holds: every call is async zbus, and no D-Bus call runs on the UI thread. What
is left is the long tail. Some events get missed, some waits can't be
cancelled, a few things leak when the other side misbehaves, and the two
stores behave differently from each other. None of it is dramatic, and all
of it is what a user with a flaky NAS runs into.

## Work

Numbers are the review's bug numbers.

### Events and lifetimes

- **(8) Subscribe before listing.** The watch loops in `network/mod.rs:198`
  and `volumes.rs:72` list first and add match rules second, and they rebuild
  their streams after every event. Create the signal streams once per client,
  before the first `ListMounts2` / `GetManagedObjects`, and keep them for the
  client's life, as `wait_for_daemon` already does.
- **(17) UDisks reconnects.** `VolumeStore` gets the reconnect loop
  `NetworkStore` has, so a restarted `udisksd` brings the Devices section
  back.
- **(16) The exported `MountOperation` removes itself.** `ExportedOperation`
  becomes a guard that unexports in `Drop`, so an early `?` in
  `unmount_record` (`gvfs.rs:750`) or a dropped mount future cannot leave a
  parked prompt task behind.
- **(10) A connect can be cancelled.** The pending row gets Cancel, which
  drops the task; the guard above answers the backend. A connect with no
  prompt and no reply for 60 seconds reports that it is still waiting,
  instead of saying nothing.

### Shares

- **(9) SMB port and domain.** `smb_location` and `to_uri` (`gvfs.rs:222`,
  `:290`) handle `port` and split `DOMAIN;user` the way GVfs's `smburi.c`
  does.
- **SFTP root.** `sftp://host/` keeps `/` distinct from no path, so it opens
  the server's root as Nautilus does; `sftp://host` still opens the default
  location.
- **`%2F` in a path segment** survives a round trip instead of being split.
- **(18) Leaving a share or drive** waits for the unmount to succeed, then
  moves every window standing on it, not just the one that asked. A Cancel in
  the busy prompt leaves everyone where they were.
- `PasswordReply` gets a `Debug` that prints the password as `<redacted>`.

### Deleting where there is no Trash

- **(12)** When some selected items can be trashed and some cannot, trash
  the ones that can and ask about the rest by name. Today one Trash-less item
  sends the whole selection to permanent deletion
  (`app/edits.rs:104`).

### Clipboard

- **(15)** The pipe reader and writer (`desktop/clipboard.rs:503`, `:435`) use
  one non-blocking fd with `poll()` against a deadline and drop the fd on
  timeout, so a client that never closes its end costs nothing afterwards.
  The reader takes `READ_LIMIT + 1` bytes and rejects an oversized list
  instead of truncating it into a different path. A finished read wakes the
  window so Paste enables.

### Tests and structure

- Tests for `src/volumes.rs` (the store's deferred-continuation logic, which
  panicked once in Sprint 30) and the testable parts of `app/network.rs`.
- Round trips in `gvfs.rs` for SMB port and domain, IPv6 hosts, the SFTP root,
  an unknown backend kind, and `directory_for` with a DAV default location.
- The `dbus-run-session` child-process harness, copied between
  `desktop/bus.rs:789` and `desktop/gvfs.rs:913`, moves into `testing.rs`.
- `render_network_menu` (about 177 lines) splits the way the entry menu did.
  `network/mod.rs:75`'s `.ok().filter(..).ok_or(())` becomes a `match`.

## Not in scope

- Polling FUSE mounts that inotify cannot watch (`ntfs-3g`, GVfs). It is a
  feature, and it stays in the TODO.
- Browsing `smb://` and `network://` without a share name.
- Pasting `sftp://` URIs from other applications.

## Acceptance checks

- [ ] A share mounted, and a stick plugged in, in the window between a list
      call and the subscription is seen (tested by holding the list reply on
      a private bus).
- [ ] Restarting `udisksd` brings the Devices section back without
      restarting Marcel.
- [ ] A connect to a blackholed host can be cancelled, after which a retry
      is accepted.
- [ ] No exported `MountOperation` is left on the bus after a failed unmount
      or a cancelled connect.
- [ ] `smb://DOM;user@nas:4455/share` round-trips through the servers file
      and connects to port 4455.
- [ ] `sftp://host/` opens `/`; `sftp://host` opens the home directory.
- [ ] Ejecting a stick that two windows are browsing moves both, and only
      after the eject succeeds.
- [ ] Deleting a mix of trashable and Trash-less items trashes the first and
      asks about the second by name.
- [ ] A clipboard client that never closes its pipe leaves no thread behind
      after the timeout, and a file list over the limit pastes nothing and
      says why.
- [ ] Copying in Nautilus enables Paste in Marcel without any other input.
- [ ] `src/volumes.rs` has tests; the bus harness is in `testing.rs` and used
      by both bus tests.
- [ ] The gate is green, with both bus tests run unsandboxed.
