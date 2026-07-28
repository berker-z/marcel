# Sprint 2: Selection and visual browsing

**Status:** Active — shared list/grid selection, virtualized icon view, native
icons, and progressive still-image thumbnails are implemented; visual
validation and interaction cleanup remain.

## Goal

Make Marcel a capable pointer-first browser for visual folders without losing
the responsiveness established in Sprint 1. Selection must behave like a
conventional desktop file manager in both list and icon views, and thumbnails
must arrive progressively without blocking navigation.

## Deliverables

- [x] A path-keyed multi-selection model independent of list or grid layout.
- [x] Conventional click, Control-click, and Shift-click selection.
- [x] Drag selection from empty browser space with a themed marquee.
- [x] Edge auto-scroll while extending a drag selection.
- [x] A list/icon view switch that preserves selection and preview state.
- [x] A virtualized icon grid that adapts its column count to available width.
- [x] Freedesktop icon-theme discovery and cached icon lookup.
- [x] MIME-specific icons with generic and built-in fallbacks.
- [x] Progressive image thumbnails for visible and near-visible grid items.
- [x] A bounded memory thumbnail cache and freedesktop-compatible disk cache.
- [ ] Explicit loading, failure, and unsupported-thumbnail states.

File move/copy drag-and-drop is a later sprint. This sprint must leave a clean
gesture boundary for it.

## Selection interaction

- A plain click selects one item, makes it primary, and updates the preview.
- Control-click toggles one item without discarding the rest.
- Shift-click selects the visible sorted range from the anchor to the clicked
  item.
- A press on empty browser space clears the selection unless Control is held
  and begins a possible marquee gesture.
- The marquee appears only after a small movement threshold, preventing normal
  clicks from becoming accidental drags.
- Items intersecting the marquee are selected. The selection at mouse-down is
  snapshotted so modifier behavior remains stable throughout the gesture.
- Mouse-up ends the gesture even if the pointer has left the browser bounds.
- Dragging from an item does not begin marquee selection. Once file
  drag-and-drop exists, dragging a selected item will start that operation.
- The most recently focused selected file owns the preview. A multi-selection
  with no distinct primary item shows a compact selection summary.

## Proposed model

Selection belongs to the browser state, not the current renderer:

```text
SelectionModel
├── selected paths
├── anchor path
└── primary path

MarqueeGesture
├── origin and current window positions
├── selection snapshot at mouse-down
└── replace or additive mode
```

Paths remain authoritative because directory batches can still arrive and
reorder visible indices. Indices are derived only when applying a range to the
current sorted view.

GPUI's normal payload drag API is intended for drag-and-drop. Marquee selection
instead uses mouse-down plus window-level mouse-move and mouse-up listeners,
following the event-listener pattern used by Zed's editor and GPUI controls.
`Bounds::from_corners` and `Bounds::intersects` provide the geometry.

Both views record entry bounds in scroll-content coordinates behind a shared
Marcel interface. At marquee start, Marcel keeps the currently visible
geometry; edge auto-scroll progressively exposes and records additional
entries. The selection rectangle therefore survives virtualization without
depending on window coordinates that change while scrolling.

## Icon view

`ViewMode::List` and `ViewMode::Grid` consume the same entries, selection, and
activation APIs. The initial grid uses fixed-width tiles and virtualizes rows:
each `uniform_list` row renders the number of tiles that fit the current
viewport. Resizing changes the column count without changing selected paths.

Tiles show, in order of availability:

1. A cached thumbnail.
2. A MIME-specific icon from the active freedesktop icon theme.
3. A generic Marcel fallback.

Controls and other application chrome continue using gpui-component icons.
Filesystem icons are a separate concern because the component set is not a
complete MIME icon theme.

## Icon-theme strategy

On Linux, Marcel should honor the user's freedesktop icon theme rather than
hard-code Breeze. It will search XDG icon locations, follow the selected
theme's inheritance chain, and end at the required `hicolor` fallback. MIME
icon names come from the shared MIME database, falling back from a specific
name to a generic media-type icon.

Breeze is an excellent supported theme and may provide a small bundled fallback
subset if needed. Any bundled Breeze assets must retain KDE's LGPL-3.0-or-later
notice and be recorded in `THIRD_PARTY_NOTICES.md`. Marcel should not bundle an
entire desktop icon theme.

The first implementation should evaluate a spec-compliant Rust lookup crate
behind a Marcel-owned `IconProvider` interface. Theme lookup and SVG/PNG
decoding must be cached and stay off the foreground executor.

## Thumbnail pipeline

- Schedule only visible and near-visible entries, ordered by viewport priority.
- Use request tickets so results for an old directory or tile size cannot
  become current.
- Bound decode dimensions and memory independently of source-file dimensions.
- Cache in memory by canonical URI, modification time, file size, requested
  size, and scale.
- Read and write the freedesktop thumbnail cache under
  `$XDG_CACHE_HOME/thumbnails`.
- Validate cached URI, modification time, and size before display.
- Respect image orientation metadata.
- Record failed generations so corrupt files are not retried every frame.
- Start with still-image thumbnails. PDF and video thumbnailers can plug into
  the same provider after their Sprint 1 preview providers exist.

## Acceptance checks

- [ ] Click, Control-click, Shift-click, and empty-space click match conventional
  desktop selection behavior in list and icon views.
- [ ] Dragging in every direction produces the same intersecting selection.
- [ ] Releasing outside the browser always ends the marquee.
- [ ] Edge dragging scrolls and can select items that began off-screen.
- [ ] Double-click activation still works and does not leave a marquee active.
- [ ] Switching view mode retains selected and primary paths.
- [ ] Opening a directory with 50,000 entries does not eagerly decode icons or
  thumbnails for every entry.
- [ ] Rapid navigation cannot publish stale thumbnails into the new directory.
- [ ] Missing themes and corrupt icons degrade to a visible fallback.
- [ ] Theme icons and thumbnails remain sharp at scale factors greater than one.

## Progress log

- Replaced the single selected-path field with a Marcel-owned selection model
  containing a path-keyed set, range anchor, and primary preview item.
- Added conventional replacement, toggle, and visible-order range selection.
- Added an empty-space marquee gesture with a movement threshold, additive
  Control mode, a palette-driven overlay, and mouse-up handling both inside and
  outside the browser.
- Added a cancellable marquee auto-scroll loop. Scroll speed increases toward
  the viewport edge and selection continues updating when the pointer is held
  still.
- Promoted active marquee movement and release tracking from the browser
  element to window-level GPUI listeners. The pointer may leave the browser
  while the gesture continues; only the painted rectangle and intersection
  area are clipped to the browser viewport.
- Replaced full-width `ListItem` entry hit targets with compact Marcel-owned
  entry surfaces. `ListItem` deliberately expands to the complete row, which
  prevented empty horizontal list space from starting a marquee. The virtual
  row remains full-width for layout and scrolling while only its visible
  contents activate the entry.
- Kept marquee intersection proportional to the number of intersected rows:
  fixed-height virtual row indices are derived from pointer geometry and the
  current scroll offset instead of scanning the directory.
- Added a Marcel-owned Linux icon provider around freedesktop theme lookup.
  Theme discovery honors `MARCEL_ICON_THEME`, GTK 4/3 configuration, and
  freedesktop fallbacks; positive and negative name lookups are cached.
- Added extension-derived specific and generic MIME icon candidates and resolve
  them during background directory loading. GPUI rows receive only resolved
  SVG/PNG paths and never perform filesystem lookup during paint.
- Added unit coverage for selection replacement, toggling, bidirectional ranges,
  additive marquee snapshots, rectangle normalization, edge-scroll direction,
  and MIME icon fallback ordering.
- Added compact gpui-component toolbar controls for switching between list and
  icon views. Selection, primary item, and preview state remain path-keyed and
  survive the renderer change.
- Added a responsive icon grid that virtualizes fixed-height rows and derives
  its column count from the current browser width. Tiles use the same click,
  modifier-selection, double-click activation, and marquee APIs as list rows.
- Reserved explicit grid gutters and independent image/label geometry so the
  final column cannot cross the viewport edge and three-line filenames remain
  contained inside their tiles.
- Added a single-worker thumbnail scheduler for visible and one-row-near-visible
  images, then replaced it with a Yazi-inspired two-worker viewport-priority
  scheduler. A changed viewport rebuilds all not-yet-started work so old
  look-ahead cannot block visible tiles; at most two already-running decodes
  may finish.
- Directory tickets prevent old work from publishing after navigation. Decode
  source bytes, dimensions, pixels, and allocation are bounded through the
  decoder itself, and orientation is applied after downscaling to avoid
  transforming the full-resolution allocation.
- Added a 512-entry in-memory thumbnail-result cap. Evicted entries remain
  cheap to revisit through the standard freedesktop `normal` thumbnail cache,
  keyed by the MD5 of the canonical file URI and validated with `Thumb::URI`,
  `Thumb::MTime`, and `Thumb::Size`.
- Cache validation accepts freedesktop metadata stored in PNG `tEXt`, `zTXt`,
  or `iTXt` chunks, allowing thumbnails produced by other compliant desktop
  applications to be reused. New thumbnails use the PNG crate's fast
  compression profile.

## Study and provenance notes

- Zed is used only to study GPUI's window-level pointer listener and gesture
  state patterns. No GPL-licensed Zed application code is to be copied.
- The freedesktop Icon Theme, Icon Naming, Shared MIME-info, and Thumbnail
  Managing specifications define Linux integration behavior.
- Thumbnail preloading and decode-flow adaptations use Yazi commit
  `e58022b9aafc8dabf586e2cc29b79a230071716f`. Exact upstream and Marcel files
  are recorded beside the adapted code and in `THIRD_PARTY_NOTICES.md`.
