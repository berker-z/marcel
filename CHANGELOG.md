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

List and grid views, breadcrumbs, bookmarks, and the usual XDG places in a
sidebar. `Ctrl+L` to type a path, or start typing to filter the current folder
with fuzzy matching. Marquee and keyboard selection. Folders update as they
change on disk instead of reloading. Comfortable at 50,000 entries.

### Preview

A preview pane that stays open while you browse: text and code, images,
continuously scrolling PDFs, and folder listings. Thumbnails come from the
freedesktop cache, so they are shared with other applications rather than
duplicated.

### File operations

Copy and move with progress and cancellation. When a destination is taken,
Marcel asks whether to replace, rename, skip, or merge, and one answer can
apply to the rest of the operation. Undo and redo cover copy, move, rename,
Trash, restore, archive creation, and extraction. Permanent deletion needs a
confirmation and stays out of undo history. Inline rename, new folders, zip
creation, and extraction of the common free formats.

Marcel checks that files are still what and where it thinks they are before
touching them, and refuses rather than guessing.

### Desktop integration

Bilateral file drag and drop with other applications on Wayland. Registration
as a file manager over D-Bus, so "show in folder" works from elsewhere. One
process per graphical session, with each `marcel` invocation opening its own
window rather than taking over one you were already using.

Installing Marcel does not change your MIME associations and does not claim the
generic `org.freedesktop.FileManager1` name. Both are opt-in.

### Appearance

Several built-in themes. Marcel ships its own icon subset and font and uses
them first, so it looks the same on a bare system, falling back to the system
icon theme only for icons it does not ship.

### Packaging

A Nix flake with a package, an overlay, an app, and NixOS and Home Manager
modules for theme, icon theme, and font. The package installs a desktop entry,
branded icons, AppStream metadata, and a D-Bus service file, and carries a
private free `7zz` for archives. RAR and CBR extraction are off by default
because the decoder is not free.

### Known gaps

No search, no Properties dialog, no New File. Moves between filesystems are
refused rather than silently turned into a copy and a delete. No removable
volumes or remote locations. Sorting is fixed. Dragging files out of Marcel is
not implemented on X11. The full list is in the README.
