# Marcel product backlog

This is Marcel's cross-sprint backlog and product roadmap. Sprint documents
under [`docs/sprints/`](sprints/) turn selected items into bounded deliverables
and detailed acceptance checks. Finishing a sprint does not imply that every
item here belongs in that sprint.

Sprint status uses four consistent meanings:

- **Implemented** means the planned code and automated checks are complete;
  explicitly listed manual acceptance may still be pending.
- **Implemented core** means the sprint's product foundation is complete while
  a named follow-up remains in the hardening or parked-feature backlog.
- **Partially implemented** means named deliverables were deliberately parked.
- **Planned** means delivery has not started. Sprint 16 is also explicitly
  deferred while hardening remains the priority.

## Current priorities

- Keep feature and release work frozen. The hardening code queue is now closed;
  the next work is the graphical acceptance matrix in
  [`Sprint 20`](sprints/020-cleanup-interlude.md) and whatever it reveals.
  Nothing on that list can be reached by `cargo test`, and automated checks have
  now twice been green over defects a reader found by hand.
- The matrix has started running. The first pass is recorded in
  [`acceptance-2026-08-21.md`](acceptance-2026-08-21.md), driven through
  hyprhands. Seven checks pass and two fail:
  - **A1, fixed the same day.** Revealing a file deep in a folder that was
    still loading selected and previewed the right file but left it off
    screen, at a different wrong offset each run. Sprint 22's E7 fixed the
    loaded case and left the streaming one. The reveal now records its target
    while the stream owns the listing and re-applies the *scroll* at `Done`,
    after the deferred refresh batch. Four regression tests, and re-verified
    graphically. Sprint 22 had recorded this exact check as delivered, so the
    matrix caught something a sprint believed it had closed.
  - **A2, cosmetic and still open.** The "no preview available" placeholder
    neither wraps nor elides in a narrow preview pane, so it is clipped at
    both edges.
  - **A3, fixed the same day.** Escape did not close a context menu. It
    cleared the selection underneath instead, leaving the menu on screen with
    every action greyed out. Escape now consumes the frontmost surface first,
    in both `on_clear_selection` and `on_window_key_down`. Reproduced by hand
    before it was fixed, and re-verified after.
  - Extraction refuses a taken destination and says so, where copy and move
    offer replace, rename, skip, or merge. Refusing safely is defensible for
    a first release; making the two consistent is post-0.1 work.
  - Screenshot-driven runs cannot see notification cards at all. Successful
    and failed operations both look silent through that lens, so anything
    involving a notification needs a person watching the screen.
  - Drag and drop could not be driven at all: hyprhands needs ydotool for
    press-move-release and this machine does not have it. That part of the
    matrix still needs a person.
- Release paperwork is no longer the gap it was. AppStream metadata,
  a changelog, a version-consistency check, and a hosted CI gate all exist now;
  see the [release plan](release.md). What is left before a tag is mostly
  verification rather than authorship.
- A fourth review is cross-checked in
  [`review-2026-08-18.md`](review-2026-08-18.md), the first to read the tree
  Sprints 18 and 19 produced. All four of its findings reproduced, one of them a
  data-loss path where a failed rollback left the user's only copy in storage a
  later Marcel would sweep. A fifth defect in the same function — a cancelled
  merge reported as a failure — is recorded there and was missed by the review.
  All five are closed in Sprint 20.
- A fifth review, [`review-2026-08-20.md`](review-2026-08-20.md), read the
  whole tree rather than the operations core and found its defects almost
  entirely outside it — the load/watcher seam, the preview surface, bookmarks
  persistence, the D-Bus surface, and the window layer. All confirmed findings
  are fixed with regression coverage in
  [`Sprint 22`](sprints/022-read-the-whole-tree.md), which also adds its own
  short list to the graphical acceptance matrix; its four deliberately
  deferred items are folded into the lists below.
- The plan in [`review-2026-08-10.md`](review-2026-08-10.md) is complete through
  Stage 5 and Stage 7. Stage 6 is partly done: every transfer path now shares
  one bounded snapshot budget, and the journal-wide budget it asks for remains
  open. Stage 8, hosted CI, is Sprint 16 scope. One finding stays rejected: its
  permanent-delete change would report a provably complete deletion as failed.
- The critical and high findings in
  [`review-2026-08-05.md`](review-2026-08-05.md) are fixed and covered by
  regression tests. Its lower-tier backlog is now closed except for these:
  - Decide and document a setuid/setgid/sticky policy for copy in
    [`copy-semantics.md`](copy-semantics.md); Marcel currently preserves them
    where `cp` does not.
  - Compare device identity when deciding drop acceptance so cross-filesystem
    targets read as refused rather than accepted-then-failed. Sprint 20 declined
    to do this the cheap way: the acceptance predicate runs while the pointer
    moves, so it needs a device cached for the life of the drag session, plus a
    graphical run to confirm the hover styling. The failure message is already
    accurate, so what is missing is the affordance, not the outcome.
  - Reuse one `IconProvider` per watcher instead of rebuilding it per batch.
  - Optional cleanup: one process-level coalescing writer for browser state,
    replacing the per-window writers. Bookmarks no longer need this — Sprint 19
    gave them an application-global store, because last-writer-wins there was
    silent user-data loss rather than a benign race. Browser view state can keep
    last-writer-wins.
- Continue mechanical ownership cleanup only where a concrete interaction or
  test seam requires it. Preview, sidebar, drag/drop, and cohesive window UI
  state now have controllers; GPUI-bound orchestration intentionally remains
  on `Marcel` rather than moving behind an event bus or speculative traits.
- Marcel's local-filesystem core has reached the personal daily-driver
  milestone: browsing, previews, selection, filtering, bookmarks, incremental
  watching, safe copy/move, Trash/restore, permanent deletion, archives,
  packaging, D-Bus activation, single-instance routing, and bilateral Wayland
  file drag-and-drop are implemented.
- Bilateral Chrome/Marcel dragging was manually reconfirmed on Wayland after
  replacing the private GPUI fork with the upstream drag lifecycle.
- The remaining Sprint 16 work—mandatory hosted CI, public presentation,
  release metadata, distribution expansion, and a tagged `0.1.0`—is explicitly
  deferred. The application icon identity slice is complete. Local fmt, Clippy,
  and all-target tests remain mandatory meanwhile.
- New File, Properties, Duplicate, Move To, media playback, remote locations,
  X11 outbound drag, and other feature work are parked until this hardening
  phase is accepted.
- Finish the remaining list-view repetition from the Trash/restore matrix in
  [`Sprint 7`](sprints/007-trash-and-restore.md). Home and mounted Trash,
  read-only failure, identity-validating Undo/Redo and Restore, occupied restore
  refusal, and external freedesktop Trash interoperability pass.
- Finish the remaining list-view/item-menu repetition from
  [`Sprint 8`](sprints/008-permanent-deletion.md). Confirmation cancellation,
  confirmed deletion, Empty Trash, partial failure, process interruption, and
  startup quarantine guidance pass.
- Retain New File and Properties in
  [`Sprint 9`](sprints/009-conventional-local-actions.md) as parked feature
  work; safe inline Rename and Open in Terminal are implemented.
- The Sprint 4 large-directory check passes at 50,000 entries in list and grid
  views, including viewport retention across view switches.
- Finish only the unchecked watcher edge cases in Sprint 5. External create,
  rename, filtering, hidden entries, watcher replacement, rapid navigation,
  and large-directory responsiveness pass.
- Complete Sprint 1 preview and navigation acceptance runs.
- Keep the working but visually imperfect PDF resize behavior parked until the
  remaining hardening and feature queues are exhausted.
- [x] Add interactive breadcrumb navigation and a `Ctrl+L` editable location
  mode for paths and local file URIs.
- Finish Sprint 2 visual-browsing acceptance checks and thumbnail failure
  presentation.

## Deferred `v0.1.0` closure

The personal daily-driver milestone is complete. Public release work is parked
during hardening; when resumed, its remaining scope is intentionally bounded:

1. Harden the distribution closure: free archive baseline, packaged icon
   fallback, AppStream metadata, clean-environment launch checks,
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

This order favors evidence-driven hardening over novelty. Steps after the
manual acceptance phase are intentionally parked, not current commitments.

1. Run [Sprint 20](sprints/020-cleanup-interlude.md)'s graphical acceptance
   matrix, together with [Sprint 21](sprints/021-a-launch-is-a-window.md)'s
   shorter one — the two-window ownership checks inherited from Sprint 19, Sprint
   18's remaining interaction checks, and Sprint 17's remaining pointer and
   marquee checks. This is the only thing standing between the current tree and
   a release-readiness decision.
2. Fix any correctness, recovery, diagnostics, or ownership problems exposed
   by that matrix, with focused regression coverage.
3. When release work resumes, complete
   [Sprint 16](sprints/016-public-release-presentation.md), including mandatory
   hosted fmt, Clippy, tests, and Nix package-build gates.
4. Complete the distribution-hardening checklist below and test the flake
   install on a clean minimal NixOS environment.
5. Cut `v0.1.0` and prepare the nixpkgs package.
6. Close Sprint 14's remaining Properties surface without changing the user's
   default directory handler merely by installing Marcel.
7. Add New File.
8. Add Duplicate and Move To.
9. Finish X11 source support and manual acceptance for the implemented
   bilateral native desktop drag-and-drop, then add desktop clipboard
   interoperability.
10. Add cross-filesystem transfers, explicit conflict decisions, and the
   documented symbolic-link policy.
11. Add removable volumes, mounts, and common remote locations.
12. Consolidate broader settings, sorting, grouping, zoom,
   and other UI refinements.
13. Add media playback and optional ebook previews after the file-manager and
   desktop-integration foundation is complete.

Interrupted permanent-delete quarantine recovery, large-directory benchmarks,
thumbnail failure presentation, and manual sprint acceptance checks remain
cross-cutting quality work rather than optional feature ideas. The working PDF
resize quirk is explicitly deferred until higher-value work is exhausted.

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
- [x] Install a freedesktop desktop entry, initially using the generic
  file-manager icon.
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
  opt-in. Installing `pkgs.marcel-rs` alone must not displace the user's current
  generic file manager.
- [x] Make the default package fully free and cacheable by switching its
  private backend from `_7zz-rar` to `_7zz`; keep RAR/CBR actions disabled
  unless an explicitly supplied capable backend is enabled.
- [ ] Expose RAR decoding as an explicitly unfree opt-in package variant rather
  than requiring users to assemble the backend and
  `MARCEL_ENABLE_RAR=1` override themselves.
- [x] Add a distinct Marcel application icon in the freedesktop-required
  scalable and raster sizes. Use it in desktop entries and X11 window metadata;
  reuse the same icon name when AppStream metadata lands.
- [x] Install and validate `io.github.berker_z.Marcel.metainfo.xml`, including
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
  versioning, release checks on `x86_64-linux` and `aarch64-linux`, and hosted
  release automation that publishes exact derivations to a signed Nix binary
  cache alongside the GitHub Release.
- [ ] Submit `marcel-rs` to nixpkgs as a tagged-source package with a free
  closure, a maintainer, complete `meta`, and a package test. Run `nixfmt`,
  `nixpkgs-review`, the package build/tests, and use the conventional
  `marcel-rs: init at 0.1.0` commit. Both collisions with the existing
  `pkgs.marcel` are already resolved upstream of this: the command, `pname`,
  `mainProgram`, flake attribute, and overlay attribute are all `marcel-rs` as
  of 2026-08-21. Still needs a nixpkgs maintainer entry and
  `passthru.updateScript`.
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
  ordinary folders — and so the Trash view can participate in Back/Forward
  history, which currently skips over it
  ([`review-2026-08-20.md`](review-2026-08-20.md)).
- Give watcher-triggered rescans a backoff, and preserve the selection and any
  in-progress rename across them. A churning directory (a build tree) can loop
  full reloads today, each one clearing the user's selection
  ([`review-2026-08-20.md`](review-2026-08-20.md)).
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
  duplicate and move-to. Cross-filesystem cut/paste remains explicitly parked
  until its safety and UX work is scheduled.
- [x] Ask about an occupied destination instead of failing, with skip, rename,
  replace, and cancel, each able to answer the rest of the operation. Never
  overwrite without an explicit decision, hold a replaced item aside so undo
  can restore it, and refuse a transfer whose destination is its own source.
  See [`Sprint 18`](sprints/018-destination-conflict-decisions.md). Merging a
  folder into an existing one is implemented for copying, and
  [`Sprint 20`](sprints/020-cleanup-interlude.md) made a merge that stops part
  way describable and a cancelled one honest. Merging while *moving* remains a
  deliberate gap: it is a recursive move with per-leaf decisions, which is a
  design question rather than a cleanup.
- [x] Accept native local-file drops from desktop applications into Places,
  bookmarks, folders, and the current browser directory, using safe copy
  semantics.
- [x] Reconfirm native file-drag source interoperability with Chrome on
  Wayland after the upstream GPUI migration, in both directions.
- [ ] Add and manually verify the corresponding X11 source path.
- [x] Replace Marcel's private outbound Wayland drag patch with GPUI's upstream
  `ExternalDragPayload::Files` lifecycle. Marcel now accepts GPUI's standard
  Copy-or-Move source negotiation; `Cargo.lock` pins the integrated revisions.
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
- Bound the folder preview's child listing: its per-batch merge is quadratic
  on the foreground executor and the full listing is retained unbounded, which
  a very large hovered directory turns into real degradation. Deciding what a
  bounded glanceable preview shows is the product half of the fix
  ([`review-2026-08-20.md`](review-2026-08-20.md)).
- Key freedesktop thumbnails by the original URI as the spec requires, rather
  than the canonicalized path; today Marcel and other applications regenerate
  instead of sharing thumbnails for symlinked paths. Interop only
  ([`review-2026-08-20.md`](review-2026-08-20.md)).
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
- [x] Open a window per launch: running `marcel`, with or without a path, opens
  a window rather than navigating the one already in front of the user, and a
  folder's context menu offers Open in New Window. See
  [`Sprint 21`](sprints/021-a-launch-is-a-window.md). `Ctrl+N` and tabs are
  deliberately not part of it.
- Per-window view state is still last-writer-wins, which was invisible with one
  window and will be noticeable with two: set grid view in one window, close the
  other, and it can snap back. Left alone until it actually irritates someone.
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

- [x] Restructure the root README into a product-first landing page: identity
  statement, alpha status and safety expectations, feature tour, keyboard
  shortcuts, honest gap list, installation, declarative settings, credits, and
  licensing. Maintainer detail moved out rather than being summarized twice, and
  the roadmap prose is gone because this file is the roadmap.
- [x] Add a hero screenshot, at `docs/screenshots/marcel.png`. A short GIF of
  navigation, filtering, and one safe file action is still wanted and is the
  better artefact; the still is what exists today.
- The support matrix and the reserved sections for artifacts that do not exist
  are deliberately absent. Nix is the only route today and the README says so in
  one line; a table of empty rows advertises formats Marcel does not ship.
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
