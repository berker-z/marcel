# Third-party notices and adapted code

Marcel depends on third-party Rust crates whose license information is recorded
in Cargo metadata. This file additionally tracks source code substantially
adapted into Marcel itself.

## Yazi

- Project: <https://github.com/sxyazi/yazi>
- License: MIT
- Copyright: Copyright (c) 2023 - sxyazi
- Current adaptations:
  - `src/fs.rs` adapts the partial-update and monotonically increasing ticket
    model from `yazi-fs/src/op.rs` and `yazi-fs/src/entries.rs`. Marcel's
    implementation uses GPUI tasks and bounded GUI list batches rather than
    Yazi's event layer.
  - `src/history.rs` adapts the bounded cursor-and-stack behavior from
    `yazi-core/src/tab/backstack.rs`.
  - `src/app.rs` adapts the task replacement and stale-preview rejection
    principles from `yazi-core/src/tab/preview.rs` and
    `yazi-core/src/tab/preview_lock.rs`.
  - `src/app.rs` adapts the separation of finder matches as derived state over
    the current folder from `yazi-core/src/tab/finder.rs` at upstream commit
    `e58022b9aafc8dabf586e2cc29b79a230071716f`. Marcel uses a fuzzy-ranked
    visible-index list shared by both GUI views rather than Yazi's finder
    match-index map.
  - `src/app.rs` and `src/fs.rs` adapt Yazi's separation of a hovered-folder
    preview from the main browser, its bounded visible folder slice, and its
    independently refreshed folder state from
    `yazi-actor/src/lives/preview.rs`, `yazi-actor/src/mgr/peek.rs`, and
    `yazi-core/src/tab/tab.rs` at upstream commit
    `e58022b9aafc8dabf586e2cc29b79a230071716f`. Marcel uses a cancellable
    batch stream and a GPUI virtualized list; it adds no selection or file
    operations to the preview surface.
  - `src/app.rs` adapts the visible-page-first preloading, bounded worker-pool,
    duplicate suppression, and queued-work supersession principles from
    `yazi-core/src/tasks/prework.rs`, `yazi-scheduler/src/scheduler.rs`, and
    `yazi-scheduler/src/worker.rs`. The adaptation was made against upstream
    commit `e58022b9aafc8dabf586e2cc29b79a230071716f`; Marcel uses GPUI entity
    tasks and a Marcel-owned viewport queue rather than Yazi's task types.
  - `src/thumbnails.rs` adapts the decoder-limit and resize-before-orientation
    flow from `yazi-adapter/src/image.rs` at the same upstream commit. Marcel
    produces 128-pixel freedesktop thumbnail PNGs rather than Yazi's
    preview-sized private cache entries.
  - `src/pdf_preview.rs` adapts the requested-page `pdftoppm` bridge and cached
    image flow from `yazi-plugin/preset/plugins/pdf.lua` at the same upstream
    commit. Marcel adds a file-identity cache, fixed raster bounds, timeouts,
    subprocess cancellation, and a GPUI virtualized continuous-scroll page
    scheduler.
  - `src/file_ops.rs` conceptually adapts per-item operation outcomes,
    cooperative cancellation, partial-success accounting, and the rename-first
    move path from `yazi-scheduler/src/worker.rs` and
    `yazi-scheduler/src/file/file.rs` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. Marcel's implementation is
    session-serialized and adds hidden staging, Linux `RENAME_NOREPLACE`,
    recursive identity validation, bounded atomic progress snapshots, and
    general filesystem undo/redo. No Yazi code was copied.
  - Sprint 6's copy-fidelity audit additionally studied
    `yazi-scheduler/src/file/traverse.rs`,
    `yazi-fs/src/engine/local/copier.rs`, and
    `yazi-fs/src/engine/attrs.rs` at the same commit. Marcel conceptually
    adapts Yazi's opaque-symlink traversal, file mode/time baseline, and
    buffered progressive copying while adding Marcel-owned sparse, xattr/ACL,
    hardlink, staging, and undo semantics. No Yazi code was copied.
  - `src/directory_watcher.rs` conceptually adapts Yazi's non-recursive
    recommended watcher, polling fallback, 250 ms event coalescing,
    deduplication, and metadata-revalidated upsert/delete flow from
    `yazi-watcher/src/local/local.rs` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. Marcel publishes bounded batches
    through its own `DirectoryEvent` reducer and uses GPUI/background-executor
    lifetimes rather than Yazi's Tokio, VFS, and global event infrastructure.
    No Yazi code was copied.

Yazi is a primary architectural influence for asynchronous filesystem work,
task scheduling, cancellation, previews, and responsiveness. Future direct or
substantial adaptations will list the upstream path, Marcel path, and a short
description here.

The Yazi MIT license notice applies to the adaptations identified above:

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

## Zed and GPUI

- Project: <https://github.com/zed-industries/zed>
- GPUI license: Apache-2.0
- Zed application license: primarily GPL-3.0-or-later
- Current adaptations: The initial window bootstrap and declarative layout
  follow the public Apache-2.0 GPUI examples; no Zed application code has been
  copied.

## gpui-component

- Project: <https://github.com/longbridge/gpui-component>
- License: Apache-2.0
- Current adaptations: The application root initialization follows the
  project's public basic example.

## Poppler

- Project: <https://poppler.freedesktop.org/>
- Use in Marcel: `pdfinfo` supplies page counts and `pdftoppm` rasterizes one
  bounded page at a time for the PDF preview provider.
- Integration: Poppler remains an external runtime utility supplied by the Nix
  development environment; its source is not copied or linked into Marcel.
