# Sprint 21: A launch is a window

**Status:** Implemented — the code slice is delivered and passes the local
quality gate. The acceptance checks below need a graphical session, and they are
now reachable without a hand-written D-Bus call, which is most of the point.

## Goal

Make running `marcel` open a Marcel, and add the folder context-menu entry that
follows from it.

## What was wrong

A second launch does not start a second process. `acquire_or_forward` hands the
request to the Marcel that is already running, and that Marcel answered it in
two ways, both wrong:

- **`marcel ~/Downloads` navigated the window the user was reading.**
  `open_desktop_locations` reused the existing window for the first location,
  so the folder they were looking at was replaced by the one they asked to
  *also* see. Nothing was gained and their place was lost.
- **`marcel` with no argument ignored them entirely.** With no location
  argument, the forwarded request was `Activate`, which raises the existing
  window and does nothing else. So `cd ~/Projects && marcel` brought forward a
  window still showing wherever it had been.

Underneath both: [Sprint 19](019-application-global-operations.md) moved
operations and user data onto the application, but opening a window stayed a
private detail of `main.rs`, and `main.rs` kept its own hand-maintained list of
windows. That is why the only way to get a second window was a D-Bus request
carrying two locations — and why
[Sprint 20](020-cleanup-interlude.md)'s acceptance matrix, which is entirely
about two windows, was something nobody could reasonably sit down and run.

## Decisions taken

- **A launch is a window.** `Open` — which is what a terminal launch, a desktop
  launcher, and "open with Marcel" all forward — always opens one. Somebody
  asked for Marcel; they get a Marcel.
- **A launch always carries a location.** With no argument it forwards the
  current directory rather than a bare `Activate`, because `marcel` in a
  terminal means "show me this folder". `launch_uris` is where that is decided.
- **A reveal may still take over a window.** "Show me where this file is"
  (`ShowItems`, `ShowFolders`) is a request about a view that already exists, so
  it reuses the window the user is looking at. This is the one case where
  reusing is the answer rather than a shortcut, and `may_reuse_a_window` states
  the rule in one place with a test on it.
- **Activating is not launching.** Clicking an application icon in a dock still
  raises the window you have. Piling up windows from a dock click would be
  wrong everywhere else, and the two signals are already distinct on the wire.
- **The application owns its windows.** `WindowRegistry` is an application-global
  entity, the same shape as `OperationCoordinator` and `BookmarkStore`.
  `main.rs`'s private `Vec<MarcelWindow>` is gone: it would have gone stale the
  moment a window could be opened from a menu, and the registry it replaces is
  now consulted by both.
- **Folders only, in the item menu, and nothing else.** No toolbar button, no
  keyboard shortcut yet. `Ctrl+N` is conventional and trivial to add later; it
  was not asked for, and unrequested surface area is how menus stop being
  legible.
- **New windows cascade.** Each opens 32px further down and right, repeating
  after eight. A tiling compositor places windows itself and ignores this
  entirely; on a floating desktop it is the difference between one window and
  two. **Unverified here** — this machine runs Hyprland, where the hint is
  discarded by design.

## Correctness contract

- Running `marcel`, with or without a path, never changes what an existing
  window is showing.
- A reveal request reuses at most one window — the one the user is looking at —
  and opens windows for any further locations.
- Every window Marcel opens is in one registry, whatever opened it.
- A closed window is never chosen to answer a request.
- Windows are surfaces, still: a new one reads the same journal, clipboard, and
  bookmarks as every other.

## Delivered scope

- [x] `src/window.rs` — `MarcelWindow`, `open`, and the application-global
  `WindowRegistry`, with pruning on window close and a `current` that skips
  windows the user has closed.
- [x] `launch::launch_uris` — a launch forwards a location whether or not it was
  given one.
- [x] `window::may_reuse_a_window` — launch versus reveal, stated once and
  tested.
- [x] `main.rs` reduced to routing: it no longer owns window creation, the
  window list, or the icon.
- [x] `BrowserCommand::OpenInNewWindow`, shown in the item context menu only
  when the selection is a single folder outside the Trash.
- [x] A window cascade, applied on open.

## Acceptance checks

### Automated

- [x] A launch does not reuse a window; a reveal does.
- [x] The cascade steps and returns to the start rather than walking off screen.
- [x] Pass `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-targets`, with
  `desktop_integration::tests::private_session_bus_integration` confirmed
  outside the sandbox. 252 library tests plus 1 binary test.

### Manual — needs a graphical session

- [ ] With Marcel open, run `marcel ~/Downloads` in a terminal. A second window
  appears at Downloads and the first window is untouched.
- [ ] With Marcel open, `cd` somewhere and run `marcel` with no arguments. A
  second window appears at *that* folder.
- [ ] Right-click a folder, choose Open in New Window, and confirm it opens
  there. Confirm the entry is absent for a file, for a multiple selection, and
  in the Trash.
- [ ] Close one of two windows and confirm Marcel keeps running, and that the
  remaining window still answers reports and questions.
- [ ] Confirm the launcher icon still raises the existing window rather than
  opening another.
- [ ] Confirm "show in folder" from another application still reveals in the
  window you are using.
- [ ] On a floating window manager, confirm the cascade actually offsets.

### Then: Sprint 20's matrix, which this makes reachable

Every two-window check in [Sprint 20](020-cleanup-interlude.md) can now be set
up with two terminal commands instead of a `gdbus call`. That list is the
remaining work before a release decision, and it is unchanged by this sprint
except in being possible to run.

## Out of scope

- **`Ctrl+N`.** Easy to add; deliberately not added.
- **Tabs.** A different feature with a different interaction model.
- **Changing what a reveal does.** Taking over the current window for "show in
  folder" is arguably also intrusive, but it is the conventional behaviour and
  changing it is a separate decision with its own acceptance run.
- **Per-window view state.** View mode and hidden-file visibility are still
  saved per window, last writer wins. With one window that was invisible; with
  two it will be noticeable, and it stays in [`TODO.md`](../TODO.md) rather than
  being pre-emptively redesigned.
