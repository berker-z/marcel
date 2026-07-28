# Sprint 1: Foundation and previews

**Status:** Active — working alpha, core navigation and bounded previews
implemented.

## Goal

Deliver a Linux-first file browser that can navigate local directories without
blocking the GPUI thread and show a useful, cancellable preview for common file
types.

## Deliverables

- [x] A three-pane application shell: places, directory contents, and preview.
- [x] Asynchronous, incremental directory enumeration.
- [x] Single selection and conventional pointer interaction.
- [ ] Multi-selection.
- [x] Back, forward, parent, and path display navigation.
- [ ] Interactive breadcrumb segments.
- [x] Preview request cancellation and stale-result rejection when selection
  changes.
- [x] Image previews, including animated GIFs through GPUI's image pipeline.
- [x] Plain-text and source-file previews with a 256 KiB limit.
- [x] Rendered Markdown previews using gpui-component.
- [ ] PDF previews.
- [ ] Media metadata and an explicit play action for audio and video.
- [x] A generic metadata fallback for unsupported file types.
- [x] Loading, unsupported, empty, and error states.
- [x] Open files through the system default application.
- [x] A shared, palette-driven theme for application surfaces and components,
  with Nord as the default.
- [x] Places discovered from configured XDG user directories, with existing
  conventional directories as a fallback.
- [x] A flat, content-first preview pane without redundant header, action, or
  nested card chrome.

## Remaining work

- PDF preview provider and fixtures.
- Audio/video metadata plus an explicit contextual play action.
- Multi-selection.
- Interactive breadcrumb segments.
- Large-directory, rapid-selection, corrupt-file, and complete preview-fixture
  acceptance runs.

## Engineering constraints

- Filesystem and preview work never run on GPUI's foreground executor.
- Preview providers are isolated behind a small common interface.
- Large directories are rendered through a virtualized list.
- New selection supersedes stale preview work.
- Unbounded files are never loaded wholly into memory.
- File operations are out of scope for this sprint.

## Early decisions to validate

- [x] Use GPUI's `uniform_list` for virtualization with gpui-component
  `ListItem` rows. This keeps the high-volume viewport native while retaining
  the component library's selection and hover treatment.
- [x] Start with GPUI's built-in image pipeline. Its first-party GIF viewer
  demonstrates animated GIF support without a separate decoder.
- [ ] Choose between native Rust rendering and Poppler-backed rasterization for
  the initial PDF provider.
- [x] Use gpui-component `TextView` for bounded rich previews. A measured freeze
  on a roughly 180 KiB `Cargo.lock` showed that its synchronous initial
  Markdown parse and syntax highlighting are unsuitable for larger payloads,
  so those use GPUI `uniform_list` with bounded lines instead.
- [x] Start media playback with the system default application unless embedded
  playback proves inexpensive and reliable. On Linux, double-click opening
  uses MIME-aware `gio open`, with the desktop portal's file-descriptor API as
  a fallback; media will receive an explicit contextual play action.
- [x] Keep the preview pane content-first. Images use the full available
  content region; text applies its own reading inset and optional metadata is
  kept in a compact footer.

## Implemented architecture

- Directory enumeration runs through `smol::unblock` and emits sorted batches
  over an async channel.
- Every directory request has a ticket. Superseded batches are ignored.
- Sorted batches are merged incrementally so a large directory becomes usable
  before enumeration completes.
- Selection is keyed by path rather than row number, so incoming sorted batches
  do not move the selection to another file.
- Preview tasks are replaceable and ticketed. A late preview result cannot
  overwrite the current selection's preview.
- Places discovery runs off the foreground executor, honors `XDG_CONFIG_HOME`,
  omits missing and disabled directories, and never evaluates user-dir values
  as shell code. If no configuration exists, only conventional user
  directories that actually exist are shown.
- Text reads are capped at 256 KiB. Rich Markdown and syntax rendering is
  limited to 32 KiB; larger payloads use a line-virtualized source view so
  parsing and layout cannot stall the GPUI thread.
- Individual preview lines are bounded before layout, preventing minified or
  generated one-line files from monopolizing a frame.
- File paths remain `PathBuf`; lossy UTF-8 conversion is limited to display
  names.

## Acceptance checks

- [ ] The UI remains interactive while entering a directory with at least 50,000
  entries.
- [ ] Rapidly changing selection never displays a stale preview as current.
- [ ] Corrupt or unsupported files produce an error state rather than a crash.
- [ ] Image, GIF, text, Markdown, PDF, audio, and video fixtures each exercise the
  expected provider.
- [x] Large plain-text payloads select the virtualized path instead of
  synchronous rich rendering.
- [x] Configured and fallback XDG Places parsing is covered by unit tests.
- [x] Linux file activation honors MIME defaults without forcing an Open With
  dialog during the normal path.

## Progress log

- Established the Nix development shell and locked Rust toolchain inputs.
- Built the GPUI/gpui-component three-pane shell and conventional pointer
  interaction.
- Adapted Yazi-inspired incremental enumeration, bounded history, and
  stale-preview rejection behind Marcel-owned modules.
- Added image/GIF, bounded text/source, rendered Markdown, and metadata preview
  paths.
- Fixed large-text UI stalls with line virtualization and per-line bounds.
- Unified component and application colors under a palette-driven theme.
- Replaced hard-coded Home/Documents/Downloads entries with asynchronous XDG
  Places discovery.
- Flattened the preview presentation and removed redundant Preview/Open chrome.
- Corrected Linux activation for Hyprland and other desktops where `xdg-open`
  can fall through to a browser.
