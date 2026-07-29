# Marcel

Marcel is an experimental, preview-first graphical file explorer built with
Rust, GPUI, and gpui-component.

## Current status

Marcel is a working Linux-first alpha. It currently provides:

- Conventional plain, Control, Shift, and drag selection with double-click
  activation.
- Asynchronous, incremental directory enumeration with virtualized rows.
- Back, forward, parent, refresh, and Places navigation.
- A fixed, content-sized Places sidebar and draggable browser/preview splitter
  with a 3:2 workspace default.
- An application-wide top bar for navigation, the current-path breadcrumb
  surface, and current-directory filter.
- Window-wide type-to-filter: typing anywhere in Marcel fuzzy-filters the
  current directory, while `Ctrl+F` focuses the same top-bar input explicitly.
- Places discovered from XDG user directories, with existing conventional
  directories used when no XDG configuration exists. Rows are left-aligned and
  use semantic icons from the active freedesktop icon theme.
- Compact Places-footer switches for Iosevka, hidden files, and list/icon view
  mode.
- A persistent, flat preview pane for images and animated GIFs, continuously
  scrollable PDFs, rendered Markdown, source code, plain text, and generic file
  metadata.
- Streamed, virtualized folder previews with immediate-child counts and
  double-click activation, without recursive scanning or a second selection
  model.
- Bounded text previews: 256 KiB maximum reads, rich rendering below 32 KiB,
  and a Unicode-aware, soft-wrapped, line-virtualized fallback for larger
  files.
- MIME-default file opening on Linux through `gio`, with the desktop portal as
  a fallback and an explicit portal-backed `Open With…` chooser.
- Native folder and MIME icons resolved from the active freedesktop icon theme.
- Responsive list and icon views with selection preserved between them.
- Progressive image thumbnails backed by the standard freedesktop disk cache.
- A shared palette system for Marcel and gpui-component, with Nord as the
  default.

Sprint 1 remains active. Audio/video metadata and play actions, interactive
breadcrumbs, and the full stress/fixture acceptance pass are not implemented
yet. General file operations are outside the first sprint.

## Development

Enter the reproducible development shell and run the application:

```sh
nix develop
cargo run
```

The first development build compiles third-party dependencies with
optimizations so image decoding and rendering behave much closer to a release
build. That one-time build is larger; subsequent Marcel-only rebuilds remain
incremental.

Marcel opens the directory it was launched from. Click an entry to select and
preview it; double-click a folder to navigate or a file to open it with the
configured system application. A selected folder previews its immediate
children; double-click a child in that preview to navigate into a folder or
open a file.

Start typing anywhere in Marcel to fuzzy-filter the displayed directory.
Matches update both list and icon views; Up/Down moves through them, Enter
activates the primary match, Backspace edits the query, and Escape clears it.
`Ctrl+F` focuses the filter field directly. This is an in-memory filter of the
current directory, not recursive filesystem search.

PDF pages are rendered by Poppler in cancellable background processes. The Nix
development shell includes the required `pdfinfo` and `pdftoppm` utilities.

Nord is the default palette. Marcel's own surfaces and gpui-component widgets
share the same theme, so additional palettes do not require repainting the UI
piece by piece. The built-in alternatives can currently be selected when
launching:

```sh
MARCEL_THEME=dark cargo run
MARCEL_THEME=light cargo run
```

This environment variable is a development-facing switch; an in-app theme
selector can be added on top of the same theme API later.

The Places sidebar includes a session-level `Iosevka Mono` switch. It changes
the shared UI font immediately and returns to the active theme's original font
when switched off. Marcel currently uses an installed `Iosevka Nerd Font Mono`
or plain `Iosevka` family; distribution packaging will supply a small,
OFL-licensed subset so the setting does not depend on a system font.

On Linux, filesystem icons follow the configured GTK/freedesktop icon theme.
Override it for development or comparison with:

```sh
MARCEL_ICON_THEME=breeze cargo run
```

The shell tracks the latest stable Rust toolchain supplied by the locked
`nixpkgs` and `rust-overlay` inputs. Update the complete environment
intentionally with:

```sh
nix flake update
cargo update
```

Useful checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

## Product direction

Marcel aims to feel like a conventional graphical file explorer while keeping
directory navigation and previews off the UI thread. The persistent preview
pane is a primary interface, not an optional add-on. Preview content owns the
pane directly rather than being nested in a decorative card.

The cross-sprint [product backlog](docs/TODO.md) tracks longer-term work,
desktop integration, packaging, and ideas that have not yet been assigned to a
bounded implementation sprint.

The [interaction and command model](docs/interaction-model.md) defines
keyboard shortcuts, context-menu behavior, and the safety contract for future
file operations, undo, and redo.

See [Sprint 1](docs/sprints/001-foundation-and-previews.md) for completed
deliverables, remaining work, architecture decisions, and acceptance status.
The proposed [Sprint 2](docs/sprints/002-selection-and-visual-browsing.md)
defines conventional multi-selection, drag selection, icon view, native icon
themes, and a progressive thumbnail pipeline.

## Acknowledgements and provenance

[Yazi](https://github.com/sxyazi/yazi) is a major inspiration and an intended
source of both architectural ideas and MIT-licensed implementations. Marcel
will be unusually direct about that relationship: substantial adaptations are
identified near the relevant code and recorded in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

[Zed](https://github.com/zed-industries/zed) is the primary real-world
reference for using GPUI. We study Zed's application code as documentation, but
only copy code whose license is compatible with Marcel. GPUI itself is
Apache-2.0.

[gpui-component](https://github.com/longbridge/gpui-component) is Marcel's
default component library. We prefer an existing component over custom UI
unless a measured performance, interaction, or accessibility requirement makes
custom work necessary.
