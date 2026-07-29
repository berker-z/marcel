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

## Copy-semantics and scale debt

The review correctly observed that Marcel currently preserves:

- file contents;
- directory structure;
- symbolic links without following them;
- basic permission bits.

It does not yet promise preservation of:

- timestamps;
- extended attributes;
- ACLs;
- ownership;
- sparse-file layout;
- hardlink relationships.

Ownership in particular requires a product policy: a normal unprivileged file
manager copy should not automatically claim the semantics of a privileged
archival copy. Marcel needs an explicit copy-semantics contract and fixtures
before users should trust it with irreplaceable directory trees.

Scale also needs attention. A current recursive copy may:

1. measure the source tree for progress;
2. snapshot the source for validation;
3. traverse the source while copying;
4. snapshot the destination.

The source and destination snapshots are retained in the bounded operation
journal. This provides strong undo/redo refusal behavior but creates repeated
tree walks and potentially large in-memory vectors. Future work should examine
combining measurement with source snapshotting, collecting destination
identities during the copy, and bounding journal cost by memory as well as
operation count.

No-overwrite publication protects the destination namespace, but it does not
make the source tree transactional. External mutation during a copy can still
produce a result observed across different source states. The copy-semantics
document must describe this honestly.

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
