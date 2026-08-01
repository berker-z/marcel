# Sprint 6 — Copy fidelity and scale

**Status:** Implemented — the copy-fidelity contract, scale investigation, and
automated quality gate are complete.

## Goal

Make Marcel's successful-copy promise explicit and improve ordinary Linux copy
fidelity before adding Trash and more destructive operations.

## Yazi audit and contract

- [x] Audit Yazi's traversal, regular-file copier, attribute model, symlink
  behavior, progressive work, and rename-first move fallback at an exact
  upstream commit.
- [x] Record Marcel's guaranteed, unsupported, and failure semantics in
  [`copy-semantics.md`](../copy-semantics.md).
- [x] Preserve Marcel's stricter hidden staging, no-overwrite publication,
  cancellation, partial-success, and identity-validating undo behavior.

## Fidelity implementation

- [x] Preserve regular-file access/modification times and directory
  modification times.
- [x] Preserve supported `user.*` xattrs and POSIX ACL xattrs, failing before
  publication when a readable supported attribute cannot be reproduced.
- [x] Preserve sparse extents with a buffered fallback.
- [x] Preserve hardlink relationships within a copied top-level directory
  tree.
- [x] Keep symlinks opaque and reject unsupported special files.
- [x] Add fixtures for modes/times, user xattrs, POSIX access ACLs, sparse
  files, hardlinks, special-file rejection, symlinks, conflicts, cancellation,
  and undo/redo refusal.

## Scale investigation

- [x] Audit the repeated source/destination traversals and snapshot memory on
  large trees.
- [x] Collect source and destination snapshot paths during the copy traversal,
  replacing two complete directory-enumeration passes with a flat
  post-publication identity refresh.
- [x] Cap copy undo records at 100,000 combined source/destination snapshots.
  Larger copies still complete but clearly report that they were not retained
  in undo history.
- [x] Strengthen validation to compare exact tree membership as well as path
  identities before undo/redo mutates anything.

## Deliberate limits

- Ownership, privileged security labels, birth time, filesystem flags,
  reflinks, and cross-top-level hardlink preservation are not promised.
- Cross-filesystem moves and conflict dialogs remain parked.
- Trash begins only after this sprint's fidelity acceptance checks pass.

## Quality gate

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
