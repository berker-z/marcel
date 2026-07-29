# Marcel product backlog

This is Marcel's cross-sprint backlog and product roadmap. Sprint documents
under [`docs/sprints/`](sprints/) turn selected items into bounded deliverables
and detailed acceptance checks. Finishing a sprint does not imply that every
item here belongs in that sprint.

## Current priorities

- Marcel's local-filesystem core is now a credible alpha: browsing, previews,
  selection, filtering, bookmarks, incremental watching, safe copy/move,
  Trash/restore, and confirmed permanent deletion are implemented. The main
  remaining gaps between that alpha and a daily-driver default file manager
  are conventional file actions, desktop interoperability, mounted and remote
  locations, packaging, and acceptance testing.
- Complete the manual Trash/restore checks in
  [`Sprint 7`](sprints/007-trash-and-restore.md). The implementation uses the
  native freedesktop Trash, identity-validating undo/redo, and an aggregated
  bottom-most Places entry.
- Complete the permanent-delete and Empty Trash manual checks in
  [`Sprint 8`](sprints/008-permanent-deletion.md).
- Complete New File and Properties in
  [`Sprint 9`](sprints/009-conventional-local-actions.md); safe inline Rename
  and Open in Terminal are implemented.
- Run Sprint 4's final manual 10,000-entry list/icon responsiveness check; the
  operation-controller and directory-session extractions are complete.
- Run Sprint 5's manual watcher acceptance checks. Incremental active-directory
  watching is connected through the extracted session reducer, with native and
  polling backends plus bounded rescan fallback.
- Complete Sprint 1 preview and navigation acceptance runs.
- Ask Berker to demonstrate the known PDF preview resize problem and describe
  the expected behavior before choosing a fix.
- Add audio and video metadata previews with an explicit play action.
- [x] Add interactive breadcrumb navigation and a `Ctrl+L` editable location
  mode for paths and local file URIs.
- Finish Sprint 2 visual-browsing acceptance checks and thumbnail failure
  presentation.

## Recommended delivery order

This order favors daily-driver completeness over novelty while keeping new
state machines out of Marcel's coordinator until they have clear ownership.

1. Finish the Sprint 7 and Sprint 8 destructive-operation smoke tests and
   record any discovered recovery or mounted-volume behavior.
2. Complete conventional local actions: Rename first, followed by New File,
   Open in Terminal, Properties, Duplicate, and Move To.
3. Mechanically extract preview, sidebar, and drag/drop lifecycle ownership
   from `app.rs`, preserving current behavior and tests. Do not turn this into
   an abstract architecture rewrite.
4. Implement bilateral native desktop drag-and-drop and desktop clipboard
   interoperability.
5. Add cross-filesystem transfers, explicit conflict decisions, and the
   documented symbolic-link policy.
6. Add removable volumes, mounts, and common remote locations.
7. Package Marcel through the flake, install its desktop metadata, implement
   the required file-manager D-Bus surface, and document making it the default
   directory handler.
8. Consolidate persistent settings, themes, fonts, sorting, grouping, zoom,
   and other UI refinements.
9. Add media playback and optional ebook previews after the file-manager and
   desktop-integration foundation is complete.

The known PDF resize problem, interrupted permanent-delete quarantine
recovery, large-directory benchmarks, thumbnail failure presentation, and
manual sprint acceptance checks remain cross-cutting quality work rather than
optional feature ideas.

## Desktop integration and distribution

- [x] Accept local paths and `file://` URIs on the command line so desktop launchers
  can invoke `marcel %U`.
- [x] Add an installable application package to the flake:
  - `packages.default`
  - `apps.default`
  - an overlay for downstream flakes
  - build and runtime dependency declarations suitable for GPUI, Poppler, and
    future media preview tools
- [x] Install a freedesktop desktop entry using the generic file-manager icon.
  Add branded application icons before a stable release.
- [x] Advertise `inode/directory` support and document setting `marcel.desktop` as
  the default directory handler.
- [x] Provide a Home Manager example for installing Marcel and setting
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
- [x] Implement Move to Trash, identity-validating Undo/Redo, explicit Restore,
  and an aggregated system Trash entry at the bottom of Places.
- [x] Implement confirmed permanent deletion through `Shift+Delete` and item
  menus, plus paired Trash purge and Empty Trash. Keep it outside Undo history.
- Add startup discovery and recovery guidance for interrupted
  `.marcel-delete-*` quarantine remnants.
- Generalize filesystem locations into a virtual-location abstraction so
  trashed directories can be navigated without treating their backing paths as
  ordinary folders.
- [ ] Implement New File and directory Properties behind their shared
  commands.
- [x] Implement Open in Terminal through the shared current-directory command,
  preferring `xdg-terminal-exec`, then `TERMINAL`, then explicit
  working-directory fallbacks.
- [x] Complete the bounded safe-operation foundation and New Folder slice in
  [`Sprint 3`](sprints/003-safe-file-operations.md).
- Create folders and files.
- [x] Rename files with a pointer-friendly inline interaction, `F2`, atomic
  no-overwrite publication, and identity-validating Undo/Redo.
- Finish desktop clipboard interoperability and queued transfers; then add
  duplicate and move-to. Cross-filesystem
  cut/paste and interactive conflict decisions are explicitly parked until
  their safety and UX work is scheduled.
- [x] Add a non-overlapping bottom-right progress/cancellation card for active
  copy and move operations, with item/byte accounting.
- [x] Add ZIP creation and broad-format extraction through the shared background
  operation scheduler, with progress, cancellation, no-overwrite publication,
  Undo, and protection against unsafe archive paths. MIME identifies archives
  and their associated viewer; it does not provide a portable create/extract
  operation. Put official `7zz` behind a Marcel-owned backend. Distribution
  packages automatically supply it; portable artifacts bundle the static
  executable at about 1.3 MiB compressed. Extraction has one explicit
  `Extract` action and always publishes beside the archive; there is no
  `Extract To…` action. See
  [`Sprint 10`](sprints/010-archive-operations.md).
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
  changes incrementally without a full reload.
- [x] Apply exact top-level results from Marcel-owned create, rename,
  copy/move, Trash/restore, permanent-delete, and Undo/Redo operations through
  the directory reducer without clearing the active session. Keep full rescans
  only as an explicit correctness fallback.
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

- [x] Build the first gpui-component Settings modal and expose it beside the
  list/icon-view control.
- [x] Add a hot-reloading theme selector backed by Marcel's semantic color
  tokens, with a curated built-in palette registry.
- Support installing or loading additional palettes without introducing
  hard-coded component colors.
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

- [x] Give the Nord UI a darker outer shell and a lighter center browser
  surface.
- [x] Normalize Place, bookmark, and grid-item names to the base UI text size;
  give grid names three lines and make their visuals fill more of each tile.
- [x] Replace the static path display with clickable breadcrumbs and an
  editable, validation-reporting `Ctrl+L` location field.
- Persist pane sizes, view mode, sort mode, and other user preferences.
- Add conventional context menus and properties.
- Add configurable sorting, grouping, hidden-file display, and zoom.
- Add keyboard navigation and shortcuts without making Marcel modal or
  Vim-dependent.
- Add accessible names, focus treatment, and keyboard alternatives for every
  pointer interaction.
- [x] Add an in-app theme selector on top of the existing palette system.
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
