# Marcel

A graphical file manager for Linux, written in Rust. Fast, built around a preview pane that is genuinely useful, and careful about touching your files. It does not depend on GTK or Qt.

Marcel renders its own interface through GPUI (https://github.com/zed-industries/zed), the UI framework behind Zed, so it doesn't pull in a desktop toolkit or inherit its theming and startup cost. Select a file and you see it straight away: text, images, PDFs, or the contents of a folder.

It borrows heavily from Yazi (https://github.com/sxyazi/yazi). The filesystem layer, the incremental directory updates, the preview scheduling, and the copy semantics are all built on ideas taken from reading Yazi's source. Marcel is a graphical application rather than a terminal one, so the interface is its own, but the parts underneath owe Yazi a lot. `THIRD_PARTY_NOTICES.md` records what was adapted, file by file, down to the upstream commit.

## Status

Alpha. I use Marcel as my daily file manager, and it has been through two rounds of external review focused on filesystem safety. It has not been used widely by anyone else yet.

Copy, move, rename, Trash, restore, archive creation, and extraction can all be undone. Marcel checks that files are still what and where it thinks they are before touching them, and refuses rather than guessing. Permanent deletion is the exception.

Back up anything you would be upset to lose.

Linux only. Wayland is the tested target. X11 mostly works, but dragging files out of Marcel into other applications is not implemented there.

<!-- screenshot goes here -->

## What it does

### Browsing

List and grid views. Breadcrumbs, plus `Ctrl+L` to type a path. Start typing to filter the current folder with fuzzy matching. Marquee and keyboard selection. Bookmarks and the usual XDG places in a sidebar. Folders update as they change on disk instead of reloading. Comfortable at 50,000 entries.

### Preview

Text and code files, images, PDFs with continuous scrolling, and a listing of what a folder holds. Thumbnails use the freedesktop cache, so they are shared with other applications rather than duplicated.

### File operations

Copy and move with progress and cancellation. When a destination is taken, Marcel asks: replace, rename, skip, or merge the two folders, and you can answer once for the rest of the operation. Undo and redo. Move to Trash and restore. Permanent deletion behind a confirmation, kept out of undo history. Inline rename. Create folders. Create zip archives, extract most common formats.

### Desktop integration

Drag files to and from other applications on Wayland. Registers as a file manager over D-Bus, so "show in folder" from other applications works. One Marcel process per session, and running `marcel` again opens a new window rather than taking over the one you were using.

### Appearance

Several built-in themes. Marcel ships its own icons and font and uses them first, so it looks the same on a bare system. It falls back to your system icon theme only for icons it doesn't ship, and an explicit theme setting overrides both.

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
| any character                                    | Start filtering      |
| Shift with arrows, Home, End, Page Up, Page Down | Extend the selection |
| Ctrl+A                                           | Select all           |
| Ctrl+C / Ctrl+X / Ctrl+V                         | Copy / cut / paste   |
| Delete                                           | Move to Trash        |
| Shift+Delete                                     | Delete permanently   |
| Ctrl+Shift+N                                     | New folder           |
| F2                                               | Rename               |
| Ctrl+Z / Ctrl+Y                                  | Undo / redo          |

## What it does not do

Known gaps, roughly in the order they are likely to be addressed:

* No search. You can filter the folder you are in, but there is no recursive search by name or content.
* No Properties dialog and no New File. Both are planned.
* Moving between filesystems is refused. Marcel will not quietly turn a move across drives into a copy followed by a delete. It says it cannot do it. Copying across drives works.
* No Duplicate or Move To.
* No removable volumes, network shares, or remote locations. Local paths only.
* No media playback, and no thumbnails for video.
* Sorting is fixed, and preferences other than view mode and hidden files are not persisted.
* Keyboard and accessibility coverage is incomplete. Some things are reachable only with a pointer.
* RAR extraction needs a separate build. The default package ships only free components.
* No Flatpak. Nix is the only packaging route today.

Not planned: tabs, as I do not like them very much.

## Installing

Marcel ships as a Nix flake.

Run it without installing anything:

```sh
nix run github:berker-z/marcel -- ~/Downloads
```

To install it properly, add Marcel to your system flake:

```nix
{
  inputs.marcel = {
    url = "github:berker-z/marcel";
    inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

Then apply its overlay and install the package:

```nix
{
  nixpkgs.overlays = [inputs.marcel.overlays.default];
  environment.systemPackages = [pkgs.marcel];
}
```

Installing Marcel does not change your MIME associations and does not take over the generic file manager registration on D-Bus. Both are opt-in, and are covered in `docs/release.md`.

## Declarative settings

The flake exports NixOS and Home Manager modules for theme, icon theme, and font:

```nix
{
  imports = [inputs.marcel.nixosModules.default];

  programs.marcel = {
    enable = true;
    settings = {
      theme = "tokyo-night";
      icon_theme = null;
      ui_font = null;
    };
  };
}
```

`imports = [inputs.marcel.homeManagerModules.default]` gives the same options per user. Leaving `icon_theme` and `ui_font` as `null` keeps Marcel's bundled icons and font, which is the default.

View mode and hidden file visibility are deliberately not Nix options. Marcel treats them as interaction state and remembers what you last chose in `$XDG_CONFIG_HOME/marcel/state.conf`.

## Building

```sh
nix develop
cargo run
```

The development shell is required. A plain shell will not find the system libraries the build needs.

## Credits

Built with GPUI (https://github.com/zed-industries/zed) (Apache-2.0) and gpui-component (https://github.com/longbridge/gpui-component). PDF rendering goes through Poppler, archives through 7-Zip. Icons are a small subset of Nordzy (https://github.com/alvatip/Nordzy-icon) (GPL-3.0) and the bundled font is a subset of Iosevka (https://github.com/be5invis/Iosevka) (SIL OFL).

And Yazi, again, for most of the thinking underneath.

## License

MIT, see `LICENSE`.

Bundled assets keep their own licenses, which are not MIT. The icon set is GPL-3.0 and the font is under the SIL Open Font License. Full details, along with the record of adapted code, are in `THIRD_PARTY_NOTICES.md`.
