# Sprint 11: visual hierarchy polish

**Status:** Active — the first shell, typography, and grid-density pass is
implemented; manual visual acceptance remains.

## Goal

Make Marcel's three-pane structure immediately legible and keep file and place
labels comfortably readable across list and grid views.

This pass changes visual hierarchy, sizing, and the first appearance setting.
It does not add pane controls, view zoom, persistent settings, or new
filesystem behavior.

## Visual contract

- In the Nord palette, the top bar, Places pane, and preview pane form one
  darker outer shell.
- The center file browser is a distinct, slightly lighter working surface.
- Place and bookmark names use Marcel's base UI text size, matching file names
  rather than secondary metadata.
- Grid file names use the base UI text size and may occupy three lines before
  elision. Extensions remain visible when a long name must be shortened.
- Grid icons and thumbnails use more of the tile footprint without changing
  the five-column density available at the reference browser width.
- A Settings button sits to the right of the list/icon switch. Its modal
  contains a theme dropdown, and selecting a palette repaints Marcel
  immediately without closing the modal or changing typography.
- Built-in themes are Nord, Gruvbox Dark, Tokyo Night, Catppuccin Mocha,
  Dracula, One Dark, Solarized Dark, Everforest Dark, Rosé Pine, Kanagawa Wave,
  System Dark, and System Light.

## Acceptance checks

- [x] Invert the Nord shell/browser surface hierarchy.
- [x] Preserve visible hover and selection states on the lighter browser.
- [x] Raise Place, bookmark, and grid-item labels to the base text size.
- [x] Recalculate the Places pane width using its actual label font size.
- [x] Expand grid visuals from 88 to 104 px and fallback icons from 56 to 80
  px.
- [x] Allocate three base-sized lines to grid labels, set an explicit compact
  line height so the third line cannot clip, and update tile geometry.
- [x] Replace the Nord-only color path with a named semantic palette registry.
- [x] Add a gpui-component Settings button, modal, and theme dropdown.
- [x] Hot-apply dropdown changes to all windows while preserving the active
  fonts, font sizes, and radii.
- [x] Keep `MARCEL_THEME` compatible and expand it to every built-in palette.
- [x] Give enabled bookmark, entry, and empty-space context-menu rows a
  theme-aware hover tint that remains visible over the popover surface.
- [ ] Manually verify every built-in theme and live switching while the modal
  remains open.
- [ ] Manually verify the result in list and grid views at the reference window
  size.
- [ ] Manually verify long ASCII and non-ASCII names, selection, thumbnails,
  inline Rename, and narrow-window behavior.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets`.
