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
  - `src/file_ops.rs` adapts the object-kind taxonomy of `ChaType` from
    `yazi-fs/src/cha/type.rs` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286` into `SnapshotKind`. Like Yazi,
    Marcel enumerates block devices, character devices, sockets, and FIFOs as
    distinct kinds with an explicit unknown case rather than collapsing
    everything it cannot reproduce into one variant. The same commit's
    rename-first move path in `yazi-scheduler/src/file/file.rs` confirmed that a
    rename never inspects what a directory holds, which is why Marcel records
    those kinds for move undo while refusing them wherever undo would have to
    recreate or delete them. Yazi has no filesystem undo, so the surrounding
    snapshot, validation, and removal policy is Marcel's own. No Yazi code was
    copied.
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
  - `src/trash_ops.rs` conceptually adapts Yazi's separation of background
    Trash scheduling from its freedesktop Trash VFS at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. The audited sources are
    `yazi-scheduler/src/file/file.rs`,
    `yazi-fs/src/trash/freedesktop/trash.rs`, and
    `yazi-fs/src/trash/freedesktop/trash_info.rs`. Like Yazi, Marcel delegates
    platform placement to the MIT-licensed `trash` crate. Marcel adds its own
    exact-entry discovery, identity-validating no-replace restore, operation
    journal, and stricter missing-parent policy. No Yazi code was copied.
  - `src/delete_ops.rs` conceptually adapts Yazi's leaf-before-directory,
    no-symlink-follow delete traversal and per-entry scheduler outcomes from
    `yazi-scheduler/src/file/file.rs`,
    `yazi-scheduler/src/file/traverse.rs`, and
    `yazi-fs/src/engine/traits.rs` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. Marcel adds whole-selection
    atomic quarantine, filesystem-identity revalidation, paired Trash metadata
    cleanup, and its own progress interface. No Yazi code was copied.
  - Sprint 9's Rename interaction in `src/app.rs` and safe operation in
    `src/file_ops.rs` conceptually adapt Yazi's focused rename input,
    extension-aware cursor placement, watcher/model coordination, and reveal
    behavior from `yazi-actor/src/mgr/rename.rs`, `yazi-fs/src/op.rs`,
    `yazi-vfs/src/engine/engine.rs`, and
    `yazi-fs/src/engine/local/local.rs` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. Marcel uses an inline GPUI
    editor, Linux `RENAME_NOREPLACE`, conservative identity-validating
    Undo/Redo, and never overwrites. No Yazi code was copied.
  - Sprint 10's archive flow in `src/archive_ops.rs` conceptually adapts the
    staging, one-versus-many top-level tidy behavior, compound-tar handling,
    and `7zz`/`7z` fallback from
    `yazi-plugin/preset/plugins/extract.lua` and
    `yazi-plugin/preset/plugins/archive.lua` at upstream commit
    `319f90e0eab185a231eef5562215ba322e320286`. Marcel adds preflight and
    post-extraction validation, strict no-overwrite publication, bounded
    subprocess diagnostics, process-group cancellation, identity-validating
    Undo/Redo, and ZIP-only creation. No Yazi code was copied.

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
- Current adaptations:
  - The initial window bootstrap and declarative layout follow the public
    Apache-2.0 GPUI examples; no Zed application code has been copied.
  - Marcel uses GPUI's upstream `ExternalDragPayload::Files` API and Linux
    drag-source implementation. `Cargo.lock` pins the exact Zed revision; no
    locally modified GPUI source is shipped.

## gpui-component

- Project: <https://github.com/longbridge/gpui-component>
- License: Apache-2.0
- Current adaptations: The application root initialization follows the
  project's public basic example. `Cargo.lock` pins the exact source revision;
  no locally modified gpui-component source is shipped.

## trash

- Project: <https://github.com/ArturKovacs/trash>
- License: MIT
- Use in Marcel: version 5.x implements native freedesktop Trash placement and
  enumerates valid home and mounted-volume Trash roots. Marcel wraps it behind
  `src/trash_ops.rs` rather than coupling UI code to the crate.

## Poppler

- Project: <https://poppler.freedesktop.org/>
- Use in Marcel: `pdfinfo` supplies page counts and `pdftoppm` rasterizes one
  bounded page at a time for the PDF preview provider.
- Integration: Poppler remains an external runtime utility supplied by the Nix
  development environment; its source is not copied or linked into Marcel.

## 7-Zip

- Project: <https://www.7-zip.org/>
- Copyright: Copyright (C) 1999-2026 Igor Pavlov
- License: GNU LGPL-2.1-or-later for the main program, with BSD-3-Clause and
  BSD-2-Clause portions as detailed in 7-Zip's `License.txt`. Marcel's default
  packages exclude the restricted UnRAR decoder.
- Use in Marcel: official `7zz` provides ZIP creation and broad-format
  extraction behind `src/archive_ops.rs`. It is a supervised external process,
  not linked into Marcel.
- Distribution: package-managed builds use their distribution's maintained
  package. Portable artifacts bundle the official static executable as
  `libexec/marcel/7zz`.
- Notice requirement: artifacts that redistribute 7-Zip must ship its complete
  `License.txt`, the LGPL/BSD notices, and the corresponding-source offer
  required by the selected distribution method. A future RAR-capable variant
  must additionally preserve the restricted UnRAR notice.

## Iosevka

- Project: <https://github.com/be5invis/Iosevka>
- Bundled version: 34.8.0
- License: SIL Open Font License 1.1
- Use in Marcel: `assets/fonts/MarcelIosevka-Regular.ttf` and
  `assets/fonts/MarcelIosevka-SemiBold.ttf` are mechanically subset from the
  official monospaced TTF release and renamed to the private `Marcel Iosevka`
  family. They are loaded directly by GPUI and are not installed into the
  user's system font registry.
- Reproduction and notices: `scripts/build_identity_assets.py` pins the source
  URL, archive hash, selected Unicode ranges, and transformation.
  `assets/fonts/OFL-Iosevka.md` contains the complete upstream license.

## Nordzy

- Project: <https://github.com/alvatip/Nordzy-icon>
- Bundled version: 1.8.7
- License: GPL-3.0-only
- Use in Marcel: `assets/icons/nordzy` contains twenty unmodified scalable
  Places and MIME icons selected by semantic name. The files are a private
  in-application fallback, not a registered or system-installed icon theme.
- Reproduction and notices: `scripts/build_identity_assets.py` pins the source
  archive and hash and records each source-to-destination mapping.
  `assets/icons/nordzy/COPYING` contains the complete upstream license.
