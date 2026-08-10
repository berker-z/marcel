# Copy semantics

Marcel's copy operation is a safe desktop file copy, not an archival or
privileged replication tool. This document defines what a successful copy
means and what Marcel must never silently imply.

## Publication and conflicts

- Each top-level item is assembled under a hidden staging name in the
  destination directory.
- The completed staging tree is published atomically with Linux
  `RENAME_NOREPLACE`.
- An occupied destination is never overwritten, merged, or implicitly renamed.
- Cancellation and failure remove unpublished staging output. Successfully
  published earlier top-level items remain explicit partial successes and are
  recorded for undo.

These guarantees are stricter than Yazi's normal interactive conflict model
and remain Marcel-owned behavior.

## Preserved by a successful local copy

- regular-file contents and logical length;
- directory structure;
- symbolic links as links, without following them;
- Unix permission mode bits for regular files and directories;
- access and modification times for regular files, and modification times for
  directories. Directory access time is not promised because measuring and
  traversing a directory can itself update it before copying;
- `user.*` extended attributes;
- POSIX access/default ACL attributes where the source filesystem exposes them
  as `system.posix_acl_access` and `system.posix_acl_default`;
- sparse extents when the source filesystem exposes `SEEK_DATA`/`SEEK_HOLE`,
  with a bounded buffered fallback when it does not;
- hardlink relationships between regular files within one copied top-level
  directory tree.

If Marcel can read a supported attribute from the source but cannot reproduce
it at the destination, that top-level copy fails before publication. It does
not silently publish a knowingly degraded result.

## Deliberately not preserved

- numeric ownership. Marcel does not elevate privileges or attempt to make
  output owned by another user or group;
- set-user-ID and set-group-ID execution semantics beyond ordinary mode-bit
  reproduction permitted by the destination filesystem;
- security labels and privileged namespaces such as `security.*` and
  `trusted.*`;
- birth/creation time on Linux;
- filesystem-specific flags, compression policies, reflink relationships, and
  project quotas;
- sparse layout when the source or destination does not support the required
  seek semantics;
- hardlink relationships across separately selected top-level sources.

Unsupported special files—including sockets, devices, and FIFOs—cause that
top-level item to fail rather than being followed or converted.

## Source consistency

A copy is not a filesystem transaction over the source tree. External mutation
while traversal is in progress can produce a destination observed across
different source states. Marcel snapshots identities for undo/redo refusal but
does not freeze or lock the source.

## Undo and scale

Undo compares exact tree membership and validates every recorded destination
identity before removing anything. Source and destination snapshot paths are
collected during the copy traversal, then destination identities receive a
flat refresh after atomic publication; this avoids two additional complete
directory-enumeration passes.

A copy operation may retain at most 100,000 combined source and destination
snapshot records—approximately 50,000 copied paths. Larger copies still
complete safely, but Marcel discards that operation's partial snapshot record,
does not place it in undo history, and reports that limitation in the
completion notification. The operation journal remains separately bounded to
20 operations per stack.

Progress measurement is still an additional source-tree traversal, and deeply
nested traversal is still recursive. Those are performance debts rather than
silent fidelity changes.

## Yazi comparison

Audit baseline: Yazi commit
`319f90e0eab185a231eef5562215ba322e320286`.

Yazi's local copy:

- traverses directories breadth-first and schedules per-file work;
- recreates symlinks unless explicit following is requested;
- copies regular files with mode, access time, and modification time;
- tries a direct rename first for moves and otherwise traverses/copies;
- reports progressive work and retries selected unusual filesystem errors.

The audited implementation does not preserve directory metadata after
population, xattrs, ACLs, ownership, sparse extents, or hardlink relationships.
Marcel adopts Yazi's proven asynchronous, cancellation, metadata-baseline, and
rename-first ideas while retaining Marcel's staging, no-overwrite publication,
and identity-validating undo.

Sources:

- <https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs>
- <https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/traverse.rs>
- <https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/local/copier.rs>
- <https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/attrs.rs>
