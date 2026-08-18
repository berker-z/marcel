# Marcel — working notes for Claude Code

Read [`AGENTS.md`](AGENTS.md) for product, architecture, and contribution rules.
This file only covers how to run things without wasting the maintainer's time.

## Enter the dev shell once, not once per check

`cargo` needs the Nix dev shell; a plain shell fails building
`yeslogic-fontconfig-sys` for want of `fontconfig.pc`. Do **not** prefix every
command with `nix develop`. That is one approval prompt per check, and
`nix develop` itself needs the sandbox disabled here (flake evaluation reads the
git tree and hits `.gitmodules is locked: Permission denied`).

Capture the dev-shell environment **once per session**, then run every check
sandboxed and prompt-free:

```sh
# Once, with dangerouslyDisableSandbox: true
nix develop --command bash -c 'declare -px' \
  | grep -v -E '^declare -x (TMPDIR|HOME|PWD|OLDPWD|SHLVL|_)=' > "$TMPDIR/devshell.env"
```

Dropping `TMPDIR`/`HOME` matters: the ambient sandbox values have to win, or
cargo writes outside the writable roots.

Then, for every check afterwards — ordinary sandboxed Bash, no prompt:

```sh
set -a && source "$TMPDIR/devshell.env" && set +a
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
```

Chain the whole gate into a single invocation rather than three. If the captured
environment goes stale (a `flake.nix` change), recapture it.

## Keep `TMPDIR` short

Several `file_ops` tests bind Unix sockets to prove that a tree holding a
special file still moves. `sun_path` is 108 bytes, so a long `TMPDIR` — a
per-session scratchpad path, for instance — makes six of them fail at once with
errors that look like permission or sandbox problems and are neither. Point
`TMPDIR` at something short (`/tmp/claude-1000`) before running the suite.

## The one test that needs an unsandboxed run

`desktop_integration::tests::private_session_bus_integration` spawns
`dbus-run-session` and fails under a restrictive sandbox. Before reporting the
suite as green, run it once outside the sandbox — do not write that failure off
as pre-existing.

## Do not reach for `nix build`

`nix build` and `nix flake check` are release-time checks. During ordinary work
use focused Nix evaluation only when a change touches flake or package
definitions.
