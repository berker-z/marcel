# Marcel product backlog

This is Marcel's cross-sprint backlog and product roadmap. Sprint documents
under [`docs/sprints/`](sprints/) turn selected items into bounded deliverables
and detailed acceptance checks. Finishing a sprint does not imply that every
item here belongs in that sprint.

## Current priorities

- Complete the copy-fidelity fixtures and implementation in
  [`Sprint 6`](sprints/006-copy-fidelity-and-scale.md), then begin Trash.
- Run Sprint 4's final manual 10,000-entry list/icon responsiveness check; the
  operation-controller and directory-session extractions are complete.
- Run Sprint 5's manual watcher acceptance checks. Incremental active-directory
  watching is connected through the extracted session reducer, with native and
  polling backends plus bounded rescan fallback.
- Complete Sprint 1 preview and navigation acceptance runs.
- Ask Berker to demonstrate the known PDF preview resize problem and describe
  the expected behavior before choosing a fix.
- Add audio and video metadata previews with an explicit play action.
- Add interactive breadcrumb navigation.
- Finish Sprint 2 visual-browsing acceptance checks and thumbnail failure
  presentation.

## Desktop integration and distribution

- Accept local paths and `file://` URIs on the command line so desktop launchers
  can invoke `marcel %U`.
- Add an installable application package to the flake:
  - `packages.default`
  - `apps.default`
  - an overlay for downstream flakes
  - build and runtime dependency declarations suitable for GPUI, Poppler, and
    future media preview tools
- Install a freedesktop desktop entry and application icons.
- Advertise `inode/directory` support and document setting `marcel.desktop` as
  the default directory handler.
- Provide a Home Manager example or module for installing Marcel and setting
  its MIME association declaratively.
- Implement `org.freedesktop.FileManager1` D-Bus support for:
  - `ShowItems`
  - `ShowFolders`
  - `ShowItemProperties`
- Decide whether Marcel should use a single running instance when desktop and
  D-Bus requests arrive.
- Add release automation and optionally a binary cache.
- Submit Marcel to nixpkgs after the application has a stable release,
  complete metadata, icons, desktop integration, and reproducible packaging.
- Treat becoming an `xdg-desktop-portal` file-picker backend as a separate
  project, not part of becoming the default file explorer.

## File-management fundamentals

- [x] Implement the shared command and enabled-state layer defined in
  [`interaction-model.md`](interaction-model.md).
- [x] Add conventional keyboard navigation and selection shortcuts.
- [x] Add an item context-menu shell backed by the shared command layer, with
  `Open` and `Open With…` active and planned file operations visibly disabled.
- [x] Add the current-directory context-menu shell specified in
  [`interaction-model.md`](interaction-model.md), with Select All, Refresh, and
  Copy Location active and future commands visibly disabled. Keep persistent
  view-mode controls in the Places footer.
- [x] Implement Show Hidden Files through the Places footer and
  current-directory menu.
- [x] Implement New Folder and its bounded, conflict-validating undo/redo path.
- [x] Implement the session-local Copy, Cut, and Paste slice behind shared
  commands, no-overwrite transfers, cancellation, and identity-validating
  undo/redo.
- [ ] Implement New File, Open in Terminal, and directory Properties behind
  their shared commands.
- [x] Complete the bounded safe-operation foundation and New Folder slice in
  [`Sprint 3`](sprints/003-safe-file-operations.md).
- Create folders and files.
- Rename files with a pointer-friendly inline interaction.
- Finish desktop clipboard interoperability and queued transfers; then add
  duplicate, move-to, trash, restore, and permanent deletion. Cross-filesystem
  cut/paste and interactive conflict decisions are explicitly parked until
  their safety and UX work is scheduled.
- [x] Add a non-overlapping bottom-right progress/cancellation card for active
  copy and move operations, with item/byte accounting.
- Add ZIP archive creation and extraction through the shared background
  operation scheduler, with progress, cancellation, conflict handling, and
  protection against unsafe archive paths.
- Research and document Marcel's symbolic-link policy before expanding write
  operations: compare Yazi and conventional GUI file managers for copying,
  moving, deleting, previewing, resolving broken links, following links during
  recursion, and preventing cycles. Turn the result into explicit behavioral
  tests and user-facing wording.
- Add bounded in-memory undo and redo for every reversible file operation,
  including precise records for partially successful multi-file operations.
- Extend progress/cancellation beyond copy and move as more long-running
  operation types land; add conflict decisions when that parked work resumes.
- [x] Watch the active directory and apply coalesced external filesystem
  changes incrementally without a full reload. Marcel's own completed
  operations still perform a conservative reload until operation-to-watcher
  reporting is implemented.
- [x] Add instantaneous fuzzy filtering for the current directory with
  window-wide type-to-filter behavior.
- Add recursive filename and content search without blocking navigation.
- Support removable volumes, mounts, and common remote locations.

## Bookmarks and drag interactions

- [x] Add a persistent Bookmarks section directly below Places, separated
  visually from the automatically discovered XDG locations.
- [x] Allow dragging a folder from the browser into the Bookmarks section to
  create a bookmark without moving or modifying the folder itself.
- [x] Add a bookmark context menu with Remove Bookmark. Removing a bookmark must
  never delete, trash, or otherwise mutate its target directory.
- [x] Allow pointer reordering within Bookmarks, with an unambiguous insertion
  indicator and persisted order.
- Support dragging a bookmark out of the section as another discoverable way to
  remove it, while requiring a safe threshold so ordinary navigation clicks
  cannot remove bookmarks accidentally.
- [x] Define the first reusable Marcel drag-session payload covering selection
  identity, bookmark candidates, typed drop targets, insertion indicators, and
  cancellation through GPUI's active-drag lifecycle.
- [x] Reuse the marquee edge-scroll acceleration and bounds for file drags in
  both list and icon views. Extend drag sessions later with explicit action
  negotiation and keyboard/accessibility alternatives.
- [x] Use bookmark creation and reordering as the first non-filesystem proving
  ground for the shared drag payload and typed drop-target infrastructure.
- [x] Allow selected filesystem items to be dropped onto browser folders,
  Bookmarks, and Places using the safe move engine and undo journal.
- Later, extend those drops with copy/link modifiers, cross-filesystem moves,
  progress UI, hover-open, and native desktop drag-and-drop.
- [x] Clearly distinguish reordering a bookmark, bookmarking a folder, and
  moving filesystem contents with target styling, insertion lines, and
  operation-specific cursors.

## Preview quality

- [x] Add a lightweight preview for a selected folder's immediate children.
  Keep it virtualized and cancellable; allow hover and double-click activation,
  but do not turn the preview pane into a second selectable file browser.
- Consider image thumbnails in folder-preview rows after measuring whether
  their extra scheduling and decoded-image pressure materially improves the
  glanceable preview.
- Add PDF fixtures covering long documents, mixed page sizes, corruption, and
  rapid scrolling.
- Determine whether PDF page canvases should use actual per-page dimensions
  instead of a uniform fitted canvas.
- Add adjacent-page prefetch tuning based on measured scroll behavior.

## Settings, themes, and typography

- Build a basic settings UI using gpui-component controls rather than leaving
  preferences as scattered sidebar toggles.
- Add a theme selector backed by Marcel's semantic color tokens and support
  installing or loading additional palettes without introducing hard-coded
  component colors.
- Add a UI-font selector with system font discovery, an explicit default, and
  a bundled-font policy. Preserve the fast Iosevka toggle as a convenient
  preset until the selector replaces it.
- Move Show Hidden, default view mode, typography, theme, preview behavior, and
  other durable preferences into a documented settings model.
- Persist settings under the XDG config directory with versioned, atomic,
  recoverable writes.
- Add media thumbnails, metadata, and explicit playback.
- Unprioritized idea: add lightweight EPUB and MOBI previews showing the cover,
  metadata, table of contents where available, and a bounded text sample.
  Prefer small pure-Rust parsers; do not embed Calibre, a browser engine, or a
  full ebook reader. Handle DRM-protected content as an unsupported/metadata-
  only case.
- Define cache inspection, size policy, and clearing controls.
- Expand unsupported, loading, corrupt, and partial-preview presentation.

## Interaction and polish

- Persist pane sizes, view mode, sort mode, and other user preferences.
- Add conventional context menus and properties.
- Add configurable sorting, grouping, hidden-file display, and zoom.
- Add keyboard navigation and shortcuts without making Marcel modal or
  Vim-dependent.
- Add accessible names, focus treatment, and keyboard alternatives for every
  pointer interaction.
- Add an in-app theme selector on top of the existing palette system.
- [x] Make custom Marcel surfaces use the active theme radius and keep
  typography on semantic `rem`-based roles.
- [x] Add a session-level system/Iosevka UI-font switch.
- [x] Measure the active monospace font for virtualized preview wrapping and
  row height instead of assuming fixed character geometry.
- Persist typography preferences and package a compact OFL-licensed Iosevka
  face or subset for systems where the family is not installed.

## Engineering and release quality

- Use [`external-review.md`](external-review.md) as design input for the
  refactor-first watcher roadmap and copy-semantics work; it is not itself an
  implementation specification.
- Add repeatable large-directory and rapid-selection benchmarks.
- Add representative preview fixtures without introducing unclear licensing.
- Add crash recovery and structured diagnostics.
- Define supported Linux desktop environments and test Wayland/X11 behavior.
- Establish release versioning, changelog, and compatibility policy.
- Keep Yazi adaptations and other upstream reuse documented in
  `THIRD_PARTY_NOTICES.md`.
