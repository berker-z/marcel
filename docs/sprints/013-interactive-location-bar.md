# Sprint 13: interactive location bar

**Status:** Implemented — automated acceptance is complete and the user
confirmed the normal breadcrumb/address interaction and slash presentation.
Edge-case manual acceptance remains.

## Goal

Replace the static path display with conventional clickable breadcrumbs and an
editable location mode without creating a second navigation implementation.

## Interaction contract

- The normal location bar shows the filesystem root and progressive path
  segments. Every displayed ancestor navigates through Marcel's existing
  history-aware `navigate_to` path.
- The current segment is visually active. Trash is represented as one virtual
  segment rather than leaking its backing path.
- Narrow windows retain the root and deepest segments and replace hidden
  middle ancestors with `…`. Clicking `…`, the current segment, or empty bar
  space enters location editing.
- `Ctrl+L` enters editing from the browser or current-directory filter and
  selects the complete current path. Pressing it again reselects the input.
- `Escape` abandons editing. `Enter` resolves and opens the location.
- Input accepts absolute and relative paths, `~`, `~/…`, and local
  percent-decoded `file://` URIs. Other URI schemes are refused.
- Directory targets navigate normally. Regular-file targets open their parent
  and reveal the file. Missing and unsupported targets leave the current
  directory untouched, retain the editor, and report an error.
- Filesystem metadata resolution stays off GPUI's foreground executor.
  Ticketing prevents an abandoned or superseded lookup from navigating later.

## Acceptance checks

- [x] Render ancestor segments as gpui-component text buttons with literal
  slash separators.
- [x] Route ancestor clicks through existing navigation and history.
- [x] Add compact root/ellipsis/deep-tail presentation.
- [x] Add gpui-component input editing through `Ctrl+L` and pointer activation.
- [x] Select the complete path on entry and repeated `Ctrl+L`.
- [x] Implement Enter, Escape, blur, error, and superseded-resolution behavior.
- [x] Resolve filesystem targets on the background executor.
- [x] Support relative paths, home expansion, and local file URIs.
- [x] Reveal regular-file targets in their parent folder.
- [x] Add unit coverage for breadcrumbs, compaction, path expansion, file
  targets, remote-URI refusal, and missing paths.
- [x] Manually verify normal pointer interaction, slash visibility, and
  keyboard address editing.
- [ ] Manually verify invalid-input presentation and narrow-window compaction.
- [ ] Manually verify paths containing spaces and non-ASCII characters.
