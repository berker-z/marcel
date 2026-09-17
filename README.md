# Marcel

A fast graphical file manager for Linux, written in Rust. The preview pane stays open and useful as you browse, and file operations are careful about touching your files. Marcel does not depend on GTK or Qt.

Marcel renders its interface with [GPUI](https://github.com/zed-industries/zed), the UI framework behind Zed. It does not pull in a desktop toolkit or inherit its theme and startup cost. Select a file and you see it straight away: text, images, PDFs, or the contents of a folder.

Marcel borrows heavily from [Yazi](https://github.com/sxyazi/yazi). The filesystem layer, incremental directory updates, preview scheduling, and copy semantics all grew from reading Yazi's source. Marcel is a graphical application rather than a terminal one, so the interface is its own, but much of the machinery underneath owes Yazi a lot. [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) records what was adapted, file by file, down to the upstream commit.

## Status

Alpha. I use Marcel as my daily file manager, and it has been through several rounds of external review focused on filesystem safety. Nobody else has used it much yet.

Copy, move, rename, Trash, restore, archive creation, and extraction can all be undone. Marcel checks that files are still what and where it thinks they are before touching them, and refuses rather than guessing. Permanent deletion is the exception.

Linux only. Wayland is the tested target. X11 mostly works, but dragging files out of Marcel into other applications is not implemented there.

![Marcel in grid view, with a folder of two files on the left and a PDF open in the preview pane on the right, showing the cover and first page of text](docs/screenshots/marcel.png)

## What it does

### Browsing

List and grid views, breadcrumbs, bookmarks, and the usual XDG places in a sidebar. `Ctrl+L` to type a path, or just start typing to filter the folder with fuzzy matching. Sort by name, modified time, size, or kind from the button next to the location bar, or by clicking a column heading in list view; folders stay first. Folders update as they change on disk instead of reloading, and 50,000 entries remain comfortable.

### Preview

Text and code, images, continuously scrolling PDFs, and folder listings. Thumbnails come from the freedesktop cache, shared with other applications rather than generated again.

### File operations

Copy and move with progress and cancellation. When a destination is taken, Marcel asks whether to replace, rename, skip, or merge, and one answer can apply to the rest of the operation. Undo and redo cover copy, move, duplicate, rename, Trash, restore, archive creation, extraction, and permission changes. Permanent deletion requires confirmation and stays out of undo history. New folders and files, zip creation, extraction of the common free formats, Move To… through a small folder dialog, and Copy Path.

Properties (`Ctrl+I`, or `Alt+Enter` if your hands know KDE) shows what an item is, where it lives, who owns it, and its permissions and timestamps, plus what the preview loaders know: image dimensions, PDF page count, text line count, archive contents. Folders are measured in the background while the dialog is open. The permission bits are checkboxes, and ticking one is a `chmod` that undoes like everything else.

### Desktop integration

On Wayland, files drag to and from other applications. Marcel registers as a file manager over D-Bus, so "show in folder" from other applications works. One process per session; each `marcel-rs` invocation opens a new window rather than taking over one you were using.

Marcel can also be the file dialog. Applications on a modern Linux desktop ask xdg-desktop-portal for open and save dialogs, and the portal hands the job to a backend. Marcel implements that backend, so with it enabled the dialog from a browser, an editor, or a chat client is a Marcel window: the same browser, preview, bookmarks, and filter, plus a bar with the name field, the file-type filter the application asked for, Cancel, and the accept button. If Marcel is not running, D-Bus starts it. How to turn it on is under Installing.

### Appearance

Several themes, picked in Settings and remembered. Marcel ships its own icons and font, so it looks right on a bare system; missing icons fall back to the system icon theme, and an explicit icon theme setting overrides both.

## Keyboard shortcuts

| Key                                              | Action               |
| ------------------------------------------------ | -------------------- |
| Arrow keys                                       | Move selection       |
| Home / End                                       | First / last item    |
| Page Up / Page Down                              | Move a page          |
| Enter                                            | Open                 |
| Escape                                           | Clear selection      |
| Ctrl+Up                                          | Parent folder        |
| Ctrl+Left / Ctrl+Right                           | Back / forward       |
| Ctrl+L                                           | Edit the location    |
| Ctrl+F                                           | Focus the filter     |
| Any character                                    | Start filtering      |
| Shift with arrows, Home, End, Page Up, Page Down | Extend the selection |
| Ctrl+A                                           | Select all           |
| Ctrl+C / Ctrl+X / Ctrl+V                         | Copy / cut / paste   |
| Delete                                           | Move to Trash        |
| Shift+Delete                                     | Delete permanently   |
| Ctrl+Shift+N                                     | New folder           |
| Ctrl+D                                           | Duplicate            |
| F2                                               | Rename               |
| Ctrl+I or Alt+Enter                              | Properties           |
| Ctrl+Z / Ctrl+Y                                  | Undo / redo          |

## What it does not do

Known gaps, roughly in the order they are likely to be addressed:

* No search. You can filter the folder you are in, but there is no recursive search by name or content.
* Moving between filesystems is refused rather than quietly turned into a copy and a delete. Copying across drives works.
* No removable volumes, network shares, or remote locations. Local paths only.
* No media playback, and no thumbnails for video.
* The window needs to be at least 900 pixels wide; below that the preview pane runs off the edge. Marcel asks the desktop not to shrink it further, and a tiling compositor can insist anyway.
* Keyboard and accessibility coverage is incomplete. Some things are reachable only with a pointer.
* RAR extraction needs a separate build. The default package ships only free components.
* Nix is the only packaging route today. One file operation runs at a time; a second is refused until the first finishes.

Not planned: tabs, as I do not like them very much.

## Installing

Marcel ships as a Nix flake.

Run it without installing anything:

```sh
nix run github:berker-z/marcel -- ~/Downloads
```

For a persistent installation, add Marcel as a flake input and import its
Home Manager module:

```nix
{
  inputs.marcel.url = "github:berker-z/marcel";
}
```

```nix
{
  imports = [inputs.marcel.homeManagerModules.default];

  programs.marcel = {
    enable = true;
    defaultDirectoryHandler = true;
    fileManager1 = true;
    fileChooserPortal = true;
    settings.theme = "nord";
  };
}
```

Do not add `inputs.nixpkgs.follows` to the input. Marcel pins its own
nixpkgs, and the binary cache holds builds against that pin; following your
nixpkgs produces a different derivation that has to be compiled locally.

`enable` alone installs Marcel and changes nothing else about the desktop.
The three flags are the integration you actually want from a file manager,
and each is off by default because it takes something over:

- `defaultDirectoryHandler` makes Marcel the `inode/directory` handler, so
  `xdg-open` on a folder opens Marcel.
- `fileManager1` makes Marcel answer `org.freedesktop.FileManager1` on the
  session bus, the call behind "show in folder" in browsers and most other
  applications. The module writes an activation file to
  `~/.local/share/dbus-1/services`, which D-Bus reads before any package's,
  so Marcel wins even with Nautilus or Dolphin installed.
- `fileChooserPortal` makes Marcel the open and save dialog of every
  application that asks xdg-desktop-portal for one, which on Wayland is most
  of them. It adds the portal variant to `xdg.portal.extraPortals` and names
  `marcel` for the FileChooser interface; `xdg.portal.enable` is still yours
  to set. After the rebuild, `systemctl --user restart xdg-desktop-portal`,
  since the frontend reads `portals.conf` once at startup. Firefox and Zen
  need `widget.use-xdg-desktop-portal.file-picker` set to `1` in
  `about:config`. Details in [`docs/file-chooser-portal.md`](docs/file-chooser-portal.md).

There is a NixOS module with the same options
(`inputs.marcel.nixosModules.default`; packages land in
`environment.systemPackages`). It has nowhere per-user to put the D-Bus
override, so with a second file manager on the system, which one D-Bus starts
for `FileManager1` falls back to profile order. Use the Home Manager module
when that matters.

The command is `marcel-rs`, not `marcel`: nixpkgs already has a `marcel` (an unrelated Python shell) and two packages installing the same `bin/marcel` collide in a profile. Only the command carries the suffix; the application, icon, desktop entry, D-Bus name, and `~/.config/marcel` are all still Marcel.

Without the module, `overlays.default` provides `pkgs.marcel-rs`,
`packages.<system>.file-manager1-service` is the variant that claims the
`FileManager1` name, and `packages.<system>.file-chooser-portal` is the one
that answers file dialogs (it goes in `xdg.portal.extraPortals`, with
`xdg.portal.config.common."org.freedesktop.impl.portal.FileChooser" = ["marcel"]`).
Installing any of them changes no MIME associations; the details are in
[`docs/release.md`](docs/release.md).

## Declarative settings

`settings` covers theme, icon theme, and font:

```nix
programs.marcel.settings = {
  theme = "tokyo-night";
  icon_theme = null;
  ui_font = null;
};
```

Leaving `icon_theme` and `ui_font` as `null` keeps Marcel's bundled icons and font, which is the default.

View mode, sort order, and hidden files are interaction state, not Nix options; Marcel remembers them in `$XDG_CONFIG_HOME/marcel/state.conf`. The theme is both: `settings.theme` is the default, and a theme picked in Settings is written to the same file and wins from then on. Delete its `theme=` line to follow the Nix option again.

## Building

```sh
nix develop
cargo run
```

The development shell is required: a plain shell will not find the system libraries the build needs, and a different `cargo` picks a different compiler and invalidates everything compiled in `target/`. The shell pins its compiler through `flake.lock`. Avoid `cargo clean`; the dependency build is long.

`nix build .#marcel-rs` builds the release package in isolation, using Crane so dependencies survive application-only edits. To keep those dependencies across garbage collection:

```sh
nix build --accept-flake-config --max-jobs 1 --cores 2 .#marcel-deps --out-link .marcel-deps
nix build --accept-flake-config --max-jobs 1 --cores 2 .#marcel-rs
```

The cache workflow runs on every push to `master` and on tags, so a commit is cached a few minutes after it lands. The cache holds builds against Marcel's own locked nixpkgs; consume `packages.<system>.marcel-rs` from this flake rather than applying the overlay to another nixpkgs, or you compile GPUI yourself.

## Credits

Built with [GPUI](https://github.com/zed-industries/zed) (Apache-2.0) and [gpui-component](https://github.com/longbridge/gpui-component). PDF rendering goes through Poppler, archives through 7-Zip. Icons are a small subset of [Nordzy](https://github.com/alvatip/Nordzy-icon) (GPL-3.0) and the bundled font is a subset of [Iosevka](https://github.com/be5invis/Iosevka) (SIL OFL).

And Yazi, again, for most of the thinking underneath.

## License

MIT, see [`LICENSE`](LICENSE).

Bundled assets keep their own licenses, which are not MIT. The icon set is GPL-3.0 and the font is under the SIL Open Font License. Full details, along with the record of adapted code, are in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
