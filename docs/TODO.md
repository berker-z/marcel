# Marcel product backlog

This is Marcel's cross-sprint backlog and product roadmap. Sprint documents
under [`docs/sprints/`](sprints/) turn selected items into bounded deliverables
and detailed acceptance checks. Finishing a sprint does not imply that every
item here belongs in that sprint.

## Current priorities

- Freeze feature and release work while
  [`Sprint 17`](sprints/017-stability-and-architecture-hardening.md) closes the
  confirmed PDF, watcher, D-Bus reveal, selection, window-lifecycle,
  non-UTF-8 filename, hostile-image, enumeration, and hot-path allocation
  problems found during external review.
- Mechanically extract preview, sidebar, drag/drop, and cohesive window UI
  ownership from `app.rs` during the same hardening sprint. Preserve behavior
  and avoid an event bus or speculative trait architecture.
- Marcel's local-filesystem core has reached the personal daily-driver
  milestone: browsing, previews, selection, filtering, bookmarks, incremental
  watching, safe copy/move, Trash/restore, permanent deletion, archives,
  packaging, D-Bus activation, single-instance routing, and bilateral Wayland
  file drag-and-drop are implemented.
- After Sprint 17, continue distribution hardening before adding more features:
  the free 7-Zip
  baseline and private font/icon identity bundle are complete; add Marcel's
  branded icon and AppStream metadata, audit every runtime subprocess/resource,
  and test from a clean minimal desktop.
- Make the public documentation match the product's maturity. Restructure the
  README around the application rather than the current packaging path, add
  polished visual media, and keep installation guidance platform-neutral even
  while Nix remains the only shipped package.
- Cut a tagged `0.1.0` personal MVP release from the hardened package. Keep
  becoming the default directory handler a separate, explicit user step, then
  prepare the independent nixpkgs package contribution.
- Finish Sprint 14's shared Properties/`ShowItemProperties` surface after the
  packaging milestone. X11 outbound drag support is not a blocker for the
  initial Wayland-focused release.
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

## Daily-driver milestone and `v0.1.0` closure

The personal daily-driver milestone is complete. The remaining work before a
tagged, publicly consumable `v0.1.0` is intentionally bounded:

1. Harden the distribution closure: free archive baseline, packaged icon
   fallback, branded icon/AppStream metadata, clean-environment launch checks,
   and documented runtime dependencies.
2. Tag `0.1.0`, build it from the tag on both declared architectures, and
   verify that installing it takes neither MIME nor generic FileManager1
   ownership.
3. Implement one shared read-only Properties presentation and route both the
   in-app action and D-Bus `ShowItemProperties` through it.
4. Implement New File with the same bounded name validation and no-overwrite
   behavior as New Folder.
5. Run the outstanding destructive-operation smoke checks, especially
   mounted-volume Trash behavior.
6. Submit the tagged package to nixpkgs after its package recipe passes
   `nixpkgs-review`.

Everything else—X11 outbound drag, desktop clipboard integration, Duplicate,
Move To, cross-filesystem conflict UI, remote locations, broader preference
persistence, custom sorting, media playback, and deeper coordinator
extraction—is valuable post-MVP work rather than a reason to hold the first
personal release.

## Recommended delivery order

This order favors daily-driver completeness over novelty while keeping new
state machines out of Marcel's coordinator until they have clear ownership.

1. Complete [Sprint 17](sprints/017-stability-and-architecture-hardening.md):
   correctness fixes, hostile-input bounds, validation debt, and mechanical
   coordinator extraction.
2. Complete [Sprint 16](sprints/016-public-release-presentation.md): public
   documentation, branded artwork, AppStream metadata, and honest
   platform-neutral installation structure.
3. Complete the distribution-hardening checklist below and test the flake
   install on a clean minimal NixOS environment.
4. Cut `v0.1.0` and prepare the nixpkgs package.
5. Close Sprint 14's remaining Properties surface without changing the user's
   default directory handler merely by installing Marcel.
6. Add New File, then finish the Sprint 7 and Sprint 8 destructive-operation
   smoke tests and record any discovered recovery or mounted-volume behavior.
7. Add Duplicate and Move To.
8. Finish X11 source support and manual acceptance for the implemented
   bilateral native desktop drag-and-drop, then add desktop clipboard
   interoperability.
9. Add cross-filesystem transfers, explicit conflict decisions, and the
   documented symbolic-link policy.
10. Add removable volumes, mounts, and common remote locations.
11. Consolidate broader settings, sorting, grouping, zoom,
   and other UI refinements.
12. Add media playback and optional ebook previews after the file-manager and
   desktop-integration foundation is complete.

The known PDF resize problem, interrupted permanent-delete quarantine
recovery, large-directory benchmarks, thumbnail failure presentation, and
manual sprint acceptance checks remain cross-cutting quality work rather than
optional feature ideas.

## Desktop integration and distribution

The packaging contract, current dependency caveats, target formats, and
`v0.1.0` gate are detailed in the
[release and distribution plan](release.md).

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
- [x] Implement `org.freedesktop.FileManager1` navigation support for
  `ShowItems` and `ShowFolders`.
- [ ] Route `ShowItemProperties` through the planned shared read-only
  Properties presentation.
- [x] Use one primary Marcel process per graphical session. Later CLI, desktop,
  and D-Bus requests must be routed to it without blocking GPUI's foreground
  executor.
- [x] Treat every D-Bus URI as untrusted input: accept only bounded local `file:`
  URIs, validate requested filesystem types before acting, and never interpret
  `ShowFolders` input as a request to open a regular file.
- [x] Keep ownership of the generic `org.freedesktop.FileManager1` activation name
  opt-in. Installing `pkgs.marcel` alone must not displace the user's current
  generic file manager.
- [x] Make the default package fully free and cacheable by switching its
  private backend from `_7zz-rar` to `_7zz`; keep RAR/CBR actions disabled
  unless an explicitly supplied capable backend is enabled.
- [ ] Expose RAR decoding as an explicitly unfree opt-in package variant rather
  than requiring users to assemble the backend and
  `MARCEL_ENABLE_RAR=1` override themselves.
- [ ] Add a distinct Marcel application icon in the freedesktop-required
  scalable and raster sizes. Use it in the desktop entry, window metadata, and
  AppStream metadata.
- [ ] Install and validate `io.github.berker_z.Marcel.metainfo.xml`, including
  release data, content rating, URLs, launchable desktop ID, and representative
  screenshots.
- [x] Bundle a private curated Nordzy fallback containing only Marcel's
  semantic Places and MIME icons. Prefer only an explicit Marcel theme override
  over Nordzy; use the ambient GTK theme afterward for missing-icon coverage.
  Keep the bundle under approximately 250 KiB, preserve GPL-3.0 source SVGs and
  notices, and verify precedence with automated layer-order coverage.
- [x] Bundle private regular and semibold Iosevka subsets as Marcel's default
  UI family. Target Latin, Greek, Cyrillic, punctuation, currency, arrows,
  mathematical, box-drawing, and geometric ranges in under approximately
  750 KiB total; use system font fallback for other scripts and let
  `MARCEL_FONT_FAMILY` explicitly override both typography roles.
- [ ] Audit and document the runtime closure: GIO, Poppler tools, 7-Zip,
  Fontconfig/Freetype, graphics/Wayland/X11 libraries, D-Bus activation files,
  icon lookup paths, thumbnail cache paths, and desktop portals.
- [ ] Add clean-environment package smoke tests that launch Marcel, enumerate a
  fixture directory, resolve baseline icons/fonts, render one PDF, extract one
  free archive, and verify installed desktop/AppStream/D-Bus metadata.
- [ ] Establish tagged release sources, changelog/release notes, deterministic
  versioning, release checks on `x86_64-linux` and `aarch64-linux`, and optional
  Cachix/GitHub release automation.
- [ ] Submit `marcel` to nixpkgs as a tagged-source package with a free closure,
  a maintainer, complete `meta`, and a package test. Run `nixfmt`,
  `nixpkgs-review`, the package build/tests, and use the conventional
  `marcel: init at 0.1.0` commit.
- [ ] Treat Flatpak as a separate compatibility project, not a repackaging
  checkbox: prototype host-filesystem access, portals, D-Bus activation,
  external file DnD, Trash, subprocesses, and icon discovery inside the
  sandbox. Do not advertise it until those semantics match the native package.
- [ ] Re-evaluate Flathub only if Marcel is eligible under the then-current
  submission policy. The current policy treats broad-scope file managers as
  exceptional and disallows AI-assisted application content and AI-generated
  submission work, so a compliant Flathub submission is currently blocked.
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
- [x] Accept native local-file drops from desktop applications into Places,
  bookmarks, folders, and the current browser directory, using safe copy
  semantics.
- [x] Confirm native file-drag source interoperability with browser/desktop
  targets on Wayland.
- [ ] Add and manually verify the corresponding X11 source path.
- [ ] Upstream Marcel's outbound Wayland file-drag primitive to GPUI after
  this desktop-interoperability slice lands: port the patch from GPUI 0.2.2 to
  current Zed `main`, separate drag-source payload ownership from clipboard
  state, add URI serialization tests and a minimal example, and submit a
  Marcel-independent PR with its Wayland/copy-only scope documented.
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
- [x] Expose theme, icon theme, and UI font through configured Nix packages
  plus Home Manager/NixOS modules.
- [x] Persist list/grid view and hidden-file visibility as versioned, atomic
  interaction state under the XDG configuration directory.
- Support installing or loading additional palettes without introducing
  hard-coded component colors.
- Add a UI-font selector backed by the same single font-family setting as
  `MARCEL_FONT_FAMILY`; keep bundled Marcel Iosevka as the explicit default.
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
- Improve marquee-selection edge-drag acceleration so scrolling ramps smoothly
  with pointer distance while remaining controllable near the pane edge.
- Add conventional middle-mouse autoscroll to both browser and preview panes,
  with direction/speed feedback and predictable cancellation on click, Escape,
  focus loss, or pane changes.
- Add keyboard navigation and shortcuts without making Marcel modal or
  Vim-dependent.
- Add accessible names, focus treatment, and keyboard alternatives for every
  pointer interaction.
- [x] Add an in-app theme selector on top of the existing palette system.
- [x] Make custom Marcel surfaces use the active theme radius and keep
  typography on semantic `rem`-based roles.
- [x] Replace the session-level system/Iosevka switch with one font-family
  variable shared by UI and monospace roles, defaulting to bundled Iosevka.
- [x] Measure the active monospace font for virtualized preview wrapping and
  row height instead of assuming fixed character geometry.
- Persist the typography preference currently exposed through
  `MARCEL_FONT_FAMILY`.

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

## Documentation and public presentation

- Restructure the root README into a product-first landing page:
  - short identity statement and maturity/support badge;
  - one representative hero image or short GIF before implementation detail;
  - compact feature tour and safety expectations;
  - platform-neutral installation table;
  - configuration/state distinction;
  - links to focused contributor, release, roadmap, and architecture docs.
- Record and optimize a short deterministic GIF showing navigation, filtering,
  preview, list/grid switching, and one safe file action. Avoid personal paths
  or data, provide meaningful alt text, include a static fallback image, and
  keep the repository/download cost reasonable.
- Add a small curated screenshot set covering list view, grid thumbnails,
  preview types, themes, and conventional context menus. Refresh it deliberately
  for releases rather than allowing screenshots to drift silently.
- Make installation documentation platform-neutral:
  - show a support matrix with Nix marked available today;
  - reserve stable sections for AppImage, Flatpak/Flathub, AUR, Debian/Ubuntu,
    and Fedora/RPM without publishing commands for artifacts that do not exist;
  - keep “make Marcel the default file manager” separate from installation;
  - give every packaging route the same desktop-integration and archive-policy
    contract.
- Split user installation/configuration guidance from the maintainer-facing
  release handbook. The root README should summarize; `docs/release.md` should
  retain artifact, tag, CI, repository-submission, and licensing details.
- Maintain [`docs/README.md`](README.md) as the internal documentation index
  and current-milestone pointer.
- Audit stale version pins, feature counts, sprint references, limitation
  claims, and command examples before every tag.
- Add a contributor quickstart covering the dev shell, Rust quality gate,
  release-only Nix checks, fixture policy, and GPUI/upstream licensing rules.
- Add automated Markdown-link checking and a lightweight documentation lint
  once the public structure settles.
- Preserve numbered sprint files as implementation history. Use explicit
  statuses and the docs index for the current milestone rather than rewriting
  old acceptance records to resemble a changelog.
