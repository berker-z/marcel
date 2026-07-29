# Sprint 5 — Incremental directory watching

## Goal

Keep the active directory current when other applications mutate it without
turning every notification into a full reload or foreground-executor work.

## Implementation

- [x] Watch only the active local directory and replace the watcher on
  navigation.
- [x] Prefer the platform-recommended `notify` backend and fall back to a
  one-second polling watcher when setup fails.
- [x] Coalesce noisy events for 250 ms, cap continuous coalescing at one second,
  deduplicate paths, and bound batches to 4,096 paths.
- [x] Revalidate final filesystem metadata off the GPUI foreground executor
  instead of treating raw create/modify/remove event labels as authoritative.
- [x] Apply each event batch through `DirectorySession`, rebuilding the visible
  projection and reconciling selection once.
- [x] Refresh a changed selected-file preview and invalidate changed image
  thumbnails without allowing an in-flight stale decode to publish.
- [x] Reject stale watcher generations after navigation.
- [x] Fall back to a full rescan for backend errors, watched-directory changes,
  ambiguous event kinds, and oversized batches.
- [x] Record the Yazi influence and exact source in
  `THIRD_PARTY_NOTICES.md`.

## Deliberate limits

- Marcel's own completed write operations still request a conservative full
  directory load. Replacing that with explicit operation-to-watcher reporting
  is a follow-up after watcher behavior is proven.
- Only the active browser directory is watched. Folder previews, bookmarked
  directories, and inactive history entries do not consume watcher resources.
- Native remote-filesystem behavior varies. The polling fallback provides
  eventual updates, but mounts and remote locations need dedicated acceptance
  work.

## Manual acceptance

- [ ] Creating, modifying, renaming, and deleting a file from another terminal
  updates list and icon views without pressing Refresh.
- [ ] New entries respect directory-first sorting, Show Hidden, and an active
  fuzzy filter.
- [ ] Deleting the selected file clears or reconciles selection and preview;
  modifying it refreshes the preview.
- [ ] Rapid saves coalesce without visible flicker or repeated full-directory
  loading states.
- [ ] Navigating rapidly between directories never publishes an old
  directory's events into the current one.
- [ ] External changes remain responsive in the 10,000-entry fixture near both
  the beginning and end of list and icon views.

## Quality gate

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
