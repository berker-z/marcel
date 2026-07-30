# Sprint 15: private visual identity bundle

**Status:** Implemented — the identity and persistent-state behavior are
working in daily use; clean minimal-package visual testing remains part of the
release gate.

## Goal

Give Marcel a small, deliberate baseline appearance on any Linux desktop
without installing a font or icon theme globally and without inheriting the
ambient GTK theme ahead of Marcel's own visual identity.

## Product contract

- Marcel embeds private regular and semibold monospaced Iosevka faces and uses
  the `Marcel Iosevka` family for both UI and monospace text roles.
- `MARCEL_FONT_FAMILY` is the single explicit startup override. An empty or
  unavailable family falls back to the bundle.
- There is no footer system/Iosevka toggle.
- Icon resolution is layered across the complete semantic candidate list:
  explicit `MARCEL_ICON_THEME`, bundled Nordzy, ambient GTK, then the existing
  generic text glyph.
- The curated resources stay private to Marcel. They do not register Nordzy or
  Iosevka in the user's desktop environment.
- Built-in first-run interaction defaults are grid view and visible hidden
  files; subsequent launches restore the last selected state.
- Home Manager and NixOS users can declaratively set palette, icon theme, and
  UI font through `programs.marcel.settings`; overlay users can call
  `pkgs.marcel.withSettings`.

## Asset and licensing contract

- Iosevka 34.8.0 is subset to the documented Latin, Greek, Cyrillic,
  punctuation, currency, arrow, mathematical, box-drawing, geometric, and
  ligature ranges.
- Nordzy 1.8.7 contributes twenty unmodified scalable Places and MIME icons.
- The pinned generator verifies upstream SHA-256 hashes and preserves the OFL
  and GPL license texts.
- The Nix source closure includes the embedded font inputs and installs only
  the private icon directory and license notices.

## Acceptance checks

- [x] Generate two valid `Marcel Iosevka` faces totaling less than 750 KiB.
- [x] Load both faces from application bytes before Marcel's theme is applied.
- [x] Set UI and monospace roles from one family variable with bundled Iosevka
  as the unconditional default.
- [x] Remove the footer font switch and its coordinator state.
- [x] Curate exactly twenty Nordzy SVGs below 250 KiB, excluding license text.
- [x] Resolve all explicit-theme candidates before any bundled candidate, and
  all bundled candidates before any ambient-theme candidate.
- [x] Keep source versions, hashes, mappings, licenses, and regeneration
  instructions in the repository.
- [x] Include private runtime assets and notices in the Nix package recipe.
- [x] Export configured-package, Home Manager, and NixOS visual settings
  interfaces.
- [x] Persist view and hidden-file interaction state in a versioned, atomic XDG
  file through a serialized background writer.
- [x] Route application-specific D-Bus activation through the configured
  wrapper without changing MIME or generic FileManager1 ownership.
- [ ] Launch the installed package in a minimal desktop session and visually
  confirm bundled fonts/icons with GTK settings absent.
- [ ] Confirm a non-Latin script outside the subset uses system glyph fallback
  without affecting layout or crashing.
