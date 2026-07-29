# Marcel product backlog

This is Marcel's cross-sprint backlog and product roadmap. Sprint documents
under [`docs/sprints/`](sprints/) turn selected items into bounded deliverables
and detailed acceptance checks. Finishing a sprint does not imply that every
item here belongs in that sprint.

## Current priorities

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
- [ ] Implement New File, Paste, Open in Terminal, and directory Properties
  behind their shared commands.
- [x] Complete the bounded safe-operation foundation and New Folder slice in
  [`Sprint 3`](sprints/003-safe-file-operations.md).
- Create folders and files.
- Rename files with a pointer-friendly inline interaction.
- Copy, cut, paste, duplicate, move, trash, restore, and permanently delete.
- Add bounded in-memory undo and redo for every reversible file operation,
  including precise records for partially successful multi-file operations.
- Show progress, cancellation, conflicts, and recoverable errors for file
  operations.
- Watch directories and apply incremental filesystem changes without full
  reloads.
- [x] Add instantaneous fuzzy filtering for the current directory with
  window-wide type-to-filter behavior.
- Add recursive filename and content search without blocking navigation.
- Support removable volumes, mounts, and common remote locations.

## Bookmarks and drag interactions

- Add a persistent Bookmarks section directly below Places, separated visually
  from the automatically discovered XDG locations.
- Allow dragging a folder from the browser into the Bookmarks section to create
  a bookmark without moving or modifying the folder itself.
- Add a bookmark context menu with Remove Bookmark. Removing a bookmark must
  never delete, trash, or otherwise mutate its target directory.
- Allow pointer reordering within Bookmarks, with an unambiguous insertion
  indicator and persisted order.
- Support dragging a bookmark out of the section as another discoverable way to
  remove it, while requiring a safe threshold so ordinary navigation clicks
  cannot remove bookmarks accidentally.
- Define one reusable Marcel drag-session model covering payload identity,
  allowed actions, drop-target negotiation, insertion indicators, cancellation,
  edge scrolling, and keyboard/accessibility alternatives.
- Use bookmark creation and reordering as the first non-filesystem proving
  ground for that drag infrastructure before enabling drag-to-copy or
  drag-to-move.
- Later, allow selected filesystem items to be dropped onto bookmark and Places
  destinations using the same safe file-operation scheduler, conflict UI,
  progress reporting, and undo journal as keyboard/menu moves.
- Clearly distinguish reordering a bookmark, bookmarking a folder, and moving
  filesystem contents; the cursor and drop indicator must expose which action
  will occur before release.

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

- Add repeatable large-directory and rapid-selection benchmarks.
- Add representative preview fixtures without introducing unclear licensing.
- Add crash recovery and structured diagnostics.
- Define supported Linux desktop environments and test Wayland/X11 behavior.
- Establish release versioning, changelog, and compatibility policy.
- Keep Yazi adaptations and other upstream reuse documented in
  `THIRD_PARTY_NOTICES.md`.
