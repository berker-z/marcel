# Marcel session handoff

**Prepared:** 2026-08-21
**Branch:** `master`, pushed through `2b4be3a`
**Workspace:** `/home/berkerz/Projects/marcel`

Read `AGENTS.md` first, and `CLAUDE.md` for how to run the checks without one
approval prompt per command. One correction to `CLAUDE.md`: the D-Bus test no
longer needs an unsandboxed run for its own sake, because the dev shell now
supplies `dbus-run-session` and a private bus config. It still fails under a
restrictive sandbox.

## Where things stand

Release readiness for `v0.1.0`. The code queue has been closed since Sprint 22;
what remained was packaging, hosted checks, and the graphical acceptance matrix
that `TODO.md` calls the last thing between the tree and a release decision.

The matrix has now been run once, and it earned its cost immediately. Two real
defects, both fixed and re-verified graphically:

- **A1** — revealing a file in a folder that was still loading selected and
  previewed it but left it off screen, at a different wrong offset each run.
  Sprint 22 had recorded this exact check as *delivered*. The scroll is now
  re-applied at `Done`.
- **A3** — Escape did not close a context menu; it cleared the selection
  underneath, leaving the menu greyed out on screen.

Full record with method notes: [`acceptance-2026-08-21.md`](acceptance-2026-08-21.md).

The command is now `marcel-rs`, not `marcel`, because nixpkgs already has an
unrelated `marcel` shipping `bin/marcel`. Only the command changed; the app ID,
D-Bus name, icon, and `~/.config/marcel` are untouched. The overlay attribute
moved too, which mattered more than the binary: it used to bind `marcel` and so
*replaced* nixpkgs' package for anyone applying it.

## Two traps that cost real time today

Both produced confidently wrong conclusions. Expect them again.

**A rebuild is not a new process.** Marcel routes a second launch to the
existing session owner, so a "fresh" window can be drawn by the binary from
before your change. This happened twice; the second time the packaged wrapper
reports itself as `.marcel-rs-wrap`, so a pattern match on `marcel-rs` missed
it. Check `readlink /proc/<pid>/exe` before believing any graphical result.
Note also that the sandbox has its own PID namespace, so `pkill` from a
sandboxed shell sees only itself.

**Screenshots cannot see notification cards.** A successful operation and a
failed one look equally silent through `grim`. Extraction was wrongly recorded
as failing silently on a taken destination; it reports fine, and the maintainer
photographed the card. Anything involving a notification needs a person
watching, or the code read.

## CI

`.github/workflows/ci.yml`, four jobs. **Tags and manual dispatch only** —
nothing fires on an ordinary push, deliberately, because every job compiles
GPUI and the same gate is already mandatory locally. Run one with:

```sh
gh workflow run ci.yml --ref master
```

First hosted run found two environment bugs a developer machine hides: a
`toString` path that was never a real store dependency, and ten Trash tests
with no Trash to resolve. Both fixed in `2b4be3a`.

The consequence to keep in mind: a broken `master` will not be caught by
anything hosted. The local gate is the only thing before the tag.

## What is left before tagging

Verified: AppStream metadata, changelog, `nix build` on **x86_64 and
aarch64**, `nix flake check`, minimal-environment smoke
(`scripts/clean_env_smoke.sh`), 264 tests green.

Outstanding:

1. **Confirm CI run [32469902009](https://github.com/berker-z/marcel/actions/runs/32469902009) went green.** It was still running when this was written; aarch64 had already passed.
2. **Destructive-operation acceptance checks, never run graphically.** Trash,
   restore, permanent deletion behind its confirmation, Empty Trash, and
   mounted-volume Trash. This is the one I would hold the tag for: Marcel
   mutates user data, permanent deletion is unrecoverable, and today proved
   twice that a green suite says nothing about the graphical layer.
3. **Install and launch from the pushed revision** —
   `nix profile install github:berker-z/marcel/<rev>`. Everything so far was a
   local build.
4. **Confirm installing does not become the default directory handler.** CI
   proves no `FileManager1` service is installed; the MIME association is
   unverified live.
5. Run `scripts/check_version.sh v0.1.0`, then the signed tag and checksums.

Not blockers, logged and deliberate: **A2**, the "no preview available"
placeholder clipping in a narrow pane; extraction *refusing* a taken
destination where copy and move offer replace/rename/skip/merge; and the
absence of external testing, which is a nixpkgs-submission concern rather than
a tagging one.

Still unrun from the standing matrix: two-window ownership checks, cold D-Bus
activation, Show Hidden with a selection, and drag and drop, which needs
ydotool that this machine does not have.

## Testing the GUI

hyprhands is wired in as an MCP server and is how the matrix above was driven.
`busctl --user call io.github.berker_z.Marcel /org/freedesktop/FileManager1 …`
drives reveal and navigation without synthetic input, which is more trustworthy
than clicking; findings driven that way are immune to focus and tiling churn.
Coordinates read off a screenshot go stale when the compositor retiles, so
re-screenshot immediately before every click.
