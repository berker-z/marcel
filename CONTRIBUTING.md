# Contributing

Thanks for looking. Marcel is small enough that one document holds the rules:
[`AGENTS.md`](AGENTS.md) covers what the program is trying to be, how upstream
code (mostly Yazi's) is reused and credited, when to use gpui-component, how
picker windows share the browser, where shortcuts are declared, and the module
map. Read it first; this page is the short version of how a change gets in.

## Building

The Nix dev shell is the only supported build environment. It pins the
compiler and declares the system libraries GPUI needs, and a plain `cargo`
outside it fails at `fontconfig.pc`.

```sh
nix develop
cargo run
```

The README's Building section has the rest, including why not to run
`cargo clean`.

## The gate

Before a change is done, from inside the dev shell:

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

That is also what CI runs on a tag. Nothing hosted runs on an ordinary push,
so a green gate on your machine is what stands between a mistake and
`master`. The one test that needs a real session bus,
`desktop::bus::tests::private_session_bus_integration`, spawns
`dbus-run-session` from the dev shell and fails inside a restrictive sandbox;
run it unsandboxed once before calling the suite green. Several `fsops` tests
bind Unix sockets, so keep `TMPDIR` short.

New behaviour comes with a test, in a `#[cfg(test)]` module in the file it
belongs to, using `src/testing.rs` (`Sandbox` and friends) rather than a bare
`tempdir()`. Comments explain why, not what.

## Sprint documents

Work larger than a fix is planned and recorded in a numbered document under
[`docs/sprints/`](docs/sprints/): `NNN-short-name.md`, starting with a
**Status** line (Planned, In progress, Implemented, or Accepted; the
definitions are in [`docs/README.md`](docs/README.md)), then the goal, what
was built and why it was built that way, and a list of acceptance checks,
ticked as they pass. The document is written before the code and kept honest
as decisions change; older ones are history and are not rewritten.
[`docs/TODO.md`](docs/TODO.md) is where the next sprint's items come from.

A user-visible change also gets a line in [`CHANGELOG.md`](CHANGELOG.md),
written for someone using Marcel rather than someone reading the diff, and a
shortcut change updates the README's table in the same commit.

## Commits and pull requests

Commit messages are an imperative sentence, with a body explaining the why
where the diff does not. Pull requests get formatting, Clippy, the version
check, and AppStream validation from `pr.yml`; the tests and the packaged
build run on tags. If you are unsure whether something fits, open an issue
first. It is cheaper than a pull request that does not land.

Security reports go through [`SECURITY.md`](SECURITY.md), not the issue
tracker.
