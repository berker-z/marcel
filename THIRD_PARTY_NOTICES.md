# Third-party notices and adapted code

Marcel depends on third-party Rust crates whose license information is recorded
in Cargo metadata. This file additionally tracks source code substantially
adapted into Marcel itself.

## Yazi

- Project: <https://github.com/sxyazi/yazi>
- License: MIT
- Copyright: Yazi contributors
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

Yazi is a primary architectural influence for asynchronous filesystem work,
task scheduling, cancellation, previews, and responsiveness. Future direct or
substantial adaptations will list the upstream path, Marcel path, and a short
description here.

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
