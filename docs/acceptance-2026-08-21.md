# Graphical acceptance run, 2026-08-21

Recorded: 2026-08-21. Tree: `c2ffabb` plus the release-readiness changes made
in the same session. Driven through
[hyprhands](https://github.com/berker-z/hyprhands), an MCP server for Hyprland,
against a debug build on Wayland at 2560x1440.

This is the first entry in what TODO.md calls the graphical acceptance matrix:
the checks no `cargo test` can reach, and the thing the backlog names as
standing between the tree and a release-readiness decision. It covers part of
that matrix, not all of it. What was not run is listed at the end.

The accessibility bus was down on this machine, so everything below was driven
by pointer, keys, and screenshots rather than by the semantic tools. That is
worth knowing when reading the results: they describe what was actually on
screen.

## Verdict

| # | Check | Source | Result |
|---|---|---|---|
| 1 | A writerless FIFO named `something.png` in a browsed grid | Sprint 22 (E2) | Pass |
| 2 | Reveal a file deep in a large folder, selected *and* on screen | Sprint 22 (E7) | **Fail**, fixed same day |
| 3 | Reveal into a folder already loaded | Sprint 22 (E7) | Pass |
| 4 | Incremental watching of external creates | Sprint 5 | Pass |
| 5 | PDF preview, two pages, continuous scroll | release gate smoke | Pass |
| 6 | Bundled icons and font on their own | release gate smoke | Pass |
| 7 | D-Bus surface, branded name only | release gate | Pass |
| 8 | `ShowItems` routes to an existing window | Sprint 21 | Pass |
| 9 | Preview placeholder text in a narrow pane | incidental | **Fail** |
| 10 | Packaged build, x86_64-linux, with its install checks | release gate | Pass |
| 11 | Minimal-environment smoke test of the installed package | release gate | Pass |
| 12 | Escape with a context menu open | incidental | **Fail**, fixed same day |

Two failures, one of them a check Sprint 22 wrote for itself and recorded as
delivered.

## A1 — Reveal into a still-loading folder scrolls to the wrong place

**The check.** Sprint 22, first manual item: "Reveal a file deep in a large
folder (location bar, D-Bus ShowItems, or drag) and confirm the row is both
selected and on screen."

**What happens.** In a folder of 2000 entries, revealing
`big/entry-1900.txt` over `org.freedesktop.FileManager1.ShowItems` selects
`entry-1900.txt`, previews its contents, and names it in the footer. The
viewport is showing `entry-1457.txt` through `entry-1466.txt`. The revealed
file is roughly 220 rows below the bottom of the window.

It reproduces, and it lands somewhere different each time:

| Revealed | Viewport afterwards |
|---|---|
| `entry-1900.txt` | 1457–1466 |
| `entry-1750.txt` | 0455–0464 |

Two different wrong offsets for two nearby targets, so this is not an
off-by-a-constant in the scroll arithmetic. It is a race against the load.

**Why it is a race.** Reveal the same file into a folder Marcel is already
sitting in and it is exact: `entry-0500.txt` came up selected, centred
vertically, previewed, with the correct footer. The failure only appears when
the reveal arrives while the directory is still streaming.

Sprint 22 made mid-stream events defer *as paths* and re-validate in one
catch-up batch at `Done`, which is why the selection and the preview are
right. The scroll is what does not participate. It is computed once against
however many entries had arrived at that instant, and nothing recomputes it
when the rest of the listing lands.

**Shape of the fix.** The pending reveal has to outlive the load the same way
the deferred paths do: keep the target path, and re-apply the scroll at `Done`
rather than only the selection. E7 fixed "reveal selects but never scrolls" for
a loaded directory and left the streaming case behind.

This is the same lesson as E4 and E5 one layer up. Everything that lands
mid-stream has to be re-applied after the stream, and "everything" includes
what the viewport is looking at, not only what the model holds.

**Fixed, same day.** `DirectorySession` gained `reveal_scroll_target`.
`select_pending_loaded_entries` still selects, previews, and scrolls the
moment the entry arrives, because that immediate feedback is the point of a
reveal; when it does that while `loading` is true it also records the path.
The `Done` arm calls `settle_revealed_scroll` after the deferred refresh batch
has been applied, since that batch can move the row one last time, and the
scroll is recomputed against the finished listing. A new load drops any target
it inherits, and so does `replace_pending_reveal`.

Verified the same way it was found: a cold `ShowItems` into the 2000-entry
folder now leaves the revealed row selected and vertically centred, checked
for `entry-1900.txt` and again for `entry-0733.txt`.

Four regression tests came with it. The load-order one is the honest part of
the coverage: it merges a second batch after a reveal and asserts the revealed
row moves from 1 to 3, which is the mechanism the bug depended on and the
reason a single scroll cannot be correct.

Worth recording how nearly this was missed. The first attempt at verifying the
fix appeared to fail, because Marcel routes a second launch to the existing
session process: the new window was drawn by the *old* binary, still running
from before the change. Single-instance routing working correctly is exactly
what makes "did my rebuild take effect?" untrustworthy, and the pid in
`list_windows` is what settles it.

## A2 — Preview placeholder text is clipped in a narrow pane

With the window at 1027 px wide, selecting a file with no preview shows
"Special file" and "No preview is available for this kind of file" clipped at
both edges of the preview pane: the text is centred on a box wider than the
pane and neither wraps nor elides. Widen the window and it renders correctly.

Cosmetic, and only in the placeholder, but it is the first thing a user sees
when Marcel declines to preview something, and Marcel declining to preview
something is exactly what A3 below is about.

## A3 — Escape does not close a context menu, and empties it instead

Open an item's context menu and press Escape. The menu stays on screen. The
selection underneath it is cleared, so the menu that was describing a file is
now describing nothing, and Cut, Copy, Rename, Move to Trash and Delete all
grey out while it sits there. A second Escape does the same. Clicking outside
dismisses it correctly, so the menu itself is fine; it is Escape that is
reaching past it to the grid.

Escape closing the frontmost transient surface first is what every file
manager does, and this is also the E10 shape again: a menu still on screen
after the thing it acts on has gone away.

This was initially held as unconfirmed, because the way it was driven could
have caused it: approving a tool call moves focus to the terminal, hyprhands
refocuses Marcel before delivering the key, and a broken popup grab would look
identical. The maintainer reproduced it by hand, which rules that out.

**Fixed, same day.** Escape now consumes the frontmost surface before anything
underneath sees it. `on_clear_selection` dismisses an open menu and returns
rather than falling through to the browser, and `on_window_key_down` does the
same before its filter handling, which is a separate Escape path that would
otherwise have cleared the filter and left the menu standing. Verified: Escape
closes the menu and leaves the selection and preview intact; a second Escape
then clears the selection as it always did.

No unit test. The handlers need a `Window`, and per `AGENTS.md` the
GPUI-bound orchestration deliberately stays on `Marcel` rather than moving
behind a testable interface. This matrix is what covers it, which is the
argument for running the matrix.

## What passed, and what the passes prove

**The FIFO (E2).** A writerless FIFO named `a-fifo.png` sat in the browsed
grid. Marcel drew it with a failure badge rather than a thumbnail, and
`shot.png` beside it thumbnailed normally, so the thumbnail workers survived
scrolling past it. Selecting it answered immediately with "Special file" and
"No preview is available for this kind of file" instead of blocking. That is
`open_regular_file` doing its job: the file is what its opened descriptor says
it is, not what its name says. Before E2 this cost a pool thread for the life
of the process.

**Incremental watching.** A PDF, a zip, and a Markdown file were created in the
browsed directory from a shell while Marcel was displaying it. All three
appeared in place, correctly sorted, with the right icons, and the existing
selection was left alone.

**PDF.** A two-page PDF rendered both pages in the preview pane with
continuous scrolling, and the footer reported `report.pdf File · 897 B`. The
Poppler path works from the dev shell.

**Icons and font.** Everything above rendered with Marcel's bundled Nordzy
subset and Iosevka subset, including distinct archive, PDF, text, and folder
icons. Nothing fell through to a system icon theme.

**The D-Bus surface.** The running instance owns
`io.github.berker_z.Marcel` and nothing else. `busctl --user list` shows no
`org.freedesktop.FileManager1` owner from Marcel, while
`/org/freedesktop/FileManager1` on the branded name exports `ShowItems`,
`ShowFolders`, and `ShowItemProperties`, and `/io/github/berker_z/Marcel`
exports `Activate`, `ActivateAction`, and `Open`. That is the release promise
holding: the generic name is an opt-in extra and installing Marcel does not
take it.

**Routing.** Both `ShowItems` and `ShowFolders` were answered by the window
that was already open rather than by a new one.

## The packaged build and the minimal environment

`nix build .#marcel-rs` on `x86_64-linux` succeeded from this tree. Its check
phase ran all 264 tests inside the build sandbox, including
`private_session_bus_integration`, which now passes there on
`nix/test-session.conf` rather than on the dbus package's own config. Its
install check validated the installed AppStream file and both desktop entries.

The installed tree carries the binary, the private `7zz`, both desktop
entries, the branded D-Bus service and the FileManager1 interface XML, all nine
hicolor icon sizes, the curated Nordzy subset, the bundled-asset licenses, and
the metainfo. It installs **no** `org.freedesktop.FileManager1` service, which
is the promise the README makes.

`scripts/clean_env_smoke.sh` then launched that package under `env -i` with an
empty `PATH`, a fresh `HOME`, `XDG_DATA_DIRS` and `XDG_CONFIG_DIRS` pointed at
an empty directory, and a fontconfig file naming no font directory at all. The
point is to catch the package relying on something the host was quietly
supplying:

- Every icon rendered from the bundled Nordzy subset, with archive, PDF, text,
  image and folder icons all distinct. Nothing reached a system icon theme,
  because there was none to reach.
- Text rendered in the bundled Iosevka subset with no font path configured.
- Places collapsed to Home and Trash, and Bookmarks showed its empty
  placeholder. Correct for a `HOME` with no XDG user directories and no
  bookmark file, and worth seeing rather than assuming.
- The two-page PDF rasterized. With `PATH` empty, `pdftoppm` can only have come
  from the wrapper.
- Extracting a zip produced its contents and revealed them in the listing. With
  `PATH` empty, `7zz` can only have come from `libexec/marcel/7zz`, found
  relative to the wrapped executable.

That covers the gate's minimal-environment list except desktop activation,
which was exercised against the development build earlier rather than against
the package.

A note on what this method cannot see, because it produced one wrong reading
during the run. Extracting an archive whose top-level entry already exists
looked like it did nothing at all: no dialog, no message. It does not do
nothing. `ensure_unoccupied` bails with "already exists; nothing was
overwritten", and that becomes a `Report::Error`, which `surface::deliver`
pushes as a notification on the window that owns the work.

Two things settle it. A *successful* extraction produces no visible card in
these screenshots either, though it plainly worked, since the output appeared
and was revealed. And the maintainer, watching the actual screen, captured the
clash notification directly:

```text
“/tmp/claude-1000/mtest/ziptest” already exists; nothing was overwritten
```

which is `ensure_unoccupied`'s message word for word.

So notification cards do not survive this capture path, and their absence from
a screenshot says nothing whatsoever about whether one was shown. Anything
involving a notification has to be checked by a person watching the screen, or
read out of the code. Worth stating plainly, because a screenshot run that
cannot see a whole class of user-facing output will otherwise keep producing
confident wrong readings of it.

What remains genuinely open is smaller and is a design question rather than a
defect: extraction *refuses* on a taken destination and says so, where copy and
move offer replace, rename, skip, or merge. Refusing safely is a defensible
choice for a first release. Making the two consistent is 0.2 material.

## Not run

`aarch64-linux` could not be built here at all. The host has no
`binfmt_misc` registration and no aarch64 in `extra-platforms`, and no remote
builder is configured, so there is nothing to run the build on. Enabling
`boot.binfmt.emulatedSystems` would mean a GPUI thin-LTO build under QEMU,
which is not a reasonable way to satisfy a release gate. CI's `package` job
covers it on a native `ubuntu-24.04-arm` runner instead, and that is the route
to use.

Still outstanding from the standing matrix, and none of it blocked by anything
above:

- Two-window checks: Escape in window B against a transfer started in window A,
  bookmark menus and drags acting across windows, cancellation authority.
- Cold D-Bus activation with no Marcel running, and the check that it opens no
  stray window at the daemon's working directory.
- Another file manager owning `org.freedesktop.FileManager1` while Marcel
  starts.
- Show Hidden toggled while several items are selected and the previewed one
  disappears.
- Drag and drop in either direction. `drag` needs ydotool, which this machine
  does not have, so it could not be driven this way at all.
- Trash, restore, permanent deletion, and the conflict decisions. These move
  real files and were left for a run that is watched.
- Archive extraction through the UI. `bundle.zip` was created and displayed but
  not extracted.
