# External review notes

Recorded: 2026-07-29

## Context

These notes preserve an external AI-assisted static review supplied by the
maintainer, followed by Marcel's own validation and response. The external
reviewer inspected the repository and a screenshot but did not compile,
stress-test, or dynamically inspect the application. This document is design
input, not an acceptance report or an authorization to begin the work.

## Overall assessment

The review found Marcel to be a serious alpha rather than a generated demo. It
highlighted:

- no-overwrite file operations using hidden staging and Linux
  `RENAME_NOREPLACE`;
- cooperative cancellation, partial-success records, identity-validating
  undo/redo, and focused filesystem tests;
- bounded text, thumbnail, and PDF preview workloads;
- Linux-aware default application and Open With integration that avoids leaking
  the Nix development environment into launched applications;
- an interaction model that keeps commands, safety rules, and selection
  semantics consistent across UI entry points;
- unusually explicit Yazi provenance and separation between conceptual
  adaptation, copied code, and Zed source study.

Marcel's maintainers agree with that assessment, with the qualification that
static invariants and tests do not yet constitute production filesystem
assurance.

## Architectural pressure in `app.rs`

The review identified `src/app.rs` as the primary architectural risk. At the
time these notes were recorded it was 5,598 lines, `Marcel` exposed roughly 138
methods, and one entity coordinated:

- directory loading, filtering, navigation, and selection;
- marquee and file-drag sessions;
- file-operation state, clipboard state, progress, cancellation, and history;
- thumbnails, folder previews, text wrapping, and PDF workers;
- Places, bookmarks, menus, dialogs, layout measurements, and rendering.

The concern is not file length by itself. Native desktop drag-and-drop,
filesystem watching, mounts, media, and operation queues each add concurrent
state machines. Continuing to add all of them directly to `Marcel` would make
unrelated systems increasingly capable of affecting one another.

The 10,000-entry marquee investigation demonstrated this coupling: browser
selection repaints were constructing file-drag payloads, and row rendering
performed a directory lookup whose cost depended on scroll position. Both hot
paths were removed and documented in Sprint 3, but they are useful evidence for
establishing clearer ownership boundaries.

A later invalid-drop test exposed the same shape in drag-hover negotiation:
each painted candidate searched the source entry vector by path, making drags
from the end of a 10,000-entry directory briefly stall. Painted hit regions now
carry the navigable flag needed for target decisions, so pointer movement stays
proportional to the small painted set rather than directory position.

### Recommended response

Do not perform an abstract clean-architecture rewrite. Extract coherent,
existing behavior mechanically, preserving commands, tests, and visible
behavior:

1. Move operation lifecycle state into an `OperationController`.
2. Introduce a directory/browser session that owns entries, filtering,
   selection, navigation coupling, and future watcher events.
3. Move preview lifecycle state and workers into a `PreviewController`.
4. Move Places and bookmarks into a `SidebarModel`.
5. Split browser, preview, sidebar, and menu rendering only after state
   ownership is clearer.

These types can begin as ordinary Rust structs owned by `Marcel`. They should
become independent GPUI entities only when independent notification,
rendering, or task lifetimes provide a concrete benefit.

Status: Sprint 4A introduced `OperationController` as an ordinary Rust struct
owned by `Marcel`. It now owns the clipboard, journal, busy/cancellation state,
task handles, progress state, and their pure transitions while preserving
`file_ops.rs` as the filesystem engine.

Sprint 4B introduced `DirectorySession`, also as an ordinary Rust struct owned
by `Marcel`. It owns the current directory's source entries, fuzzy-ranked
visible projection, hidden-file policy, selection reconciliation, load
generation and task lifetime, load errors, and pending reveal. `app.rs` retains
view concerns such as scroll handles, painted hit bounds, marquee geometry, and
preview side effects. A pure add/remove/change/rename/rescan reducer is tested
and ready to become the watcher integration boundary.

Sprint 5 connected that boundary to an active-directory watcher. Native
notifications fall back to polling, noisy paths are coalesced and deduplicated,
metadata is revalidated off the foreground executor, and each bounded batch is
applied with one projection/selection reconciliation pass. Generation checks
prevent a replaced watcher from publishing after navigation. Marcel's own
completed operations now report exact top-level upserts/removals through that
same reducer. This preserves scroll, selection projection, thumbnails, and the
active watcher in large directories; metadata failures retain a bounded full
rescan fallback.

## Copy-semantics and scale debt

At review time, Marcel preserved:

- file contents;
- directory structure;
- symbolic links without following them;
- basic permission bits.

It did not yet promise preservation of:

- timestamps;
- extended attributes;
- ACLs;
- ownership;
- sparse-file layout;
- hardlink relationships.

The ownership concern remains important: a normal unprivileged file manager
copy must not claim the semantics of privileged archival replication.

At review time, a recursive copy could:

1. measure the source tree for progress;
2. snapshot the source for validation;
3. traverse the source while copying;
4. snapshot the destination.

Sprint 6 established the explicit contract and fixtures. Marcel now preserves
regular-file and directory modes, file access/modification times, directory
modification times, supported user xattrs and POSIX ACLs, sparse extents, and
hardlinks within one copied tree. Ownership, privileged labels, birth time,
filesystem flags, reflinks, and cross-top-level hardlinks remain explicit
non-goals.

Sprint 6 also collects snapshot paths during the main copy traversal and
performs a flat destination identity refresh after publication, eliminating
the separate source and destination enumeration passes. Copy undo records are
capped at 100,000 combined snapshots and oversized copies explicitly complete
without entering undo history. Validation compares exact tree membership as
well as identities before mutation. Progress measurement remains a separate
source traversal.

No-overwrite publication protects the destination namespace, but it does not
make the source tree transactional. External mutation during a copy can still
produce a result observed across different source states; the copy-semantics
document now states this explicitly.

## Priority recommendation

Before adding several more stateful features, prioritize:

1. narrow architectural seams, beginning with operation lifecycle ownership;
2. a directory/browser session designed to receive incremental watcher events;
3. incremental filesystem watching without full directory reloads;
4. bilateral native desktop drag-and-drop;
5. Trash and restore;
6. copy-semantics policy, fixtures, and scale work;
7. packaging and default-file-manager integration.

Media playback and ebook previews remain valuable, but they should not delay
the filesystem and architecture work needed for Marcel to become a credible
default file explorer.
