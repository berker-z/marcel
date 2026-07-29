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

There is not yet an installable release package or stable compatibility
promise. Development currently happens through the Nix flake.

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
- Window-wide type-to-filter fuzzy matching for the current directory.
- Persistent bookmarks created by dragging folders into the sidebar, with
  pointer reordering and safe removal.
- Internal drag-and-drop moves onto browser folders, Places, and bookmarks,
  including edge auto-scroll and operation-specific drop feedback.
- Semantic icons from the active freedesktop icon theme and progressive image
  thumbnails backed by the standard freedesktop thumbnail cache.
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

## Development

Enter the reproducible development shell and run Marcel:

```sh
nix develop
cargo run
```

Marcel opens the directory it was launched from. Release mode is substantially
faster for large directories and thumbnail-heavy views:

```sh
cargo run --release
```

The first build is intentionally large because GPUI, gpui-component, image
decoders, and other dependencies must be compiled. Dependencies are optimized
even in Marcel's development profile so decode and rendering behavior remains
representative; subsequent Marcel-only builds are incremental.

The Nix shell supplies the current stable Rust toolchain from the locked
`nixpkgs` and `rust-overlay` inputs, plus Poppler, FFmpeg, the RAR-enabled
official 7-Zip backend, and native GPUI dependencies. Update the environment
intentionally:

```sh
nix flake update
cargo update
```

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
```

The Places footer currently exposes session-level switches for Iosevka, hidden
files, and list/icon view, plus the Settings button. Iosevka is enabled by
default when a supported installed family is available.

## Known limitations

Marcel is not ready to replace a mature system file manager for every workflow:

- Packaging, a desktop entry, application icons, `inode/directory`
  registration, `org.freedesktop.FileManager1`, and single-instance behavior
  are not implemented.
- Native drag-and-drop and clipboard interoperability with browsers, desktop
  surfaces, and other file managers are not implemented. Dragging inside
  Marcel works.
- Cut/move is currently same-filesystem. Cross-filesystem moves and interactive
  conflict decisions are parked; occupied destinations are safely refused.
- New File, Duplicate, Move To, and Properties are not implemented yet.
- Removable-volume navigation, mount management, and remote locations are not
  implemented. Mounted-volume Trash behavior still needs its manual acceptance
  pass.
- Trashed directories can be previewed but not navigated as virtual locations.
- Startup discovery and recovery UI for interrupted `.marcel-delete-*`
  quarantine remnants is not implemented. Partial failures report the remnant
  path.
- Recursive filename/content search is not implemented.
- Audio/video metadata, explicit playback, and ebook previews are not
  implemented.
- Settings are session-only. Pane sizes, view choice, typography, palette, and
  other preferences are not persisted.
- Interactive breadcrumb segments, sorting, grouping, zoom, and complete
  accessibility coverage remain roadmap work.
- PDF resizing has a known behavior problem that still needs a reproducible
  report and UX decision. Thumbnail failure/loading presentation and the full
  large-directory/manual fixture matrix also need polish.

## Roadmap

The immediate alpha-to-daily-driver sequence is:

1. Complete destructive-operation, mounted-Trash, watcher, preview, and
   large-directory acceptance passes.
2. Finish New File and Properties, then add Duplicate and Move To in their
   appropriate operation slices.
3. Mechanically extract preview, sidebar, and drag/drop lifecycle ownership
   from the application coordinator.
4. Implement bilateral native desktop drag-and-drop and clipboard
   interoperability.
5. Add cross-filesystem transfers, conflict decisions, and a documented
   symbolic-link policy.
6. Add removable volumes, mounts, and common remote locations.
7. Package Marcel through the flake and add desktop/default-file-manager
   integration.
8. Consolidate persistent settings, themes, sorting, grouping, zoom,
   breadcrumbs, and accessibility work.
9. Add media playback and optional ebook previews after the file-manager
   foundation is complete.

The authoritative cross-sprint roadmap lives in the
[product backlog](docs/TODO.md). The
[interaction model](docs/interaction-model.md) defines selection, shortcuts,
menus, reversibility, and destructive-operation behavior. Detailed
implementation and acceptance history is recorded under
[`docs/sprints/`](docs/sprints/); Marcel has progressed through eight numbered
sprints rather than remaining at Sprint 1.

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
