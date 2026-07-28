# Sprint 1: Foundation and previews

**Status:** Active — working alpha, core navigation and bounded previews
implemented.

## Goal

Deliver a Linux-first file browser that can navigate local directories without
blocking the GPUI thread and show a useful, cancellable preview for common file
types.

## Deliverables

- [x] A three-pane application shell: places, directory contents, and preview.
- [x] Draggable pane splitters with a 2:6:4 default ratio and bounded minimum
  widths.
- [x] Asynchronous, incremental directory enumeration.
- [x] Single selection and conventional pointer interaction.
- [x] Multi-selection.
- [x] Back, forward, parent, and path display navigation.
- [ ] Interactive breadcrumb segments.
- [x] Preview request cancellation and stale-result rejection when selection
  changes.
- [x] Image previews, including animated GIFs through GPUI's image pipeline.
- [x] Plain-text and source-file previews with a 256 KiB limit.
- [x] Rendered Markdown previews using gpui-component.
- [x] Continuously scrollable PDF previews with virtualized pages, bounded
  rasterization, viewport-first scheduling, and caching.
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

- PDF fixture and rapid-page-change acceptance runs.
- Ask Berker for the PDF preview resize bug reproduction and expected behavior;
  a resize problem is known but intentionally left undescribed until they can
  demonstrate it.
- Audio/video metadata plus an explicit contextual play action.
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
- [x] Use Poppler-backed rasterization for the initial PDF provider. This
  follows Yazi's proven `pdftoppm` bridge while avoiding a large native binding
  in Marcel's process. Marcel renders only the requested page, constrains the
  raster to an 1800-pixel box, caches by file identity, and kills superseded or
  timed-out subprocesses.
- [x] Use GPUI's `uniform_list` for the continuous PDF viewport. There is no PDF
  viewer in gpui-component, and fixed page canvases let the list report its
  visible range directly without constructing off-screen page elements. Actual
  PDF aspect ratios are preserved by fitting each raster within its canvas.
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
- PDF page count and rasterization run through isolated Poppler subprocesses on
  the background executor. Outputs are bounded, render work has a 20-second
  timeout, a replaced request kills its child process, and the cache retains at
  most 512 page/count files.
- PDF documents use a virtualized continuous-scroll list. Only visible pages
  and one page of lookahead are queued through two workers, so document length
  does not determine startup work or decoded-image pressure.
- Places discovery runs off the foreground executor, honors `XDG_CONFIG_HOME`,
  omits missing and disabled directories, and never evaluates user-dir values
  as shell code. If no configuration exists, only conventional user
  directories that actually exist are shown.
- Text reads are capped at 256 KiB. Rich Markdown and syntax rendering is
  limited to 32 KiB; larger payloads use a line-virtualized source view so
  parsing and layout cannot stall the GPUI thread.
- Large-text soft wrapping is computed on the background executor from the
  current preview width. Wrapped visual rows retain source-line numbering and
  remain virtualized.
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
- Added always-visible gpui-component scrollbars to browser and preview lists,
  plus Unicode-width-aware soft wrapping for the large-text fallback.
- Added continuously scrollable PDF previews through a cancellable,
  timeout-bounded Poppler provider, a virtualized page list, visible-first
  two-worker scheduling, fixed-size JPEG rasterization, and an identity-aware
  disk cache.
- Replaced fixed pane widths with draggable gpui-component splitters using a
  2:6:4 Places/browser/preview default and protective minimum widths.
- Unified component and application colors under a palette-driven theme.
- Replaced hard-coded Home/Documents/Downloads entries with asynchronous XDG
  Places discovery.
- Flattened the preview presentation and removed redundant Preview/Open chrome.
- Corrected Linux activation for Hyprland and other desktops where `xdg-open`
  can fall through to a browser.
- Replaced single selection with a path-keyed multi-selection model and added
  conventional Control, Shift, and empty-space drag selection as the first
  Sprint 2 slice.
