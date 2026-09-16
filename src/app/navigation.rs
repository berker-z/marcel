//! Where the window is and how it got there: loading a folder or the Trash,
//! watching it for changes, moving through history, revealing entries, and
//! folding the effects of operations back into the listing.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use gpui::{Context, ScrollStrategy, Task, UniformListScrollHandle, Window};

use crate::{
    browse::{
        directory_session::{ApplyDirectoryEvents, DirectoryEvent, ReconcileSelection},
        entries::{DirectoryUpdate, FileEntry, sort_entries, stream_directory},
        watcher::{DirectoryWatcherUpdate, revalidate_paths, watch_directory},
    },
    desktop::icons::IconProvider,
    fsops::{
        DirectoryChanges, RECOVERY_REMNANT_PREFIX, process_is_running,
        reclaim_abandoned_quarantines,
        trash::{TrashRecord, list_trash_records, unreadable_trash_warning},
    },
    operations::OperationEvent,
};

use super::{Marcel, state::ViewMode};

/// Run blocking work on the background pool.
pub(super) fn unblock<T: Send + 'static>(
    cx: &Context<Marcel>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Task<T> {
    cx.background_executor().spawn(smol::unblock(work))
}

/// Drive a channel of background updates into the window on the foreground.
///
/// `handle` returns whether to keep listening; the loop also ends when the
/// window is gone or the sender hangs up.
pub(super) fn pump<T: Send + 'static>(
    cx: &mut Context<Marcel>,
    receiver: async_channel::Receiver<T>,
    mut handle: impl FnMut(&mut Marcel, T, &mut Context<Marcel>) -> bool + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Ok(update) = receiver.recv().await {
            let keep_going = this
                .update(cx, |this, cx| handle(this, update, cx))
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        }
    })
}

impl Marcel {
    /// Reset everything a fresh listing invalidates.
    fn begin_listing(&mut self, clear_filter: bool) {
        self.ui.rename = None;
        self.ui.entry_menu = None;
        self.sidebar.bookmark_menu = None;
        self.preview.reset_thumbnails();
        self.drag.entry_content_bounds.borrow_mut().clear();
        self.clear_selection();
        if clear_filter {
            // A navigation shows a new folder from the top. Only a refresh of
            // the same folder — which never clears the filter — keeps the
            // user's place; the old pixel offset in a different folder landed
            // them somewhere arbitrary.
            self.ui.directory_scroll = UniformListScrollHandle::new();
        }
    }

    pub(super) fn start_directory_load(&mut self, clear_filter: bool, cx: &mut Context<Self>) {
        if self.sidebar.browsing_trash {
            self.start_trash_load(clear_filter, cx);
            return;
        }
        self.begin_listing(clear_filter);
        let (ticket, path) = self.directory.begin_load(clear_filter);
        // Watch from the start of the enumeration, not its end: a change
        // arriving while a large directory streamed used to be lost for good.
        // Events that arrive while the stream owns the listing are deferred
        // and re-validated at `Done`.
        self.start_directory_watcher(ticket, path.clone(), cx);

        let (sender, receiver) = async_channel::unbounded();
        let stream_path = path.clone();
        unblock(cx, move || stream_directory(&stream_path, sender, None)).detach();

        self.directory.load_task = Some(pump(cx, receiver, move |this, update, cx| {
            if ticket != this.directory.generation {
                return false;
            }
            match update {
                DirectoryUpdate::Batch(batch) => {
                    let reconcile = this.directory.merge_batch(batch);
                    this.apply_selection_reconcile(reconcile, cx);
                    this.select_pending_loaded_entries(cx);
                }
                DirectoryUpdate::Degraded { skipped, examples } => {
                    let examples = examples.join("; ");
                    this.directory.warning = Some(if examples.is_empty() {
                        format!("Skipped {skipped} unreadable folder entries")
                    } else {
                        format!("Skipped {skipped} unreadable folder entries: {examples}")
                    });
                }
                DirectoryUpdate::Done => {
                    this.directory.finish_load();
                    // A replacement quarantine owned by a process that is gone
                    // can never be restored, so it is unreachable garbage
                    // rather than data anyone might still want. Unlike an
                    // interrupted permanent deletion, it needs no guidance.
                    let reclaim = path.clone();
                    unblock(cx, move || reclaim_abandoned_quarantines(&reclaim)).detach();
                    if let Some(quarantine_warning) =
                        quarantine_recovery_warning(&this.directory.entries)
                    {
                        this.directory.warning = Some(
                            this.directory
                                .warning
                                .take()
                                .map_or(quarantine_warning.clone(), |warning| {
                                    format!("{warning}. {quarantine_warning}")
                                }),
                        );
                    }
                    // Settle whatever changed while the stream owned the listing.
                    if this.directory.take_pending_rescan() {
                        this.start_directory_load(false, cx);
                        return false;
                    }
                    let deferred = this.directory.take_pending_refresh();
                    if !deferred.is_empty() {
                        this.apply_directory_changes(
                            DirectoryChanges::upserted(deferred),
                            None,
                            cx,
                        );
                    }
                    // The listing is final now, so a reveal that scrolled
                    // against a partial one gets its real row.
                    this.settle_revealed_scroll();
                }
                DirectoryUpdate::Error(error) => this.directory.fail_load(error),
            }
            cx.notify();
            true
        }));
        cx.notify();
    }

    pub(super) fn start_trash_load(&mut self, clear_filter: bool, cx: &mut Context<Self>) {
        self.sidebar.browsing_trash = true;
        self.begin_listing(clear_filter);
        let ticket = self.directory.begin_virtual_load(clear_filter);
        self.sidebar.trash_records.clear();

        let load = unblock(cx, move || {
            let listing = list_trash_records()?;
            let mut icons = IconProvider::discover();
            let mut entries = Vec::with_capacity(listing.records.len());
            let mut by_backing = HashMap::with_capacity(listing.records.len());
            let mut unreadable = listing.unreadable;
            for record in listing.records {
                // A record Marcel parsed but cannot present is as absent from
                // the listing as one it could not parse, and is missing from
                // Empty Trash for the same reason.
                match trash_entry(&record, &mut icons) {
                    Ok(entry) => {
                        by_backing.insert(entry.path.clone(), record);
                        entries.push(entry);
                    }
                    Err(error) => {
                        unreadable.push(format!("“{}”: {error}", record.original_path().display()));
                    }
                }
            }
            sort_entries(&mut entries);
            anyhow::Ok((entries, by_backing, unreadable))
        });

        self.directory.load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.directory.generation || !this.sidebar.browsing_trash {
                    return;
                }
                match result {
                    Ok((entries, records, unreadable)) => {
                        this.sidebar.trash_records = records;
                        let reconcile = this.directory.merge_batch(entries);
                        this.apply_selection_reconcile(reconcile, cx);
                        this.directory.finish_load();
                        // Say what is missing rather than presenting a partial
                        // enumeration as the whole Trash.
                        this.directory.warning = unreadable_trash_warning(&unreadable);
                        this.sidebar.unreadable_trash_entries = unreadable.len();
                    }
                    Err(error) => this.directory.fail_load(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_directory_watcher(&mut self, ticket: u64, path: PathBuf, cx: &mut Context<Self>) {
        let (sender, receiver) = async_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        let watcher_cancelled = cancelled.clone();
        let watched_path = path.clone();
        unblock(cx, move || {
            watch_directory(&watched_path, sender, watcher_cancelled)
        })
        .detach();

        let task = pump(cx, receiver, move |this, update, cx| {
            if ticket != this.directory.generation || path != this.directory.current_dir {
                return false;
            }
            // While the load streams, the stream owns the listing: applying
            // events mid-stream inserted entries the stream then inserted
            // again. Defer the paths; `Done` re-validates them.
            let loading = this.directory.loading;
            match update {
                DirectoryWatcherUpdate::Events(events) if loading => {
                    if events
                        .iter()
                        .any(|event| matches!(event, DirectoryEvent::RescanRequired))
                    {
                        this.directory.defer_rescan();
                    } else {
                        this.directory.defer_refresh(directory_event_paths(&events));
                    }
                }
                DirectoryWatcherUpdate::RescanRequired if loading => this.directory.defer_rescan(),
                DirectoryWatcherUpdate::Events(events) => {
                    if !this.apply_events(events, cx) {
                        return false;
                    }
                }
                DirectoryWatcherUpdate::RescanRequired => {
                    this.start_directory_load(false, cx);
                    return false;
                }
            }
            cx.notify();
            true
        });
        self.directory.set_watcher(cancelled, task);
    }

    /// Fold validated events into the listing. Returns `false` when the
    /// listing could not absorb them and a reload has been started instead.
    fn apply_events(&mut self, events: Vec<DirectoryEvent>, cx: &mut Context<Self>) -> bool {
        let affected = directory_event_paths(&events);
        match self.directory.apply_events(events) {
            ApplyDirectoryEvents::Applied(reconcile) => {
                self.preview.invalidate_thumbnails(&affected);
                self.apply_selection_reconcile(reconcile, cx);
                self.select_pending_loaded_entries(cx);
                true
            }
            ApplyDirectoryEvents::RescanRequired => {
                self.start_directory_load(false, cx);
                false
            }
        }
    }

    pub(super) fn apply_selection_reconcile(
        &mut self,
        reconcile: ReconcileSelection,
        cx: &mut Context<Self>,
    ) {
        match reconcile {
            ReconcileSelection::Unchanged => {}
            ReconcileSelection::Preview(entry) => self.start_preview(entry, cx),
            ReconcileSelection::ClearPreview => self.preview.clear(),
        }
    }

    fn select_pending_loaded_entries(&mut self, cx: &mut Context<Self>) {
        let entries = self.directory.take_pending_visible_entries();
        let Some(primary) = self.directory.selection.primary().cloned() else {
            return;
        };
        let Some(entry) = entries.into_iter().find(|entry| entry.path == primary) else {
            return;
        };
        // Revealing means *showing*: selecting an item somewhere past the
        // viewport and leaving the scroll where it was fails the feature's
        // whole purpose.
        self.scroll_to_revealed(&primary);
        // Mid-stream the row is provisional. Later batches merge into the
        // sorted listing on either side of this entry, so the row it occupies
        // now is not the row it will occupy at `Done`; scrolling once here
        // leaves the revealed file off screen by however much the rest of the
        // enumeration shifted it.
        if self.directory.loading {
            self.directory.defer_reveal_scroll(primary);
        }
        self.start_preview(entry, cx);
    }

    /// The uniform-list row a path occupies in the current view.
    pub(super) fn scroll_row_of(&self, path: &Path) -> Option<usize> {
        let index = self.directory.visible_position(path)?;
        Some(match self.ui.view_mode {
            ViewMode::List => index,
            ViewMode::Grid => index / self.grid_columns().max(1),
        })
    }

    /// Put the revealed row on screen, centred, at its current index.
    fn scroll_to_revealed(&mut self, path: &Path) {
        if let Some(row) = self.scroll_row_of(path) {
            self.ui
                .directory_scroll
                .scroll_to_item(row, ScrollStrategy::Center);
        }
    }

    /// Re-apply a mid-stream reveal's scroll once the listing has settled.
    ///
    /// Runs after the deferred refresh batch, because that batch can move the
    /// row one last time. The file may be gone by then, in which case the
    /// selection reconcile has already dealt with it.
    fn settle_revealed_scroll(&mut self) {
        let Some(target) = self.directory.take_reveal_scroll_target() else {
            return;
        };
        if self.directory.selection.primary() == Some(&target) {
            self.scroll_to_revealed(&target);
        }
    }

    pub(super) fn navigate_to(
        &mut self,
        path: PathBuf,
        add_to_history: bool,
        cx: &mut Context<Self>,
    ) {
        self.navigate_to_revealing(path, Vec::new(), add_to_history, cx);
    }

    fn navigate_to_revealing(
        &mut self,
        path: PathBuf,
        reveal: Vec<PathBuf>,
        add_to_history: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar.browsing_trash && path == self.directory.current_dir {
            if !reveal.is_empty() {
                self.set_filter_query(String::new(), cx);
                self.directory.replace_pending_reveal(reveal);
                self.preview.clear();
                self.select_pending_loaded_entries(cx);
                cx.notify();
            }
            return;
        }
        if add_to_history {
            self.history.push(&path);
        }
        self.show_directory(path, reveal, cx);
    }

    fn show_directory(&mut self, path: PathBuf, reveal: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.sidebar.browsing_trash = false;
        self.sidebar.trash_records.clear();
        self.directory.current_dir = path;
        self.directory.pending_reveal = reveal;
        self.start_directory_load(true, cx);
    }

    pub fn open_external_location(
        &mut self,
        directory: PathBuf,
        reveal: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_to_revealing(directory, reveal, true, cx);
        self.focus_browser(window, cx);
    }

    pub(super) fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.show_directory(path, Vec::new(), cx);
        }
    }

    pub(super) fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.show_directory(path, Vec::new(), cx);
        }
    }

    pub(super) fn go_up(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.browsing_trash {
            return;
        }
        if let Some(parent) = self.directory.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
    }

    /// Fold an application-wide operation effect into this window.
    ///
    /// Every window hears every effect, so a second window showing the folder a
    /// transfer landed in reconciles from the operation itself rather than
    /// waiting for a watcher event. Reveal is the one part that is not shared:
    /// it belongs to the window that asked for the work.
    pub(super) fn on_operation_event(
        &mut self,
        event: &OperationEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            OperationEvent::Applied {
                changes,
                reveal,
                origin,
            } => {
                let here = self.directory.current_dir.as_path();
                let reveal = origin
                    .is_none_or(|origin| origin == Self::origin(window))
                    .then(|| {
                        reveal
                            .iter()
                            .find(|path| path.parent() == Some(here))
                            .cloned()
                    })
                    .flatten();
                self.apply_directory_changes(changes.clone(), reveal, cx);
            }
            OperationEvent::TrashRemoved(backing_paths) => {
                self.remove_trash_entries(backing_paths.clone(), cx);
            }
            OperationEvent::TrashUpserted(records) => {
                self.upsert_trash_entries(records.clone(), cx)
            }
            OperationEvent::TrashInvalidated => {
                if self.sidebar.browsing_trash {
                    self.start_trash_load(false, cx);
                }
            }
        }
    }

    fn apply_directory_changes(
        &mut self,
        changes: DirectoryChanges,
        reveal: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.browsing_trash {
            return;
        }
        let current_dir = self.directory.current_dir.clone();
        // Every path is re-stat'd before it is applied — the removed ones too.
        // An operation's `removed` can be stale by the time it lands: an
        // external process may have recreated the path, and applying the
        // removal verbatim deleted an existing file from the view.
        let candidates = changes
            .removed
            .into_iter()
            .chain(changes.upserted)
            .filter(|path| path.parent() == Some(current_dir.as_path()))
            .collect::<Vec<_>>();
        let reveal = reveal.filter(|path| path.parent() == Some(current_dir.as_path()));
        if candidates.is_empty() && reveal.is_none() {
            return;
        }
        if self.directory.loading {
            // The stream owns the listing; fold this into the catch-up set the
            // finished load re-validates. The reveal joins the pending set,
            // which already selects entries as their batches arrive.
            self.directory.defer_refresh(candidates);
            self.directory.pending_reveal.extend(reveal);
            return;
        }

        let generation = self.directory.generation;
        let task = unblock(cx, move || revalidate_paths(candidates));
        cx.spawn(async move |this, cx| {
            let events = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.sidebar.browsing_trash
                    || generation != this.directory.generation
                    || current_dir != this.directory.current_dir
                {
                    return;
                }
                // Only an operation that carries a reveal may touch the
                // pending set here: replacing it unconditionally wiped out an
                // in-flight navigation's reveal whenever an unrelated
                // operation completed.
                let had_reveal = reveal.is_some();
                if let Some(path) = reveal {
                    this.directory.pending_reveal = vec![path];
                }
                if this.apply_events(events, cx) {
                    // Unlike a streamed directory load, this is the only
                    // result batch. Do not retain an impossible reveal across
                    // later unrelated watcher events.
                    if had_reveal {
                        this.directory.pending_reveal.clear();
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn remove_trash_entries(&mut self, backing_paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.sidebar.browsing_trash || backing_paths.is_empty() {
            return;
        }
        for path in &backing_paths {
            self.sidebar.trash_records.remove(path);
        }
        let events = backing_paths
            .iter()
            .cloned()
            .map(DirectoryEvent::Removed)
            .collect();
        self.apply_trash_events(events, cx);
    }

    fn upsert_trash_entries(&mut self, records: Vec<TrashRecord>, cx: &mut Context<Self>) {
        if !self.sidebar.browsing_trash || records.is_empty() {
            return;
        }
        let generation = self.directory.generation;
        let task = unblock(cx, move || {
            let mut icons = IconProvider::discover();
            records
                .into_iter()
                .filter_map(|record| Some((trash_entry(&record, &mut icons).ok()?, record)))
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let entries = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.sidebar.browsing_trash || generation != this.directory.generation {
                    return;
                }
                let mut events = Vec::with_capacity(entries.len());
                for (entry, record) in entries {
                    this.sidebar
                        .trash_records
                        .insert(entry.path.clone(), record);
                    events.push(DirectoryEvent::Changed(entry));
                }
                this.apply_trash_events(events, cx);
            });
        })
        .detach();
    }

    fn apply_trash_events(&mut self, events: Vec<DirectoryEvent>, cx: &mut Context<Self>) {
        let affected = directory_event_paths(&events);
        match self.directory.apply_events(events) {
            ApplyDirectoryEvents::Applied(reconcile) => {
                self.preview.invalidate_thumbnails(&affected);
                self.apply_selection_reconcile(reconcile, cx);
                cx.notify();
            }
            ApplyDirectoryEvents::RescanRequired => self.start_trash_load(false, cx),
        }
    }
}

/// A Trash entry as the listing shows it: the backing file's metadata under
/// the original name, and never navigable — the first Trash slice presents
/// top-level items only.
fn trash_entry(record: &TrashRecord, icons: &mut IconProvider) -> std::io::Result<FileEntry> {
    let mut entry = FileEntry::from_path(record.backing_path(), icons)?;
    // `set_name` rather than assigning `name`: `name_os` and `folded_name`
    // must follow, or the row sorts and filters by a name the user cannot see.
    if let Some(name) = record.original_path().file_name() {
        entry.set_name(name.to_os_string());
    }
    entry.navigable = false;
    Ok(entry)
}

fn directory_event_paths(events: &[DirectoryEvent]) -> Vec<PathBuf> {
    let mut paths = std::collections::HashSet::with_capacity(events.len());
    for event in events {
        match event {
            DirectoryEvent::Added(entry) | DirectoryEvent::Changed(entry) => {
                paths.insert(entry.path.clone());
            }
            DirectoryEvent::Removed(path) => {
                paths.insert(path.clone());
            }
            DirectoryEvent::Renamed { from, entry } => {
                paths.insert(from.clone());
                paths.insert(entry.path.clone());
            }
            DirectoryEvent::RescanRequired => {}
        }
    }
    paths.into_iter().collect()
}

/// Guidance for data Marcel is holding but no longer manages.
///
/// Two unrelated things end up here. An interrupted permanent deletion leaves a
/// quarantine no live process owns. A failed restoration leaves the *original*
/// of a replacement Marcel could not put back — which is the user's only copy,
/// which is why it carries no process id and no sweep will ever remove it.
/// Both are pointed at rather than hidden, because guidance naming a path the
/// browser refuses to show cannot be followed.
fn quarantine_recovery_warning(entries: &[FileEntry]) -> Option<String> {
    let remnants = |matches: &dyn Fn(&str) -> bool| {
        let mut paths = entries
            .iter()
            .filter(|entry| matches(&entry.name))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    };
    // A quarantine whose owner is still running is a deletion in progress,
    // not an interrupted one — two Marcel processes can exist when desktop
    // integration is unavailable, and inviting the user to move data out from
    // under an active delete would be worse than saying nothing.
    let interrupted = remnants(&|name| {
        delete_quarantine_owner(name)
            .is_some_and(|owner| owner != std::process::id() && !process_is_running(owner))
    });
    let unrestored = remnants(&|name| name.starts_with(RECOVERY_REMNANT_PREFIX));

    let mut notices = Vec::new();
    if let Some(first) = interrupted.first() {
        notices.push(if interrupted.len() == 1 {
            format!(
                "An interrupted permanent deletion left data quarantined at “{}”. Inspect this hidden path and move anything you want to recover to a free destination",
                first.display()
            )
        } else {
            format!(
                "{} interrupted permanent deletions left quarantined data here (first: “{}”). Inspect these hidden paths and move anything you want to recover to free destinations",
                interrupted.len(),
                first.display()
            )
        });
    }
    if let Some(first) = unrestored.first() {
        notices.push(if unrestored.len() == 1 {
            format!(
                "Marcel could not put back an item it had moved aside to replace, and preserved your original at “{}”. Move it somewhere safe",
                first.display()
            )
        } else {
            format!(
                "{} originals Marcel could not put back are preserved here (first: “{}”). Move them somewhere safe",
                unrestored.len(),
                first.display()
            )
        });
    }
    (!notices.is_empty()).then(|| notices.join(". "))
}

/// The process that owns a `.marcel-delete-<pid>-<seq>-<name>` quarantine, if
/// the name has exactly that shape.
fn delete_quarantine_owner(name: &str) -> Option<u32> {
    let mut fields = name.strip_prefix(".marcel-delete-")?.splitn(3, '-');
    let owner = fields.next()?.parse().ok()?;
    let sequence = fields.next()?;
    let original = fields.next()?;
    (!sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && !original.is_empty())
    .then_some(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_file_entry;

    /// A pid no Linux kernel hands out: `pid_max` tops out at 2^22, so
    /// `/proc/<this>` can never exist and the owner is provably dead.
    const DEAD_PROCESS: u32 = u32::MAX;

    #[test]
    fn completed_load_warns_about_an_interrupted_delete_quarantine() {
        let remnant = test_file_entry(
            &format!("/folder/.marcel-delete-{DEAD_PROCESS}-4-thesis"),
            false,
        );
        let warning = quarantine_recovery_warning(&[remnant]).unwrap();

        assert!(warning.contains("interrupted permanent deletion"));
        assert!(warning.contains(".marcel-delete-"));
        assert!(warning.contains("move anything you want to recover"));
    }

    /// An original Marcel failed to put back is the user's only copy of it, so
    /// it is pointed at rather than hidden — and it is not process-scoped, so
    /// the process that created it is irrelevant to the guidance.
    #[test]
    fn completed_load_points_at_an_original_that_could_not_be_put_back() {
        let preserved = test_file_entry("/folder/.marcel-recovered-0-report.txt", false);
        let warning = quarantine_recovery_warning(std::slice::from_ref(&preserved)).unwrap();
        assert!(
            warning.contains(".marcel-recovered-0-report.txt"),
            "{warning}"
        );
        assert!(warning.contains("could not put back"), "{warning}");
        assert!(
            !crate::fsops::is_internal_working_name(&preserved.name_os),
            "guidance that names a hidden path cannot be followed"
        );

        // Both kinds of remnant can be present, and both are reported.
        let interrupted = test_file_entry(
            &format!("/folder/.marcel-delete-{DEAD_PROCESS}-4-thesis"),
            false,
        );
        let warning = quarantine_recovery_warning(&[preserved, interrupted]).unwrap();
        assert!(warning.contains(".marcel-recovered-"), "{warning}");
        assert!(warning.contains(".marcel-delete-"), "{warning}");
    }

    #[test]
    fn active_delete_quarantines_do_not_trigger_recovery_guidance() {
        let active = test_file_entry(
            &format!("/folder/.marcel-delete-{}-0-active", std::process::id()),
            false,
        );
        let ordinary = test_file_entry("/folder/.marcel-delete-not-a-quarantine", false);
        // A quarantine owned by a *different* live process is a deletion in
        // progress — another Marcel instance mid-delete — not an interruption.
        // Pid 1 is always running.
        let another_live = test_file_entry("/folder/.marcel-delete-1-0-busy", false);

        assert!(quarantine_recovery_warning(&[active]).is_none());
        assert!(quarantine_recovery_warning(&[ordinary]).is_none());
        assert!(quarantine_recovery_warning(&[another_live]).is_none());
    }
}
