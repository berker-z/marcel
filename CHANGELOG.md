# Changelog

Marcel follows [semantic versioning](https://semver.org). Until 1.0 the minor
number carries breaking changes, which for a file manager mostly means changes
to settings, keyboard shortcuts, and the D-Bus surface.

Entries describe what changed for someone using Marcel. The reasoning behind a
change usually lives in the sprint document that produced it, under
[`docs/sprints/`](docs/sprints/).

## 0.1.0

First release. Everything here is new, so rather than list it as changes, here
is what Marcel does at 0.1.0.

### Browsing

List and grid views, sorted by name, modified time, size, or kind, with
breadcrumbs, bookmarks, and the usual XDG places in a sidebar. `Ctrl+L` to
type a path, or start typing to filter the current folder with fuzzy matching.
Marquee and keyboard selection. Folders update as they change on disk instead
of reloading; when a full reload is needed, the listing and the selection stay
put until the new one has arrived, and a folder that churns is reread at most
every 1.5 seconds. Leaving a large folder stops its enumeration. Comfortable
at 50,000 entries.

### Preview

A preview pane that stays open while you browse: text and code, images,
continuously scrolling PDFs, folder listings, audio with a waveform, cover
art, and spectrum bars, and a poster frame with a play button for video.
An audio file shows its tags, cover, and player at once and the waveform
fills in behind them; the waveform is cached under `~/.cache/marcel`, so a
track is measured once. A folder preview lists its first thousand entries.
Decoded images are held to a budget (64 MiB for thumbnails and icons, 128 MiB
for PDF pages) and released with their GPU tiles, so paging through a long
document does not grow the process without limit; an animation past 240
frames or 64 MiB plays what fitted rather than failing.
Thumbnails come from the freedesktop cache, so they are shared with other
applications rather than duplicated; video thumbnails and anything Marcel
cannot decode itself (Opus) need ffmpeg on `PATH`.

### File operations

Copy and move with progress and cancellation. When a destination is taken,
Marcel asks whether to replace, rename, skip, or merge, and one answer can
apply to the rest of the operation. Undo and redo cover copy, move, duplicate,
rename, Trash, restore, archive creation, extraction, and permission changes.
Permanent deletion needs a
confirmation and stays out of undo history. Inline rename, new folders and
files, Duplicate, Move To, zip creation, and extraction of the common free
formats. An extraction whose output name is taken asks the same question a
copy does instead of refusing.

A Properties dialog (`Ctrl+I` or `Alt+Enter`, also in both context menus, and
answering `ShowItemProperties` over D-Bus): kind, location, size, owner,
group, permissions, and timestamps, plus what the preview loaders know about
the file, so image dimensions, PDF page count, text line count, and archive
contents. Folders are measured in the background while the dialog is open.
The permission bits are checkboxes, and ticking one is an undoable `chmod`.

Copy Path puts the selected paths on the clipboard, and Copy Location the
folder's.

Marcel checks that files are still what and where it thinks they are before
touching them, and refuses rather than guessing.
Copies, deletions, and permission changes work through open descriptors, so
an entry swapped for a link or a pipe while an operation runs is refused, not
followed. A copy is fsynced through to the folder it lands in. Archive names
are taken literally (a file called `*.txt` is one file), and a zip or 7z that
contains links, pipes, or devices is refused before anything is written.
Working directories Marcel makes while copying or extracting carry the boot
and process that own them, and one left by a crash is removed the next time
the folder is opened.

If `~/.config/marcel/state.conf` cannot be read, Marcel says so when a window
opens, runs on defaults, and never writes the unreadable file back.

### Desktop integration

Bilateral file drag and drop with other applications on Wayland. Registration
as a file manager over D-Bus, so "show in folder" works from elsewhere. One
process per graphical session, with each `marcel-rs` invocation opening its own
window rather than taking over one you were already using.

`marcel-rs --help` lists the command line; an unknown option is an error rather
than a folder. A start the bus made for an `Activate` or a `ShowFolders` that
never arrives opens a window after three seconds rather than sitting
invisible; a start made for the file-chooser portal, which asks nothing of
its backend until a dialog is wanted, stays quiet. A second `marcel-rs` that cannot reach the
running one within ten seconds opens its own window. Requests over
`FileManager1` and the application interface are rate-limited per sender.

Marcel is also an xdg-desktop-portal `FileChooser` backend. With the portal
variant installed and named in `portals.conf`, the open and save dialogs of
any application that asks the portal for one (most of them, on Wayland) are
Marcel windows: the same browser, preview, bookmarks, and filter, with a bar
for the name field, the application's file-type filter, Cancel, and the accept
button. Single-file pickers hold the selection to one item, save dialogs ask
before replacing, and if Marcel is not running the bus starts it.

Installing Marcel does not change your MIME associations, does not claim the
generic `org.freedesktop.FileManager1` name, and does not become the portal
backend. All three are opt-in.

Open With… asks the desktop's application chooser, Open in Terminal starts a
terminal in the folder (through `xdg-terminal-exec` when it is installed,
handing it the folder only on a release that understands `--dir`), and the Trash is a place in the sidebar where items
show Restore instead of Move to Trash and the empty-space menu offers Empty
Trash.

### Appearance

Several built-in themes, chosen in Settings and remembered. Marcel ships its
own icon subset and font and uses them first, so it looks the same on a bare
system, falling back to the system icon theme only for icons it does not ship.

### Packaging

A Nix flake with a package, an overlay, an app, and NixOS and Home Manager
modules. `programs.marcel.enable` installs Marcel and changes nothing else;
three switches, each off by default, take something over:
`defaultDirectoryHandler` makes Marcel the `inode/directory` handler,
`fileManager1` makes it answer `org.freedesktop.FileManager1` (the Home
Manager module writes the activation file where D-Bus looks first, so Marcel
wins over Nautilus or Dolphin), and `fileChooserPortal` adds the portal
variant to `xdg.portal.extraPortals` and names it for `FileChooser`.

`programs.marcel.settings` covers `theme`, `icon_theme`, `ui_font`, and
`media`, which wraps ffmpeg and ffprobe into Marcel's `PATH` for video posters
and the audio formats Marcel does not decode itself; it is off by default
because ffmpeg's closure is larger than Marcel's. The same `settings` are
available without the module as `pkgs.marcel-rs.withSettings`.

The package installs a desktop entry, branded icons, AppStream metadata, a
D-Bus service file, and the licenses, and carries a private free `7zz` for
archives. RAR and CBR extraction are off by default because the decoder is
not free.

### Known gaps

Marcel needs a window at least 900 pixels wide. Narrower than that its panes are
wider than the window and the preview pane runs off the edge, so the window
refuses to shrink below it on desktops that honour a minimum size.

No search. Moves between filesystems are refused rather than silently turned
into a copy and a delete, and so is moving an item to a Trash on another
filesystem, since the copy would lose its extended attributes and its undo. No removable volumes or remote locations. The file
clipboard is Marcel's own, so Ctrl+C here followed by Ctrl+V in another
application does nothing; drag and drop is the way across. Undo history lasts
for the session. Dragging files out of Marcel is not implemented on X11. The
full list is in the README.
