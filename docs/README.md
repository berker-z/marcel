# Marcel internal documentation

The root `README.md` is the public page. This directory holds what was
decided and why, and what is left.

## Where things stand

`v0.1.0` is untagged and close. [Sprint 27](sprints/027-sound-and-a-frame.md)
added audio playback and video posters to the preview pane, with ffmpeg
found on `PATH` rather than bundled. [Sprint 26](sprints/026-tag-readiness.md)
closed the last review's blockers and added editable permissions, sorting,
and a theme that persists; its acceptance list holds the hand-checks still
open. [`TODO.md`](TODO.md) is the one list of what is left, in order, and
[`release.md`](release.md) is the gate the tag has to pass.

## Documents

- [`TODO.md`](TODO.md): the backlog, in order.
- [`release.md`](release.md): release, packaging, CI, and distribution.
- [`nixpkgs.md`](nixpkgs.md): the nixpkgs submission recipe.
- [`interaction-model.md`](interaction-model.md): shortcuts, menus, selection,
  undo, and the safety contract behind them.
- [`copy-semantics.md`](copy-semantics.md): what a copy preserves, and the
  symbolic-link policy.
- [`file-chooser-portal.md`](file-chooser-portal.md): Marcel as the
  xdg-desktop-portal file chooser, and how to test it.
- [`../THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md): upstream reuse,
  bundled assets, and their licenses.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) and
  [`../SECURITY.md`](../SECURITY.md): how a change gets in, and where a
  vulnerability report goes. [`../AGENTS.md`](../AGENTS.md) has the rules
  both point at.
- [`sprints/`](sprints/): numbered records of each slice of work, with the
  acceptance checks as they stood. Older ones keep the status of their own
  time; read them as history.

The external review records that used to live here (five reviews and one
acceptance run between 2026-07-29 and 2026-08-21) were removed once every
finding was either fixed or on the backlog. They are in the git history;
sprint documents 17 through 22 still name them.

## Sprint status convention

- **Planned:** the contract and acceptance checks exist; nothing is built.
- **In progress:** implementation is under way.
- **Implemented:** the code and automated checks are in; named manual checks
  may remain and are listed as unchecked.
- **Accepted:** every check, automated and manual, is done.
