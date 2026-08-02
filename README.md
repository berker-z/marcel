# Marcel

Marcel is a fast, preview-first graphical file explorer built with Rust,
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), and
[gpui-component](https://github.com/longbridge/gpui-component).

It is a conventional, pointer-friendly file manager—not a terminal file
manager transplanted into a window. Its defining interface is a persistent,
responsive preview pane alongside list and icon views.

## Alpha status

Marcel is a working Linux-first **pre-release alpha**. It already performs real
filesystem mutations, including copy, move, Trash, restore, and permanent
deletion. Use disposable data while evaluating it and keep backups of anything
important.

The Nix flake provides an installable package, application, and downstream
overlay, and that package has been tested in a real NixOS configuration.
Marcel does not yet publish stable release artifacts or promise configuration
compatibility between alpha versions.

### Browsing and interaction

- Asynchronous, incremental directory enumeration with virtualized list and
  icon views.
- Native directory watching with coalesced incremental updates, polling
  fallback, and stale-generation protection.
- Incremental reconciliation after Marcel-owned file operations, preserving
  the active directory session and scroll position instead of flashing through
  a full reload.
- Conventional click, Control-click, Shift-click, keyboard, and empty-space
  marquee selection.
- Back, forward, parent, Places, XDG user directories, and navigation history.
- Clickable path breadcrumbs with compact narrow-window presentation and
  `Ctrl+L` editing for paths and local `file://` URIs.
- Window-wide type-to-filter fuzzy matching for the current directory.
- Persistent bookmarks created by dragging folders into the sidebar, with
  pointer reordering and safe removal.
- Internal drag-and-drop moves onto browser folders, Places, and bookmarks,
  including edge auto-scroll and operation-specific drop feedback.
- Native Wayland file drag-and-drop with browsers, desktops, and other file
  managers. Incoming external files are copied through Marcel's bounded,
  cancellable, no-overwrite transfer path.
- A private Nordzy semantic-icon baseline, explicit theme overrides, ambient
  GTK fallback for uncovered names, and progressive image thumbnails backed
  by the standard freedesktop thumbnail cache.
- Resizable browser and preview workspace with a fixed, content-sized Places
  sidebar.

### Preview pane

- Still images and animated GIFs.
- Continuously scrollable PDF pages rendered through bounded Poppler worker
  processes and an identity-aware page cache.
- Rendered Markdown, source code, plain text, and generic file metadata.
- Bounded text work: reads stop at 256 KiB, rich rendering stops at 32 KiB, and
  larger files use a Unicode-aware, soft-wrapped, virtualized fallback.
- Cancellable folder previews that stream a selected directory's immediate
  children without recursively scanning it or creating a second selection
  model.
- Default application opening through GIO, with a desktop-portal fallback and
  an explicit portal-backed **Open With…** chooser.
- **Open in Terminal** for the displayed directory, with default-terminal and
  cross-desktop fallbacks.

### File operations

- New Folder with conflict refusal and identity-validating Undo/Redo.
- Multi-selection Copy, Cut, and Paste within Marcel.
- Same-filesystem moves, including pointer-driven internal moves.
- Bottom-right item/byte progress, cancellation for safe-to-cancel transfers,
  and explicit partial-success reporting.
- Native freedesktop Trash placement, an aggregated Trash entry in Places,
  exact-entry Restore, and identity-validating Trash Undo/Redo.
- Confirmed permanent deletion through `Shift+Delete` or the item menu,
  including selected deletion inside Trash and **Empty Trash**.
- ZIP creation for files, directories, and multi-selection, plus safe
  broad-format **Extract** beside an archive through a supervised 7-Zip
  backend. Both support cancellation and identity-validating Undo/Redo.
- A bounded operation journal with centralized command state shared by
  shortcuts, menus, and toolbar controls.

Marcel never silently overwrites, merges, or invents a destination name.
Occupied destinations are refused. Copy outputs are assembled under hidden
staging names and published with Linux `RENAME_NOREPLACE`.

Successful local copies preserve regular-file contents, directory structure,
symlinks without following them, supported modes and timestamps, `user.*`
xattrs, POSIX ACL attributes, sparse extents, and hardlinks within one copied
tree. See [Copy semantics](docs/copy-semantics.md) for the exact contract and
intentional non-goals.

Permanent deletion is never placed in Undo history. After explicit
confirmation, Marcel atomically quarantines the complete top-level selection,
plans a no-symlink-follow traversal, revalidates filesystem identities, and
removes leaves before directories. It is intentionally not cancellable once
erasure begins because cancellation would itself be a partial destructive
result.

## Keyboard essentials

| Shortcut | Action |
|---|---|
| Arrow keys | Move the primary selection |
| `Shift` + navigation | Extend the selection |
| `Enter` | Open the primary item |
| `Ctrl+Up` | Open the parent directory |
| `Ctrl+Left` / `Ctrl+Right` | Back / forward |
| `Ctrl+L` | Edit the current location |
| `Ctrl+A` | Select all |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| `Delete` | Move the selection to Trash |
| `Shift+Delete` | Confirm permanent deletion |
| `Ctrl+Shift+N` | New Folder |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `Ctrl+F` | Focus the current-directory filter |
| `Escape` | Clear the active filter or selection state |

Direct typing starts filtering regardless of whether the browser or preview
currently has focus, but yields to dialogs and other text editors. Filtering is
in-memory and limited to the current directory; it is not recursive search.

## Installation

### Nix flake (currently supported)

Run Marcel without installing it:

```sh
nix run github:berker-z/marcel -- ~/Downloads
```

Install the current package into a user profile:

```sh
nix profile install github:berker-z/marcel
```

These commands currently follow `master`; pin a commit for reproducible
personal use:

```sh
nix profile install github:berker-z/marcel/FULL_COMMIT_HASH
```

### Use from another Nix flake

Add Marcel as an input:

```nix
inputs.marcel = {
  url = "github:berker-z/marcel";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Apply its overlay and install the package:

```nix
{
  nixpkgs.overlays = [inputs.marcel.overlays.default];
  environment.systemPackages = [pkgs.marcel];
}
```

This installs Marcel without changing any MIME association or generic
file-manager D-Bus ownership. Marcel's default package uses nixpkgs' free
`_7zz` backend and does not require `allowUnfree`.

### Declarative Marcel settings

The flake exports Home Manager and NixOS modules for declarative visual
configuration. With Home Manager:

```nix
{
  imports = [inputs.marcel.homeManagerModules.default];

  programs.marcel = {
    enable = true;
    settings = {
      theme = "tokyo-night";
      icon_theme = "Breeze";
      ui_font = "IBM Plex Mono";
    };
  };
}
```

Use `imports = [inputs.marcel.nixosModules.default]` for the same
`programs.marcel` options in a NixOS module. Both modules install a configured
wrapper while leaving MIME defaults and generic FileManager1 ownership alone.
Set `icon_theme = null` to preserve Marcel's Nordzy-first icon resolution and
`ui_font = null` to use bundled Iosevka; both are the defaults.

View mode and Show Hidden are deliberately not Nix options. Marcel treats them
as interaction state and remembers the last selected values in
`$XDG_CONFIG_HOME/marcel/state.conf`, or `~/.config/marcel/state.conf` when
`XDG_CONFIG_HOME` is unset. First-run defaults are grid view with hidden files
visible.

When using the overlay directly, the same wrapper is available without a
module:

```nix
environment.systemPackages = [
  (pkgs.marcel.withSettings {
    theme = "gruvbox-dark";
    icon_theme = null;
    ui_font = null;
  })
];
```

The flake separately exposes `packages.<system>.file-manager1-service` for a
downstream configuration that explicitly wants Marcel to own the generic
`org.freedesktop.FileManager1` activation service.

For Home Manager, make Marcel the default directory handler declaratively:

```nix
{
  xdg.mimeApps = {
    enable = true;
    associations.added."inode/directory" = ["io.github.berker_z.Marcel.desktop"];
    defaultApplications."inode/directory" = ["io.github.berker_z.Marcel.desktop"];
  };
}
```

The desktop identifier is `io.github.berker_z.Marcel.desktop`.
`marcel.desktop` remains a hidden compatibility alias. Marcel does not claim
ZIP, 7z, RAR, tar, or other archive MIME types.

### Fonts and icons

Marcel ships a private, compact Iosevka Mono subset and a curated private
subset of twenty Nordzy semantic icons. Regular and semibold font faces plus
the SVG icons and their licenses occupy approximately 804 KiB uncompressed,
instead of depending on a complete Nerd Font or the approximately 89 MiB
Nordzy package.

The bundled resources provide Marcel's deliberate default appearance without
installing a font or icon theme system-wide. An explicit Marcel icon-theme
override replaces Nordzy; the ambient GTK theme does not. Instead, the GTK
theme supplies icons missing from Marcel's curated bundle before the final
generic-glyph fallback. Both the UI and monospace text roles use the private
`Marcel Iosevka` family by default. Set `MARCEL_FONT_FAMILY` to the exact name
of an installed family to override both roles. Marcel's own branded launcher
icon remains a separate original asset that must be added before the first
release.

The pinned upstream versions, hashes, Unicode ranges, licenses, and
reproducible generator live in [`assets/README.md`](assets/README.md) and
[`scripts/build_identity_assets.py`](scripts/build_identity_assets.py).

There are no nixpkgs or Flatpak/Flathub releases yet. Until the first tagged
release, the repository flake is the supported installation route.

## Development

Run the local packaged application against a path:

```sh
nix run . -- ~/Downloads
```

Or enter the reproducible development shell:

```sh
nix develop
cargo run
```

Without an argument, Marcel opens the directory it was launched from. It also
accepts absolute or relative local paths and `file://` URIs. Release mode is
substantially faster for large directories and thumbnail-heavy views:

```sh
cargo run --release
```

The first build is intentionally large because GPUI, gpui-component, image
decoders, and other dependencies must be compiled. Dependencies are optimized
even in Marcel's development profile so decode and rendering behavior remains
representative; subsequent Marcel-only builds are incremental.

The Nix shell supplies the current stable Rust toolchain from the locked
`nixpkgs` and `rust-overlay` inputs, plus Poppler, FFmpeg, the free official
7-Zip backend, and native GPUI dependencies. Update the environment
intentionally:

```sh
nix flake update
cargo update
```

Build the installable package and desktop metadata with:

```sh
nix build
```

The package installs `marcel`, the branded
`io.github.berker_z.Marcel.desktop` entry, a hidden `marcel.desktop`
compatibility alias, the branded D-Bus activation service, Poppler/GIO runtime
tools, and a private free 7-Zip backend. RAR and CBR extraction are disabled in
the default package because their decoder is non-free. Advanced users can opt
in with both `MARCEL_7ZZ=/path/to/rar-capable-7zz` and
`MARCEL_ENABLE_RAR=1`. Marcel advertises only `inode/directory`; archive
double-click behavior remains owned by the user's archive viewer.

Required checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Nord is the default semantic palette. The Settings button beside the
list/icon-view control opens a theme selector whose changes apply immediately.
Marcel currently includes Nord, Gruvbox Dark, Tokyo Night, Catppuccin Mocha,
Dracula, One Dark, Solarized Dark, Everforest Dark, Rosé Pine, Kanagawa Wave,
System Dark, and System Light. Development-facing palette and icon-theme
overrides are also available:

```sh
MARCEL_THEME=tokyo-night cargo run
MARCEL_THEME=catppuccin-mocha cargo run
MARCEL_ICON_THEME=breeze cargo run
MARCEL_FONT_FAMILY="IBM Plex Mono" cargo run
```

The Places footer exposes Show Hidden, list/icon view, and the Settings button.
Font selection has one source of truth rather than a session toggle: bundled
Iosevka Mono unless `MARCEL_FONT_FAMILY` explicitly selects an installed
family.

## Known limitations

Marcel is not ready to replace a mature system file manager for every workflow:

- A read-only Properties presentation, non-Nix release artifacts, and release
  automation are not implemented. Marcel's branded application icon is
  installed by the Nix package and used for X11 window metadata.
  Branded D-Bus activation, single-instance routing, and the FileManager1
  navigation methods are implemented. Installing the ordinary package still
  does not take ownership of the generic file-manager D-Bus service.
- Native file drag-and-drop is implemented inbound on Wayland and X11 and
  outbound on Wayland. X11 outbound support and desktop clipboard
  interoperability remain open.
- Cut/move is currently same-filesystem. Cross-filesystem moves and interactive
  conflict decisions are parked; occupied destinations are safely refused.
- New File, Duplicate, Move To, and Properties are not implemented yet.
- Removable-volume navigation, mount management, and remote locations are not
  implemented. Disposable mounted-volume Trash, restore, and read-only failure
  behavior pass their manual acceptance checks.
- Trashed directories can be previewed but not navigated as virtual locations.
- Loaded folders warn about interrupted `.marcel-delete-*` quarantine remnants
  and give conservative recovery guidance. Partial failures also report the
  exact remnant path; a one-click recovery UI is not implemented.
- Recursive filename/content search is not implemented.
- Audio/video metadata, explicit playback, and ebook previews are not
  implemented.
- List/grid view and hidden-file visibility persist as interaction state.
  Broader preference persistence—including pane sizes, sorting, and zoom—is
  still roadmap work; visual identity is declaratively configurable on Nix.
- Sorting, grouping, zoom, and complete accessibility coverage remain roadmap
  work.
- PDF resizing has a non-blocking visual quirk that is explicitly parked.
  Thumbnail failure/loading presentation is complete; the remaining manual
  fixture matrix still needs closure.

## Roadmap

The personal daily-driver milestone is reached. Marcel is now in a deliberate
hardening phase: new features and public-release work are parked while the
remaining filesystem, desktop-integration, and recovery acceptance matrix is
run.

1. Finish Sprint 17's remaining high-DPI, token-bearing activation, and
   interaction acceptance passes.
2. Fix any correctness, recovery, diagnostics, or ownership defects those
   checks expose, with focused regression tests.
3. Resume Sprint 16 only when public release work is wanted; it owns hosted CI,
   public documentation, artwork, AppStream metadata, and release presentation.
4. Resume Properties, New File, Duplicate, Move To, desktop clipboard, remote
   locations, media playback, and other feature work only after hardening is
   accepted.

The authoritative cross-sprint roadmap lives in the
[product backlog](docs/TODO.md). The
[release and distribution plan](docs/release.md) records packaging caveats,
release automation, and the routes to nixpkgs and other Linux distributions.
The
[interaction model](docs/interaction-model.md) defines selection, shortcuts,
menus, reversibility, and destructive-operation behavior. Detailed
implementation and acceptance history is recorded under
[`docs/sprints/`](docs/sprints/). Marcel has progressed through seventeen
numbered implementation sprints. The automated portion of
[Sprint 17](docs/sprints/017-stability-and-architecture-hardening.md) is
implemented and awaits its desktop/manual acceptance matrix;
[Sprint 16](docs/sprints/016-public-release-presentation.md) is intentionally
deferred.

## Acknowledgements and provenance

[Yazi](https://github.com/sxyazi/yazi) is a major inspiration and an explicit
source of architectural ideas and MIT-licensed implementations. Marcel is
deliberately transparent about that relationship: meaningful conceptual and
code adaptations are identified near the relevant implementation and recorded
with upstream files, commits, licenses, and the nature of reuse in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

[Zed](https://github.com/zed-industries/zed) is the primary real-world
reference for using GPUI. We study Zed's application source as practical GPUI
documentation, but do not copy GPL-covered Zed application code into Marcel.
GPUI itself is Apache-2.0.

[gpui-component](https://github.com/longbridge/gpui-component) is Marcel's
default component library. We prefer an existing component over custom UI
unless a measured performance, missing interaction, or accessibility
requirement gives us a concrete reason not to.

Marcel is licensed under the [MIT License](LICENSE).
