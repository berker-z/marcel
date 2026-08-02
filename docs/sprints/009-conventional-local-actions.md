# Sprint 9: conventional local actions

**Status:** Partially implemented — safe inline Rename and Open in Terminal are
delivered. New File and Properties are explicitly parked feature work.

## Goal

Close the most visible local-file-manager gaps before a dedicated UI-fix pass:

1. Rename;
2. New File;
3. Open in Terminal;
4. Properties.

Duplicate, Move To, cross-filesystem transfers, interactive conflict
decisions, archives, and native desktop drag-and-drop remain separate work.

## Rename audit

Yazi was audited at upstream commit
`319f90e0eab185a231eef5562215ba322e320286`:

- `yazi-actor/src/mgr/rename.rs` owns the input lifecycle, places the cursor
  before a file extension, coordinates with the watcher, refreshes the file
  model, reveals the result, and publishes a rename event.
- `yazi-fs/src/op.rs` emits targeted old-name removal and new-name upsert
  events.
- `yazi-vfs/src/engine/engine.rs` and
  `yazi-fs/src/engine/local/local.rs` dispatch same-service rename to the
  selected filesystem engine.

Marcel conceptually adopts the focused name editor, extension-preserving
selection, off-foreground execution, reveal-after-success, and watcher/model
coordination. No Yazi code was copied.

Marcel deliberately does not adopt Yazi's overwrite confirmation. Rename uses
Linux `RENAME_NOREPLACE`; an occupied destination is always refused.

## Rename contract

- `F2` and the item context menu dispatch the same Rename command.
- Rename is available only for one ordinary filesystem selection. It is
  disabled for multi-selection, Trash, operation-busy state, and filenames
  that cannot be represented by the current UTF-8 editor.
- The gpui-component editor replaces the filename in both list and icon views.
  It receives typing instead of Marcel's global directory filter.
- A file's stem is selected while its final extension remains in place.
  Directory names and dotfiles without a separate stem/extension boundary are
  selected completely.
- Enter or focus loss submits. Escape cancels through Marcel's shared transient
  dismissal command.
- Names cannot be empty, whitespace-only, `.`, `..`, contain `/`, or contain a
  null character.
- Work runs away from GPUI's foreground executor. Immediately before rename,
  Marcel validates the source identity and checks that the destination is
  unoccupied.
- Publication uses same-directory Linux `RENAME_NOREPLACE`; no destination is
  overwritten, merged, or silently renamed.
- Undo validates the exact renamed identity and requires the original name to
  remain free. Redo applies the same checks in the opposite direction.
  Modification after rename conservatively makes Undo unavailable for that
  record.
- Success incrementally removes the old path, revalidates/upserts the new path
  off the foreground executor, and reveals/selects it without replacing the
  active directory session or losing scroll position.

## Acceptance checks

- [x] Add a shared Rename command and `F2` binding.
- [x] Activate Rename in the item context menu for exactly one ordinary item.
- [x] Render a focused gpui-component inline editor in list and icon views.
- [x] Preserve the final file extension in the initial selection.
- [x] Keep the global type-to-filter handler out of focused rename editors.
- [x] Implement identity-validating, no-overwrite background rename.
- [x] Add bounded Undo/Redo support.
- [x] Add tests for successful Undo/Redo, occupied destinations, modified
  results, and extension selection.
- [x] Route Rename, Undo, and Redo through incremental operation-result
  reporting instead of a full directory reload.
- [ ] Manually verify Enter, focus-loss submission, and Escape cancellation in
  both list and icon views.
- [ ] Manually verify names containing spaces, dots, and non-ASCII characters.
- [ ] Manually verify watcher reconciliation and selection reveal after Rename,
  Undo, and Redo.
- [ ] Implement New File.
- [x] Implement Open in Terminal through `xdg-terminal-exec`, `TERMINAL`, and
  bounded terminal-emulator fallbacks without leaking the Nix development
  shell's native-library search path.
- [ ] Implement single- and multi-selection Properties.

## UI-fix handoff

After these four conventional actions, stop adding filesystem features and run
a dedicated UI-fix pass. Triage actionable visual and interaction reports from
alpha testing; the non-blocking PDF resize quirk remains explicitly parked.
