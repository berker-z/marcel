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

List and grid views, breadcrumbs, bookmarks, and the usual XDG places in a sidebar. The sidebar folds away (`Ctrl+B`, or the leftmost button of the top bar) and folds on its own when the window is too narrow for it, as does the preview pane below that; both come back as the window grows. `Ctrl+L` to type a path, or just start typing to filter the folder with fuzzy matching. Sort by name, modified time, size, or kind from the button next to the location bar, or by clicking a column heading in list view; folders stay first. Folders update as they change on disk instead of reloading, and 50,000 entries remain comfortable. Refresh, in the right-click menu on empty space, is there for the filesystems that do not send change notifications.

The Trash is the last place in the sidebar. It shows what is in every freedesktop Trash Marcel can find, previews it, and offers Restore where the item menu would otherwise say Move to Trash; Empty Trash is in the empty-space menu there, behind the same confirmation as permanent deletion.

### Preview

Text and code, images (HEIC and AVIF included), continuously scrolling PDFs, and folder listings. Thumbnails come from the freedesktop cache, shared with other applications rather than generated again.

Audio plays in the pane: cover art and tags, a waveform you can click to seek, and spectrum bars that move with the sound. MP3, FLAC, Ogg Vorbis, WAV, AAC, and ALAC decode in-process. Video gets a poster frame, its duration, size, and codecs, a play button that hands the file to your video player, and a thumbnail in the grid. Video, and any audio Marcel cannot decode itself (Opus voice notes, mostly), need `ffprobe` and `ffmpeg` on `PATH`; they are not bundled because ffmpeg is a 300 MiB closure, bigger than Marcel. The Nix module has `settings.media = true` to put them there.

### File operations

Copy and move with progress and cancellation. When a destination is taken, Marcel asks whether to replace, rename, skip, or merge, and one answer can apply to the rest of the operation. Undo and redo cover copy, move, duplicate, rename, Trash, restore, archive creation, extraction, and permission changes. Permanent deletion requires confirmation and stays out of undo history. New folders and files, zip creation, extraction of the common free formats, Move To… through a small folder dialog, and Copy Path. Copy and Cut share the desktop clipboard, so files cross to Nautilus, Dolphin, a terminal, or a chat client and back.

Properties (`Ctrl+I`, or `Alt+Enter` if your hands know KDE) shows what an item is, where it lives, who owns it, and its permissions and timestamps, plus what the preview loaders know: image dimensions, PDF page count, text line count, archive contents. Folders are measured in the background while the dialog is open. The permission bits are checkboxes, and ticking one is a `chmod` that undoes like everything else.

### Drives and network

The sidebar lists the drives UDisks2 knows about, using the same rules Nautilus uses for what is worth showing: a USB stick, an SD card, the Windows partition of a dual-boot machine, but not the EFI partition or `/boot`. Click an unmounted drive to mount it and go there; a stick gets an eject button that unmounts it and powers it off. Moving a folder to a drive on another filesystem copies it, checks the copy, and removes the original, as one operation that undoes as one. What FAT or NTFS cannot keep (permissions, extended attributes, symbolic links) is dropped and reported once. A Windows partition that asks for a password on mount wants an fstab entry with `x-gvfs-show` in its options; Marcel then lists it by its `x-gvfs-name`.

Network shares go through GVfs, so Marcel has no SFTP or SMB client of its own and the desktop's keyring, host-key prompts, and backends are the ones you already have. Add… under Network opens Connect to Server, which takes `sftp://`, `smb://`, `ftp://`, or `dav://` addresses (a bare hostname means SFTP, and an `ssh` config alias works there), and the location bar takes the same. A connected share is a folder under `/run/user/<uid>/gvfs/`, so everything else, copying, moving, previewing, works on it unchanged. There is no Trash on a share, so deleting there asks whether to delete permanently instead. Right-click a connected share and choose Add to Network to keep it; saved servers live in `~/.config/marcel/servers`, one URI per line with an optional name after it, and stay in the sidebar whether or not they are connected. This needs `services.gvfs.enable = true` on NixOS (any GNOME-adjacent desktop has it already); without GVfs on the session bus the Network section is absent. Folders on a share do not update on their own, since inotify does not reach through FUSE; Refresh does.

### Desktop integration

On Wayland, files drag to and from other applications. Marcel registers as a file manager over D-Bus, so "show in folder" from other applications works. One process per session; each `marcel-rs` invocation opens a new window rather than taking over one you were using. Enter opens a file with its default application; Open With… in the item menu asks the desktop's application chooser instead. Open in Terminal, in the empty-space menu, starts a terminal in the current folder, trying `xdg-terminal-exec`, then `$TERMINAL`, then the usual emulators.

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
| Escape                                           | Dismiss the frontmost thing, see below |
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
| Ctrl+B                                           | Fold or unfold the sidebar |

Escape has a stack of meanings and takes the topmost one: it closes an open context menu, then cancels a running file operation (from the window that started it), then clears the filter, then cancels a rename or location edit in progress, then closes a file-chooser dialog, and only when none of those apply does it clear the selection.

## What it does not do

Known gaps, roughly in the order they are likely to be addressed:

* No search. You can filter the folder you are in, but there is no recursive search by name or content.
* A share does not update as files change on it; Refresh reloads. Neither does an `ntfs-3g` mount, since both are FUSE.
* No LUKS, MTP, or optical drives in the sidebar, and no SMB browsing of the local network; connect to a share by address.
* Copy and Cut reach other applications through the Wayland data-control protocol, which Hyprland, Sway, and KDE offer and GNOME does not. On GNOME or under X11 the file clipboard is Marcel's own: Ctrl+C here and Ctrl+V in Nautilus does nothing. Anywhere, files another program copied from a network location it mounted itself (an `sftp://` URI from Nautilus) are skipped, because Marcel only pastes local paths.
* Undo history lives in the running process. Close Marcel and the operations it could have undone are just history.
* No dual pane. Open a second window (Open in New Window on a folder, or run `marcel-rs` again) and drag between the two.
* No video playback; the preview pane shows a frame and a play button for your player.
* Create Link is in the menu, greyed out, until it exists.
* Some conventional shortcuts are not bound: `Ctrl+H` for hidden files (it is the `.*` button next to the view toggle, and in the empty-space menu), `F5` for refresh, `Alt+Up` and `Alt+Left` for parent and back (Marcel uses `Ctrl+Up` and `Ctrl+Left`), and `Ctrl+Shift+Z` for redo (`Ctrl+Y`).
* Keyboard and accessibility coverage is incomplete. Some things are reachable only with a pointer, and screen readers see nothing.
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

`settings` covers theme, icon theme, font, and whether ffmpeg is wrapped in:

```nix
programs.marcel.settings = {
  theme = "tokyo-night";
  icon_theme = null;
  ui_font = null;
  media = false;
};
```

Leaving `icon_theme` and `ui_font` as `null` keeps Marcel's bundled icons and font, which is the default.

View mode, sort order, and hidden files are interaction state, not Nix options; Marcel remembers them in `$XDG_CONFIG_HOME/marcel/state.conf`. The theme is both: `settings.theme` is the default, and a theme picked in Settings is written to the same file and wins from then on. Delete its `theme=` line to follow the Nix option again.

Under the module, these settings are environment variables on a wrapper: `MARCEL_THEME`, `MARCEL_ICON_THEME`, `MARCEL_FONT_FAMILY`, and `PATH` for `media`. Without Nix you can set them yourself, along with `MARCEL_7ZZ` to point at a 7-Zip, `MARCEL_ENABLE_RAR=1` if that 7-Zip can read RAR, and `MARCEL_CLAIM_FILE_MANAGER1` / `MARCEL_CLAIM_FILE_CHOOSER` to take the D-Bus names the two package variants take. The full list, with what each one does, is in [`docs/release.md`](docs/release.md#runtime-environment-variables).

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
