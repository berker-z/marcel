# Interaction and command model

This document defines Marcel's user-facing command behavior across keyboard
shortcuts, context menus, and future toolbar actions. Sprint documents decide
when each command is implemented; this is the cross-sprint interaction
contract.

The navigation and selection shortcuts below are implemented. New Folder and
its bounded undo/redo path are the first active file operations; the remaining
file-operation shortcuts stay inactive until their transaction and undo
behavior exists.

## Principles

- Marcel is a conventional, pointer-friendly graphical file explorer.
- Keyboard interaction supplements pointer interaction and is not modal.
- A command has one implementation regardless of whether it is invoked from a
  shortcut, context menu, toolbar, or application menu.
- The primary selection is the keyboard navigation anchor.
- Destructive operations must not be easier to trigger than recoverable ones.
- File operations run outside GPUI's foreground executor and report progress,
  partial failures, conflicts, and cancellation.

## Navigation and selection shortcuts

| Shortcut | Command |
| --- | --- |
| `Arrow keys` | Move the primary selection in the current view |
| `Shift+Arrow keys` | Extend selection from the selection anchor |
| `Home` / `End` | Select the first / last item |
| `Page Up` / `Page Down` | Move selection by approximately one viewport |
| `Shift+Home` / `Shift+End` | Extend selection to the first / last item |
| `Shift+Page Up` / `Shift+Page Down` | Extend selection by approximately one viewport |
| `Enter` | Enter a folder or open a file |
| `Escape` | Clear selection or dismiss the active transient UI |
| `Ctrl+Up` | Go to the parent folder |
| `Ctrl+Left` | Go backward in navigation history |
| `Ctrl+Right` | Go forward in navigation history |
| `Ctrl+A` | Select all items in the current directory |
| `Ctrl+Shift+N` | Create a folder in the current directory |

Plain left/right behavior may differ between list and icon views, but the
Control-modified navigation commands above take precedence in both.

## Current-directory filtering

- Typing printable text anywhere in Marcel starts or extends the current
  directory filter and focuses the top-bar input.
- An explicitly focused text field takes precedence over type-to-filter.
  Dialog inputs and future inline editors receive their editing keys without
  changing or focusing the directory filter.
- `Ctrl+F` focuses the same input without changing its contents.
- Matching is case-insensitive and fuzzy. Contiguous, word-boundary, and early
  matches rank ahead of scattered matches.
- Up and Down move through the filtered results; Enter activates the primary
  result.
- Backspace edits an active query even if focus moved elsewhere. Escape clears
  the query and restores the complete directory.
- Navigating to another directory clears the query. Refreshing the same
  directory preserves it.
- Selection is restricted to visible results. Items hidden by a changed query
  are removed from the selection so future file commands can never operate on
  invisible selections.
- List and icon views, range and marquee selection, Select All, thumbnails, and
  keyboard navigation consume the same filtered visible-index order.

This feature filters entries already enumerated in the displayed directory. A
recursive filename/content search is a separate future feature.

## File-operation shortcuts

| Shortcut | Command |
| --- | --- |
| `Ctrl+C` | Copy the selected files |
| `Ctrl+X` | Cut the selected files for a later move |
| `Ctrl+V` | Paste into the current directory |
| `Ctrl+Z` | Undo the most recently completed reversible operation |
| `Ctrl+Y` | Redo the most recently undone operation |
| `Delete` | Move the selection to Trash |
| `Shift+Delete` | Permanently delete after explicit confirmation |

Copy and cut should integrate with the desktop clipboard so transfers can
eventually interoperate with other file managers. Marcel may begin with an
internal clipboard, but the command interface must not depend on that
implementation detail.

## Context-menu selection

- Right-clicking an unselected item makes it the sole selection before opening
  the menu.
- Right-clicking an item that is already part of a multi-selection preserves
  the complete selection and makes the clicked item primary.
- Right-clicking empty browser space clears the item selection and opens a
  current-directory menu. Its commands operate on the displayed directory, not
  on the former selection.
- Menu item availability is derived from the same command state used by
  shortcuts and other action surfaces.

`Open` and `Open With…` act only on the primary item: for a context menu, that
is the item that was right-clicked. Future batch commands such as Cut, Copy,
Move, Trash, Delete, and Compress act on the complete preserved selection.
Single-item commands such as Rename are disabled for multi-selection.
Properties shows one item's details for a single selection and an aggregate
summary for multiple items.

The first context-menu shell exposes the intended item command set while item
operations remain read-only. `Open` uses the configured MIME default without
prompting, while `Open With…` explicitly requests the desktop application
chooser. Planned commands are disabled and prefixed with `–`: Cut, Copy, Paste,
Duplicate, Rename, Move To, Move to Trash, Delete Permanently, Create Link,
Compress, Copy Path, and Properties. A planned command loses the prefix only
when its implementation, enabled-state rules, error handling, and any required
undo record are ready.

## Current-directory context menu

The empty-space menu is deliberately different from the item menu. Its proposed
initial order is:

1. New Folder
2. New File
3. Paste
4. Undo
5. Redo
6. Select All
7. Refresh
8. Show Hidden Files
9. Open in Terminal
10. Copy Location
11. Properties

Separators group creation and clipboard actions, selection and refresh,
visibility, and directory utilities. Paste is disabled when the clipboard has
no compatible file payload. Properties describes the displayed directory. Show
Hidden Files is a checked toggle. New Folder, Undo, and Redo use the same
command state as `Ctrl+Shift+N`, `Ctrl+Z`, and `Ctrl+Y`.

View mode belongs in the persistent Places footer rather than this context
menu. Marcel exposes a list/grid switch there. Future sorting controls should
use persistent application chrome rather than expanding the context menu.

Show Hidden Files is active both in the Places footer and this menu. It toggles
Unix dotfiles through the shared visible-index layer and safely removes newly
hidden paths from selection.

Open in New Tab, Open in New Window, and Add to Places are appropriate later
additions once Marcel has tabs, reliable multi-window launching, and editable
Places. Extension or desktop-service actions should eventually live in an
Actions submenu rather than expanding the top-level menu without bound.

## Undo and redo

Marcel keeps two bounded in-memory stacks:

- the undo stack contains completed reversible operations;
- the redo stack contains operations successfully undone during the current
  history branch.

Completing a new file operation clears the redo stack. Navigation and selection
changes do not affect either stack.

The top-left toolbar exposes Undo and Redo beside navigation. Their enabled
states come from the same operation journal and busy state as `Ctrl+Z` and
`Ctrl+Y`. Refresh remains in the current-directory context menu.

History stores operation records, not file contents. Representative records
include:

- rename: old and new paths;
- move or cut/paste: each source and destination pair;
- copy or duplicate: destinations created by the operation;
- create: the created path and expected type;
- trash: the original path and its freedesktop Trash entry.

Only completed filesystem effects enter history. If a multi-file operation
partially succeeds, its record contains exactly the successful subset and the
UI reports every failure.

Before undoing or redoing, Marcel validates that the affected paths still match
the recorded post-operation state. If another process has replaced, modified,
or occupied a path, Marcel must stop and explain the conflict rather than
overwrite data.

Initially, file operations should be serialized. This makes transaction
boundaries, conflict handling, and undo ordering deterministic while work still
runs away from the UI thread.

### Reversibility rules

- Rename and same-filesystem moves reverse by moving the recorded destination
  back to its original path, after conflict checks.
- Copy undo removes only the copies created by Marcel and only when validation
  confirms they are still those outputs. Recoverable Trash is preferred over
  permanent removal.
- Create undo removes only the exact directory Marcel created and only while it
  remains empty. Later create variants may prefer Trash where that produces a
  stronger recovery contract.
- Trash undo restores from the recorded Trash entry to the original path after
  conflict checks.
- Redo replays the validated forward operation and creates a fresh resulting
  record.
- Permanent deletion is explicitly non-reversible and never masquerades as an
  undoable action.

The initial history is session-local and bounded by record count and metadata
size. A crash-safe persistent operation journal can be considered later, but it
must not be required for trustworthy in-session undo.
