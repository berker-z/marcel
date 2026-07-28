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
