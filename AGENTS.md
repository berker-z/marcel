# Marcel contributor guidance

## Product

Marcel is a conventional, pointer-friendly graphical file explorer. Its
defining qualities are responsiveness and a persistent, excellent preview
pane. It is not a terminal file manager transplanted into a window and does not
require modal or Vim-like interaction.

Work is organized into numbered sprint documents under `docs/sprints/`.
Keep the active sprint's acceptance checks current as implementation decisions
change.

## Upstream reuse

Yazi is a respected, explicit upstream influence. Reusing its MIT-licensed
ideas and code is encouraged when that gives Marcel a proven, fast
implementation.

When adapting meaningful code from Yazi or another upstream:

1. Preserve all required copyright and license notices.
2. Add a concise source comment with the upstream repository and original file.
3. Record the source, license, files affected, and nature of the adaptation in
   `THIRD_PARTY_NOTICES.md`.
4. Prefer adapting behind Marcel-owned interfaces instead of coupling the
   application to unstable upstream internals.

Use Zed's source as practical GPUI documentation. Zed application code is
primarily GPL-3.0-or-later; do not copy it into Marcel unless the specific file
is marked with a compatible license or Marcel's licensing policy changes.
GPUI's Apache-2.0 code and examples may be reused with their required notices.

## UI implementation

Use gpui-component by default for controls, layout primitives, dialogs, menus,
inputs, lists, tables, Markdown, and other components it already implements.
Do not build a custom replacement without a concrete reason such as a measured
performance problem, missing required interaction, or accessibility limitation.
Document that reason in the code or active sprint.

Keep filesystem enumeration, metadata loading, decoding, PDF rasterization, and
media inspection off GPUI's foreground executor. Preview work must be
cancellable or safely superseded, bounded in memory use, and unable to publish
stale results as current.

## Picker windows

A file-chooser window (the portal backend's open and save dialog) is not a
separate UI. It is the same `Marcel` view with `picker: Some(_)`, rendered by
the same `render()`, browsing the same `DirectorySession`. Anything added to
the browser, such as sorting, a new view, a top-bar control, or a context-menu
command, appears in pickers automatically; nothing has to be replicated.

The only picker-specific code is the handful of `self.picker` checks: what
activating a file means, Escape, the Trash place, single selection, and the
bottom bar. Do not add a `picker` branch to a new feature unless the dialog
genuinely needs different behaviour, and when it does, keep the branch next to
the others so the list stays greppable.

## Keyboard shortcuts

The README carries the user-facing list of shortcuts. That list is what people
read instead of the source, so update it in the same change that adds, removes,
or rebinds one. A shortcut that only exists in the code is a shortcut nobody
knows about, and a documented one that no longer works is worse.

Shortcuts are registered in two places, so check both before editing the list:

- The `browser_commands!` table in `src/app/actions.rs` declares every command
  once: its GPUI action, its key bindings if it has any, and the `BrowserCommand`
  menus and toolbar buttons dispatch through. Enablement lives in
  `command_enabled` and behaviour in `execute`, in the same file.
- `on_window_key_down` in the same file handles the ones that must keep working
  while another surface holds focus, currently `Ctrl+L` and `Ctrl+F`.

## Module map

`src/lib.rs` carries the map. In short: `app/` is the window (one `Marcel`
view split by concern — navigation, edits, pointer, preview, menus, sidebar,
chrome), `browse/` is the read side (entries, the directory session, the
watcher, selection, history), `fsops/` is everything that changes the disk
(`local` primitives, `identity`, `journal`, then the mutations), `preview/`
decodes, `desktop/` speaks to the session (bus, portal, launch, icons, places),
and `operations` is the application-wide owner of running work and history.
Tests share `src/testing.rs` (`Sandbox` and friends) rather than building
trees by hand.

## Quality checks

Before considering a change complete, run:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Do not run a full `nix build` or `nix flake check` after every routine update.
Reserve those expensive package checks for release commits or when the user
explicitly requests them. During ordinary development, use focused Nix
evaluation checks only when a change touches flake or package definitions.
