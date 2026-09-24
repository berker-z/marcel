# Sprint 31: What a share costs

**Status:** Implemented. The gate is green at 435 tests (from 423), with
`private_session_bus_integration` run outside the sandbox.

## Goal

Sprint 30 made a network share browsable by making it a directory. That was
the right call, and it came with a bill nobody had looked at: every part of
Marcel that treats a directory as free now treats a server on the other end
of an SSH connection as free too. Scroll a folder of photos on a share and
the grid downloads each one to make a 128×128 thumbnail. Arrow through it and
the preview pane downloads whichever file you are passing over. Open
Properties on a folder and Marcel walks the tree, up to five million entries,
across the network.

None of that was deliberate. It is what happens when the only thing a call
site knows about a path is that it is a path.

## Where this came from

A `gvfsd-fuse` on the maintainer's machine reached 1.9 GB of resident
anonymous memory over five days. Nothing that ran during the investigation
reproduced the growth: sequential `stat`s, sixteen-way parallel `stat`s, a
44 MB read, thirty-two concurrent reads, and a 40,000-path walk all left it
flat, and 40,000 paths cost about 1.3 MB, which does not extrapolate to
1.9 GB by any route a person browsing files would take. The memory was 76
glibc malloc arenas, near the 96-arena cap for twelve cores, and restarting
the daemon returned all of it: 1,946,312 KB to 7,716 KB, 76 arenas to 5.

So Marcel was never shown to have caused it, and this sprint does not claim
it did. What the investigation did turn up is the list above, which is a
latency problem whether or not it is ever a memory one. A file manager that
downloads a 100 MB video because its thumbnail scrolled into view is broken
on a slow link regardless of what the FUSE bridge does with its heap.

## Decisions, and where they come from

**Nautilus's `SpeedTradeoff`, including its defaults.** Nautilus has had this
problem for twenty years and settled it with a three-valued setting:
`always`, `local-only`, `never`. Three keys use it in
`org.gnome.nautilus.gschema.xml`, all defaulting to `local-only`:
`show-image-thumbnails` (whose description notes it "applies to any
previewable file type"), `show-directory-item-counts`, and
`recursive-search`. There is also `thumbnail-limit`, 50 MB, past which an
image is not thumbnailed at all. Marcel now has `thumbnails`, `folder_sizes`,
and `thumbnail_limit_mb` in `state.conf`, with the same defaults, so a share
behaves the way someone coming from GNOME already expects. Copying the
defaults matters more than copying the mechanism: the reason to pick
`local-only` is that the same feature is free on one filesystem and a
download on another, and which one you want cannot be worked out from the
feature.

**`/proc/self/mountinfo`, not `statfs`.** The obvious way to ask whether a
path is remote is `statfs` and a table of magic numbers. It is the wrong way
twice. `FUSE_SUPER_MAGIC` is the same for `gvfsd-fuse`, `sshfs`, and a local
`gocryptfs`, so a magic number cannot tell an SFTP share from an encrypted
directory on the SSD, and treating every FUSE filesystem as remote would
quietly stop thumbnailing local ones. And `statfs` on a FUSE path is a round
trip to the daemon that owns it, which is the cost this module exists to
avoid. `mountinfo` gives the type by name (`fuse.gvfsd-fuse`, `nfs4`,
`cifs`), from a file the kernel generates, so `browse/remoteness.rs` matches
on names and keeps a short list of remote FUSE subtypes. Unknown FUSE
subtypes read as local, which is the safe direction: guessing wrong there
costs a slow preview, and guessing wrong the other way is a file manager that
mysteriously shows no thumbnails.

**One answer per directory, not one per file.** `DirectorySession` decides
when the window navigates and everything downstream reads
`directory.locality`. A grid of 500 entries would otherwise ask the same
question 500 times, and the answer is a property of the folder.

**A cached thumbnail is still shown on a share.** Refusing to *make* one is
not the same as refusing to show one. `thumbnails::load_cached` does the
freedesktop cache lookup using the size and mtime the listing already holds,
so it costs one local `open` of the cache PNG and nothing over the network. A
folder Nautilus has already visited keeps its thumbnails. This is the
difference between the setting being a performance fix and being a visible
downgrade.

**The preview pane waits instead of being switched off.** The preview pane is
the thing Marcel is for, so gating it behind `local-only` was not an option.
The actual problem was that `start_preview` fired on every selection change
with no debounce, and cancelling does not help: a read already handed to the
kernel is paid for whether or not anyone still wants it. On a share the read
now waits 250 ms to see whether the selection is settling, and replacing the
task drops the timer with it, so a read that never started costs nothing.
Locally nothing changed, because locally there is nothing to wait for.

**Properties offers the walk rather than starting it.** A folder on a share
shows a Calculate button and the note that counting means reading all of it.
`describe_totals` now takes whether a walk is running, because "counting"
and "never counted" are the same empty totals and must not read the same:
one resolves on its own and the other never will.

## Surviving a GVfs restart

Restarting `gvfs-daemon.service` during the investigation turned up a
separate bug, and a worse one. Disconnecting a share afterwards failed with
"Could not disconnect “wired”: The name is not activatable".

A `Mount` records the unique bus name of the backend serving it,
`:1.227769`. Restart the daemon and every one of those names dies with it, so
`Unmount` against a record listed beforehand goes to a peer that is not
there. The error says nothing about whether the share is still mounted, and
there was nothing the user could do with it.

Marcel never noticed the daemon had been replaced. The watch loop only
re-listed when the tracker sent `Mounted` or `Unmounted`, which a dead daemon
does not send, and if the signal stream itself failed the loop cleared the
client and returned for good: the Network section would disappear until
Marcel was restarted. Nothing in GVfs's own protocol announces this, so the
bus has to, through `NameOwnerChanged` on `org.gtk.vfs.Daemon`. `changed`
now reports which of the two happened, and a replaced daemon makes the store
drop the client, its records, and its subscriptions, and start over.

Reconnecting waits for a daemon rather than starting one. `connect` activates
GVfs, which is right when a file manager opens and wrong every two seconds
afterwards: a user who stopped GVfs on purpose would have Marcel start it
again. `wait_for_daemon` subscribes, then checks `NameHasOwner`, in that
order, because the other order misses a daemon that appears in between.

`unmount` heals a stale record instead of reporting it. A call that comes
back `ServiceUnknown` or `NameHasNoOwner` never reached a GVfs, so the list is
re-read and the answer comes from what is mounted now: the current record is
disconnected, or nothing serves the spec any more and it already is.
Cancellation is checked first, since the user cannot have answered a prompt
unless a backend was there to ask it.

## Not in this sprint

- Any UI for the three settings. They are read from `state.conf` and written
  back, but nothing in the window changes them yet. Nautilus puts them in
  Preferences; Marcel has no equivalent surface.
- A thumbnail marked `Failed` on a share stays failed for that listing even
  if the setting changes, since the setting can only change by editing the
  file and reopening.
- `recursive-search`, Nautilus's third `SpeedTradeoff` key. Marcel's search
  filters the listing already in memory and never walks, so there is nothing
  to gate.
- The folder hover preview, which is one `readdir` and costs what navigating
  into the folder would cost anyway.
- Proving the `gvfsd-fuse` growth. It needs a fresh daemon watched from zero
  while a share is browsed, and the daemon has been restarted, so the next
  occurrence is the one to measure.

## Acceptance checks

- [x] `browse/remoteness.rs` classifies a real `mountinfo`: `/` as ext4 local,
      `/run/user/1000/gvfs` as `fuse.gvfsd-fuse` remote, `/run/user/1000/doc`
      as `fuse.portal` local.
- [x] A mount point nested inside a local one wins on depth, so a share under
      the home directory is not read as local.
- [x] A mount point containing a space (`\040` in `mountinfo`) still matches.
- [x] A `state.conf` written before these keys existed loads with
      `local-only`, `local-only`, and 50; an unparseable value for one falls
      back instead of failing the load.
- [x] On a private bus with no GVfs, `wait_for_daemon` is still waiting until
      something takes `org.gtk.vfs.Daemon`, and returns when it does. Against
      the real bus, a daemon already running ends the wait without a signal.
- [x] `ServiceUnknown` and `NameHasNoOwner` are told apart from a backend
      refusing an unmount, and cancelling is never read as a stale record.
- [x] The gate is green; `private_session_bus_integration` passes
      unsandboxed.
- [ ] Browsing a real share with the defaults: icons instead of downloads in
      the grid, the preview pane reading only where the selection stops, and
      Properties showing Calculate. Not yet seen in the running application.
- [ ] Disconnecting a share in a Marcel that was running across a
      `systemctl --user restart gvfs-daemon`. The bug is understood and the
      parts are tested, but the whole path has only been reasoned through.
