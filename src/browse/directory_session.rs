use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::browse::entries::{FileEntry, SortKey, SortOrder, merge_sorted_entries, sort_entries};
use crate::browse::selection::SelectionModel;

/// Background work that stops when dropped.
///
/// The session ties its load and its watcher to its own lifetime without
/// knowing what runs them. `app` hands in its executor's task handles; this
/// module never names the UI toolkit, as the module doc promises.
pub type WorkHandle = Box<dyn Any>;

/// The least time between two rescans of one folder.
///
/// A rescan restreams the whole folder, and a folder that churns faster than
/// it streams (a build tree, a log spray) asked for another before the first
/// had finished, so the window reloaded in a loop. Waiting this long between
/// them bounds the load to well under one a second, which is as fast as a
/// listing needs to catch up with a folder nobody can read that quickly.
pub const RESCAN_BACKOFF: Duration = Duration::from_millis(1500);

/// Why a load starts, which decides what happens to the listing shown now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadKind {
    /// A different folder: nothing on screen belongs to it, so the listing,
    /// the selection, and the filter all go at once.
    Navigate,
    /// The same folder again. What is shown stays, selection included, until
    /// the new listing has fully arrived and replaces it in one step. A
    /// refresh that blanked the window and dropped the selection punished
    /// the user for a change they did not make.
    Refresh,
}

/// What a rescan request should do right now.
#[derive(Debug, PartialEq, Eq)]
pub enum RescanDecision {
    /// Start the reload immediately.
    Now,
    /// Wait this long, then start it, unless a newer load supersedes it.
    After(Duration),
    /// One is already waiting; this request adds nothing.
    AlreadyScheduled,
}

/// How the listing on screen relates to the last rescan of it.
struct RescanBackoff {
    directory: PathBuf,
    started: Instant,
}

/// The finished load's effect on what the window shows.
pub struct LoadFinished {
    pub reconcile: ReconcileSelection,
    /// Entries a refresh found changed or gone, so cached renderings of them
    /// (thumbnails) are stale. Empty for a fresh load, which had none.
    pub changed: Vec<PathBuf>,
}

/// Lazily rebuilt path lookup over `entries`.
///
/// Resolving a path by linear scan is fine once, but the browser does it
/// several times per frame — the render pass, every `command_enabled` query,
/// and each context-menu item — so a 50,000-entry directory paid tens of
/// thousands of path comparisons per frame. Rebuilding lazily keeps streamed
/// loads from paying for an index nobody has asked for yet.
#[derive(Default)]
struct EntryIndex {
    revision: u64,
    lookup: HashMap<PathBuf, usize>,
}

pub struct DirectorySession {
    pub(crate) current_dir: PathBuf,
    pub(crate) entries: Vec<FileEntry>,
    pub(crate) visible_entries: Vec<usize>,
    pub(crate) filter_query: String,
    pub(crate) show_hidden: bool,
    /// The order `entries` is kept in. Every merge and re-sort honours it.
    pub(crate) sort: SortOrder,
    pub(crate) selection: SelectionModel,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) warning: Option<String>,
    pub(crate) generation: u64,
    load_task: Option<WorkHandle>,
    watch_task: Option<WorkHandle>,
    /// Flipped when the load in flight is superseded, so the walker and the
    /// watcher stop stat-ing a folder nobody is looking at any more.
    cancel: Arc<AtomicBool>,
    /// A refresh's listing as it streams in. It replaces `entries` in one
    /// step at `finish_load`; until then the old listing stays on screen.
    staged: Option<Vec<FileEntry>>,
    last_rescan: Option<RescanBackoff>,
    rescan_scheduled: bool,
    pub(crate) pending_reveal: Vec<PathBuf>,
    /// A revealed path whose scroll position is not final yet, because it was
    /// revealed out of a batch while the enumeration was still streaming.
    ///
    /// Selecting and previewing the moment the entry appears is what a reveal
    /// should do, and that part is correct as soon as the batch lands. Its
    /// *row* is not: later batches merge into the sorted listing on both sides
    /// of the entry, so the index the scroll was computed from stops being the
    /// index the entry ends up at. The scroll is therefore re-applied once the
    /// listing settles.
    pub(crate) reveal_scroll_target: Option<PathBuf>,
    /// Paths that changed while a load was streaming, held to be re-validated
    /// once the enumeration settles. Applying them mid-stream raced the
    /// stream: an entry inserted by an event was inserted again when the
    /// stream reached the same name.
    pending_refresh: HashSet<PathBuf>,
    /// Whether a full rescan was requested while a load was streaming.
    pending_rescan: bool,
    entries_revision: u64,
    projection_revision: u64,
    entry_index: RefCell<EntryIndex>,
    /// A picker's file filter: which files stay visible alongside the folders.
    ///
    /// Applied under the hidden-file rule and before the fuzzy filter, so
    /// type-to-filter searches only what the filter admits.
    content_filter: Option<ContentFilter>,
}

/// Decides whether an entry belongs in the visible listing at all.
pub type ContentFilter = Arc<dyn Fn(&FileEntry) -> bool>;

impl DirectorySession {
    pub fn new(current_dir: PathBuf) -> Self {
        Self {
            current_dir,
            entries: Vec::new(),
            visible_entries: Vec::new(),
            filter_query: String::new(),
            show_hidden: true,
            sort: SortOrder::default(),
            selection: SelectionModel::default(),
            loading: false,
            error: None,
            warning: None,
            generation: 0,
            load_task: None,
            watch_task: None,
            cancel: Arc::new(AtomicBool::new(false)),
            staged: None,
            last_rescan: None,
            rescan_scheduled: false,
            pending_reveal: Vec::new(),
            reveal_scroll_target: None,
            pending_refresh: HashSet::new(),
            pending_rescan: false,
            entries_revision: 1,
            projection_revision: 1,
            entry_index: RefCell::new(EntryIndex::default()),
            content_filter: None,
        }
    }

    /// Replace the content filter and re-project the listing under it.
    pub fn set_content_filter(&mut self, filter: Option<ContentFilter>) -> ReconcileSelection {
        self.content_filter = filter;
        self.rebuild_visible_entries();
        self.reconcile_selection()
    }

    /// Whether the hidden-file rule and the content filter admit `entry`.
    fn admits(&self, entry: &FileEntry) -> bool {
        !crate::fsops::is_internal_working_name(&entry.name_os)
            && (self.show_hidden || !is_hidden_os_name(&entry.name_os))
            && self.content_filter.as_ref().is_none_or(|filter| filter(entry))
    }

    /// Note paths whose state changed while the load streams, so the finished
    /// load can re-validate them.
    pub fn defer_refresh(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.pending_refresh.extend(paths);
    }

    /// Note that the listing cannot be trusted and must be reloaded once the
    /// load in flight finishes.
    pub fn defer_rescan(&mut self) {
        self.pending_rescan = true;
    }

    pub fn take_pending_refresh(&mut self) -> Vec<PathBuf> {
        self.pending_refresh.drain().collect()
    }

    pub fn take_pending_rescan(&mut self) -> bool {
        std::mem::take(&mut self.pending_rescan)
    }

    /// Resolve one entry by path in constant time.
    pub fn entry(&self, path: &Path) -> Option<&FileEntry> {
        let position = {
            let mut index = self.entry_index.borrow_mut();
            if index.revision != self.entries_revision {
                index.lookup.clear();
                index.lookup.reserve(self.entries.len());
                for (position, entry) in self.entries.iter().enumerate() {
                    index.lookup.insert(entry.path.clone(), position);
                }
                index.revision = self.entries_revision;
            }
            index.lookup.get(path).copied()
        };
        position.and_then(|position| self.entries.get(position))
    }

    /// Invalidate the path lookup. Must be called wherever `entries` changes.
    fn mark_entries_changed(&mut self) {
        self.entries_revision = self.entries_revision.wrapping_add(1);
    }

    /// Bumped whenever the visible projection changes, so derived per-frame
    /// state such as the drag payload knows when it may be reused.
    pub fn projection_revision(&self) -> u64 {
        self.projection_revision
    }

    pub fn apply_events(&mut self, events: Vec<DirectoryEvent>) -> ApplyDirectoryEvents {
        let primary = self.selection.primary().cloned();
        let primary_changed = primary.as_ref().is_some_and(|primary| {
            events.iter().any(
                |event| matches!(event, DirectoryEvent::Changed(entry) if &entry.path == primary),
            )
        });
        let mut removed = HashSet::with_capacity(events.len());
        let mut upserts = HashMap::with_capacity(events.len());
        for event in events {
            match event {
                DirectoryEvent::Added(entry) | DirectoryEvent::Changed(entry) => {
                    removed.remove(&entry.path);
                    upserts.insert(entry.path.clone(), entry);
                }
                DirectoryEvent::Removed(path) => {
                    upserts.remove(&path);
                    removed.insert(path);
                }
                DirectoryEvent::Renamed { from, entry } => {
                    removed.insert(from);
                    removed.remove(&entry.path);
                    upserts.insert(entry.path.clone(), entry);
                }
                DirectoryEvent::RescanRequired => {
                    return ApplyDirectoryEvents::RescanRequired;
                }
            }
        }

        self.entries
            .retain(|entry| !removed.contains(&entry.path) && !upserts.contains_key(&entry.path));
        let mut upserts = upserts.into_values().collect::<Vec<_>>();
        sort_entries(&mut upserts, self.sort);
        self.entries = merge_sorted_entries(std::mem::take(&mut self.entries), upserts, self.sort);
        self.mark_entries_changed();
        self.rebuild_visible_entries();
        let mut reconcile = self.reconcile_selection();
        if matches!(reconcile, ReconcileSelection::Unchanged)
            && primary_changed
            && let Some(entry) = primary
                .as_ref()
                .and_then(|primary| self.entries.iter().find(|entry| &entry.path == primary))
                .cloned()
        {
            reconcile = ReconcileSelection::Preview(entry);
        }
        ApplyDirectoryEvents::Applied(reconcile)
    }

    pub fn begin_load(&mut self, kind: LoadKind) -> (u64, PathBuf) {
        let generation = self.begin_virtual_load(kind);
        (generation, self.current_dir.clone())
    }

    pub fn begin_virtual_load(&mut self, kind: LoadKind) -> u64 {
        self.cancel_background_work();
        self.generation = self.generation.wrapping_add(1);
        self.pending_refresh.clear();
        self.pending_rescan = false;
        // A rescan waiting on the backoff belongs to the load this replaces;
        // when its timer fires it will find the generation moved on.
        self.rescan_scheduled = false;
        // A target from the load being replaced describes rows that no longer
        // exist. `navigate_to_revealing` sets the new one after this runs.
        self.reveal_scroll_target = None;
        match kind {
            LoadKind::Navigate => {
                self.staged = None;
                self.entries.clear();
                self.mark_entries_changed();
                self.visible_entries.clear();
                self.projection_revision = self.projection_revision.wrapping_add(1);
                self.filter_query.clear();
                self.selection.clear();
            }
            LoadKind::Refresh => self.staged = Some(Vec::new()),
        }
        self.error = None;
        self.warning = None;
        self.loading = true;
        self.generation
    }

    /// Stop whatever the previous load left running and arm a fresh flag for
    /// the next one. Dropping the handles ends the foreground pumps; the flag
    /// is for the blocking walker and watcher, which a drop cannot reach.
    fn cancel_background_work(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.load_task.take();
        self.watch_task.take();
    }

    /// The flag the current load's background work should stop on.
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn set_load_task(&mut self, task: WorkHandle) {
        self.load_task = Some(task);
    }

    pub fn set_watcher(&mut self, task: WorkHandle) {
        self.watch_task = Some(task);
    }

    /// Decide whether a rescan of the folder shown may start now.
    ///
    /// The first request for a folder starts at once; a second within
    /// `RESCAN_BACKOFF` of it waits out the remainder, and requests that
    /// arrive while one is waiting are absorbed by it.
    pub fn schedule_rescan(&mut self, now: Instant) -> RescanDecision {
        if self.rescan_scheduled {
            return RescanDecision::AlreadyScheduled;
        }
        let since_last = self
            .last_rescan
            .as_ref()
            .filter(|last| last.directory == self.current_dir)
            .map(|last| now.saturating_duration_since(last.started));
        if let Some(since_last) = since_last
            && since_last < RESCAN_BACKOFF
        {
            self.rescan_scheduled = true;
            return RescanDecision::After(RESCAN_BACKOFF - since_last);
        }
        self.note_rescan(now);
        RescanDecision::Now
    }

    /// A rescan the backoff held back is starting now.
    pub fn begin_scheduled_rescan(&mut self, now: Instant) {
        self.rescan_scheduled = false;
        self.note_rescan(now);
    }

    fn note_rescan(&mut self, now: Instant) {
        self.last_rescan =
            Some(RescanBackoff { directory: self.current_dir.clone(), started: now });
    }

    /// Fold a streamed batch into the listing.
    ///
    /// The stream sorted the batch in the order it was started with; if the
    /// order changed meanwhile the batch is re-sorted here, which costs
    /// nothing when it was right already. A refresh only collects: its
    /// listing goes on screen whole, at `finish_load`.
    pub fn merge_batch(&mut self, mut batch: Vec<FileEntry>) -> ReconcileSelection {
        if let Some(staged) = self.staged.as_mut() {
            staged.append(&mut batch);
            return ReconcileSelection::Unchanged;
        }
        sort_entries(&mut batch, self.sort);
        self.entries = merge_sorted_entries(std::mem::take(&mut self.entries), batch, self.sort);
        self.mark_entries_changed();
        self.rebuild_visible_entries();
        self.reconcile_selection()
    }

    /// The stream is complete. A refresh swaps its collected listing in here,
    /// and reports which entries it found different from the ones shown.
    pub fn finish_load(&mut self) -> LoadFinished {
        self.loading = false;
        let Some(mut staged) = self.staged.take() else {
            return LoadFinished { reconcile: ReconcileSelection::Unchanged, changed: Vec::new() };
        };
        sort_entries(&mut staged, self.sort);
        let changed = {
            let fresh =
                staged.iter().map(|entry| (entry.path.as_path(), entry)).collect::<HashMap<_, _>>();
            let mut changed = self
                .entries
                .iter()
                .filter(|old| fresh.get(old.path.as_path()) != Some(old))
                .map(|old| old.path.clone())
                .collect::<Vec<_>>();
            let shown =
                self.entries.iter().map(|entry| entry.path.as_path()).collect::<HashSet<_>>();
            changed.extend(
                staged
                    .iter()
                    .filter(|new| !shown.contains(new.path.as_path()))
                    .map(|new| new.path.clone()),
            );
            changed
        };
        self.entries = staged;
        self.mark_entries_changed();
        self.rebuild_visible_entries();
        LoadFinished { reconcile: self.reconcile_selection(), changed }
    }

    /// A load ended without a listing. A refresh drops the one it was keeping
    /// on screen: the error is what the window has to show now, and a listing
    /// the folder no longer backs would only mislead.
    pub fn fail_load(&mut self, error: String) -> ReconcileSelection {
        self.loading = false;
        self.error = Some(error);
        if self.staged.take().is_none() {
            return ReconcileSelection::Unchanged;
        }
        self.entries.clear();
        self.mark_entries_changed();
        self.rebuild_visible_entries();
        self.reconcile_selection()
    }

    /// Forget reveals the finished load could not satisfy, so a file that
    /// appears later under one of those names is not selected out of the blue.
    pub fn clear_pending_reveal(&mut self) {
        self.pending_reveal.clear();
    }

    pub fn set_filter_query(&mut self, query: String) -> Option<ReconcileSelection> {
        if query == self.filter_query {
            return None;
        }

        self.filter_query = query;
        self.rebuild_visible_entries();
        Some(self.reconcile_selection())
    }

    pub fn set_show_hidden(&mut self, show_hidden: bool) -> Option<ReconcileSelection> {
        if show_hidden == self.show_hidden {
            return None;
        }

        self.show_hidden = show_hidden;
        self.rebuild_visible_entries();
        Some(self.reconcile_selection())
    }

    /// Reorder the listing by `key`: its natural direction, or the reverse
    /// when it already was the key. Returns the order now in force.
    pub fn sort_by(&mut self, key: SortKey) -> (SortOrder, ReconcileSelection) {
        self.set_sort(self.sort.choose(key))
    }

    pub fn set_sort(&mut self, order: SortOrder) -> (SortOrder, ReconcileSelection) {
        if order != self.sort {
            self.sort = order;
            sort_entries(&mut self.entries, order);
            self.mark_entries_changed();
            self.rebuild_visible_entries();
        }
        (self.sort, self.reconcile_selection())
    }

    pub fn take_pending_visible_entries(&mut self) -> Vec<FileEntry> {
        if self.pending_reveal.is_empty() {
            return Vec::new();
        }
        let visible = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| (entry.path.clone(), entry.clone()))
            .collect::<HashMap<_, _>>();
        let mut revealed = Vec::new();
        self.pending_reveal.retain(|path| {
            if let Some(entry) = visible.get(path) {
                revealed.push(entry.clone());
                false
            } else {
                true
            }
        });
        self.selection.add_all(revealed.iter().map(|entry| entry.path.clone()));
        revealed
    }

    pub fn replace_pending_reveal(&mut self, paths: Vec<PathBuf>) {
        self.selection.clear();
        self.pending_reveal = paths;
        self.reveal_scroll_target = None;
    }

    /// Remember that `path` was revealed mid-stream and still needs its final
    /// scroll position once the listing settles.
    pub fn defer_reveal_scroll(&mut self, path: PathBuf) {
        self.reveal_scroll_target = Some(path);
    }

    pub fn take_reveal_scroll_target(&mut self) -> Option<PathBuf> {
        self.reveal_scroll_target.take()
    }

    /// Where `path` sits in the visible listing.
    pub fn visible_position(&self, path: &Path) -> Option<usize> {
        self.visible_entries
            .iter()
            .position(|index| self.entries.get(*index).is_some_and(|entry| entry.path == path))
    }

    pub fn visible_entry(&self, index: usize) -> Option<&FileEntry> {
        self.visible_entries.get(index).and_then(|entry_index| self.entries.get(*entry_index))
    }

    pub fn visible_paths(&self) -> Vec<PathBuf> {
        self.visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn rebuild_visible_entries(&mut self) {
        self.projection_revision = self.projection_revision.wrapping_add(1);
        // Yazi keeps finder matches as derived state over the current folder
        // and catches that state up when the folder revision changes. Marcel
        // applies the same separation to a fuzzy-ranked visible-index layer.
        // Source (MIT, upstream commit e58022b9aafc8dabf586e2cc29b79a230071716f):
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-core/src/tab/finder.rs
        if self.filter_query.is_empty() {
            self.visible_entries = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| self.admits(entry).then_some(index))
                .collect();
            return;
        }

        let folded_query = self.filter_query.to_lowercase().chars().collect::<Vec<_>>();
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if !self.admits(entry) {
                    return None;
                }
                fuzzy_score_folded(&entry.folded_name, &folded_query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|(left_index, left_score), (right_index, right_score)| {
            right_score.cmp(left_score).then_with(|| left_index.cmp(right_index))
        });
        self.visible_entries = matches.into_iter().map(|(index, _)| index).collect();
    }

    pub fn reconcile_selection(&mut self) -> ReconcileSelection {
        // With nothing selected there is nothing to drop or promote, and no
        // filter that would pick a first match. This is every batch of a
        // load the user has not clicked into yet, so it must not pay for a
        // set over the whole listing. (A pending reveal is not the
        // selection's business: `take_pending_visible_entries` selects it.)
        if self.selection.selected().is_empty() && self.filter_query.is_empty() {
            return ReconcileSelection::ClearPreview;
        }
        let previous_primary = self.selection.primary().cloned();
        let Self { entries, visible_entries, selection, .. } = self;
        let visible_paths = || {
            visible_entries
                .iter()
                .filter_map(|index| entries.get(*index))
                .map(|entry| entry.path.as_path())
        };
        let visible = visible_paths().collect::<HashSet<_>>();
        selection.retain(visible_paths(), |path| visible.contains(path));
        if let Some(primary) = self.selection.primary().cloned() {
            // `retain` silently promotes a surviving selected item to primary
            // when the old primary left the visible set. The preview must
            // follow, or the pane keeps showing the vanished item while the
            // footer names the new one.
            if previous_primary.as_ref() != Some(&primary)
                && let Some(entry) =
                    self.entries.iter().find(|entry| entry.path == primary).cloned()
            {
                return ReconcileSelection::Preview(entry);
            }
            return ReconcileSelection::Unchanged;
        }

        let selected_entry = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .find(|entry| self.selection.is_selected(&entry.path))
            .cloned();
        let entry = selected_entry.clone().or_else(|| {
            (!self.filter_query.is_empty()).then(|| self.visible_entry(0).cloned()).flatten()
        });
        if let Some(entry) = entry {
            if selected_entry.is_some() {
                self.selection.make_primary(&entry.path);
            } else {
                self.selection.select_only(entry.path.clone());
            }
            ReconcileSelection::Preview(entry)
        } else {
            self.selection.clear();
            ReconcileSelection::ClearPreview
        }
    }
}

impl Drop for DirectorySession {
    /// A closed window's walker would otherwise stat on until its next batch
    /// failed to send.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub enum ReconcileSelection {
    Unchanged,
    Preview(FileEntry),
    ClearPreview,
}

pub enum ApplyDirectoryEvents {
    Applied(ReconcileSelection),
    RescanRequired,
}

pub fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    let candidate = candidate.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.to_lowercase().chars().collect::<Vec<_>>();
    fuzzy_score_folded(&candidate, &query)
}

fn is_hidden_os_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes = name.as_bytes();
    bytes.first() == Some(&b'.') && bytes != b"." && bytes != b".."
}

fn fuzzy_score_folded(candidate: &[char], query: &[char]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut search_from = 0;
    let mut previous_match = None;
    let mut score = 0i64;

    for needle in query {
        let relative =
            candidate.get(search_from..)?.iter().position(|character| character == needle)?;
        let position = search_from + relative;

        score +=
            if previous_match.is_some_and(|previous| position == previous + 1) { 24 } else { 4 };
        if position == 0
            || candidate
                .get(position.saturating_sub(1))
                .is_some_and(|character| matches!(character, ' ' | '-' | '_' | '.'))
        {
            score += 16;
        }
        score -= position as i64;

        previous_match = Some(position);
        search_from = position + 1;
    }

    score -= candidate.len().saturating_sub(query.len()) as i64;
    Some(score)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryEvent {
    Added(FileEntry),
    Removed(PathBuf),
    Changed(FileEntry),
    Renamed { from: PathBuf, entry: FileEntry },
    RescanRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::entries::EntryKind;
    use std::path::Path;

    fn entry(name: &str, navigable: bool, size: Option<u64>) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/folder").join(name),
            name: name.to_string(),
            name_os: name.into(),
            folded_name: name.to_lowercase().chars().collect(),
            kind: if navigable { EntryKind::Directory } else { EntryKind::File },
            navigable,
            size,
            modified: None,
            icon_path: None,
        }
    }

    fn file(name: &str) -> FileEntry {
        entry(name, false, Some(1))
    }

    fn dir(name: &str) -> FileEntry {
        entry(name, true, None)
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from("/folder").join(name)
    }

    /// A finished load holding `entries`, sorted and projected.
    fn session(entries: Vec<FileEntry>) -> DirectorySession {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.merge_batch(entries);
        session
    }

    fn names(session: &DirectorySession) -> Vec<(&str, Option<u64>)> {
        session.entries.iter().map(|entry| (entry.name.as_str(), entry.size)).collect()
    }

    fn visible_names(session: &DirectorySession) -> Vec<&str> {
        session.visible_entries.iter().map(|index| session.entries[*index].name.as_str()).collect()
    }

    fn previews(result: &ReconcileSelection, name: &str) -> bool {
        matches!(result, ReconcileSelection::Preview(entry) if entry.name == name)
    }

    /// Hidden entries are shown by default, so without filtering, replacing a
    /// file would make a cryptic sibling appear beside it and vanish later.
    /// Permanent-delete quarantines stay visible because their recovery
    /// guidance tells the user to go and look at them.
    #[test]
    fn marcel_working_files_stay_out_of_the_browser() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.show_hidden = true;
        session.merge_batch(vec![
            file("report.txt"),
            file(&format!(".marcel-replaced-{}-1-0-report.txt", crate::fsops::boot_id())),
            dir(".marcel-copy-1-0-staging"),
            dir(".marcel-archive-abc"),
            dir(".marcel-delete-1-0-old"),
            dir(".config"),
        ]);

        assert_eq!(visible_names(&session), [".config", ".marcel-delete-1-0-old", "report.txt"]);
    }

    #[test]
    fn events_keep_directory_first_sorting_and_replace_stale_names() {
        let mut session = session(vec![file("b"), file("old")]);

        session.apply_events(vec![
            DirectoryEvent::Added(dir("z")),
            DirectoryEvent::Changed(entry("b", false, Some(9))),
            DirectoryEvent::Renamed { from: path("old"), entry: entry("new", false, Some(3)) },
        ]);
        assert_eq!(names(&session), [("z", None), ("b", Some(9)), ("new", Some(3))]);

        session.apply_events(vec![DirectoryEvent::Removed(path("z"))]);
        assert_eq!(names(&session), [("b", Some(9)), ("new", Some(3))]);
        assert!(matches!(
            session.apply_events(vec![DirectoryEvent::RescanRequired]),
            ApplyDirectoryEvents::RescanRequired
        ));
    }

    #[test]
    fn filtering_reconciles_selection_and_prefers_first_match() {
        let mut session = session(vec![file("alpha.txt"), file("beta.txt")]);
        session.selection.select_only(path("alpha.txt"));

        let result = session.set_filter_query("bet".to_string()).unwrap();

        assert!(previews(&result, "beta.txt"));
        assert_eq!(
            session.selection.primary().map(PathBuf::as_path),
            Some(Path::new("/folder/beta.txt")),
        );
    }

    /// A picker's filter narrows what type-to-filter searches, keeps folders
    /// navigable, and drops a selection it no longer shows.
    #[test]
    fn a_content_filter_projects_under_the_fuzzy_filter_and_reconciles() {
        let mut session = session(vec![dir("assets"), file("photo.png"), file("photo.txt")]);
        session.selection.select_only(path("photo.txt"));

        let result = session.set_content_filter(Some(Arc::new(|entry: &FileEntry| {
            entry.navigable || entry.name.ends_with(".png")
        })));
        assert!(matches!(result, ReconcileSelection::ClearPreview));
        assert_eq!(visible_names(&session), ["assets", "photo.png"]);

        session.set_filter_query("photo".to_string());
        assert_eq!(session.visible_paths(), vec![path("photo.png")]);

        session.set_content_filter(None);
        assert_eq!(session.visible_paths().len(), 2);
    }

    #[test]
    fn a_batch_merged_after_a_reveal_moves_the_revealed_row() {
        // Why the scroll has to be re-applied at `Done` rather than once, when
        // the entry first appears: the row it is on while the enumeration is
        // still streaming is not the row it ends up on. `stream_directory`
        // yields in readdir order, so a later batch merges names that sort
        // *before* the revealed one and push it down.
        let mut session = session(vec![file("m.txt"), file("target.txt")]);
        let target = path("target.txt");
        assert_eq!(session.visible_position(&target), Some(1));

        session.merge_batch(vec![file("a.txt"), file("b.txt")]);

        assert_eq!(session.visible_position(&target), Some(3));
    }

    #[test]
    fn a_mid_stream_reveal_holds_its_scroll_target_until_taken() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.defer_reveal_scroll(path("target.txt"));

        // Finishing the load must not drop it: the whole point is that it
        // outlives the stream so the row can be corrected afterwards.
        session.finish_load();

        assert_eq!(session.take_reveal_scroll_target(), Some(path("target.txt")));
        assert_eq!(session.take_reveal_scroll_target(), None);
    }

    /// A scroll target belongs to one reveal: a new load or a replacement
    /// reveal drops it.
    #[test]
    fn a_new_load_or_reveal_drops_a_stale_scroll_target() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.defer_reveal_scroll(path("target.txt"));
        session.begin_load(LoadKind::Navigate);
        assert_eq!(session.take_reveal_scroll_target(), None);

        session.defer_reveal_scroll(path("old.txt"));
        session.replace_pending_reveal(vec![path("new.txt")]);
        assert_eq!(session.take_reveal_scroll_target(), None);
    }

    #[test]
    fn pending_reveal_waits_until_the_entry_is_visible() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.pending_reveal = vec![path("later.txt")];
        assert!(session.take_pending_visible_entries().is_empty());

        session.merge_batch(vec![file("later.txt")]);
        let revealed = session.take_pending_visible_entries();

        assert_eq!(revealed[0].name, "later.txt");
        assert!(session.pending_reveal.is_empty());
        assert_eq!(
            session.selection.primary().map(PathBuf::as_path),
            Some(Path::new("/folder/later.txt")),
        );
    }

    #[test]
    fn pending_reveal_selects_every_item_across_incremental_batches() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.pending_reveal = vec![path("first.txt"), path("second.txt")];

        session.merge_batch(vec![file("second.txt")]);
        assert_eq!(session.take_pending_visible_entries().len(), 1);

        session.merge_batch(vec![file("first.txt")]);
        assert_eq!(session.take_pending_visible_entries().len(), 1);

        assert!(session.pending_reveal.is_empty());
        assert_eq!(session.selection.selected().len(), 2);
        assert!(session.selection.primary().is_some());
    }

    #[test]
    fn pending_reveal_can_replace_an_existing_selection_before_streaming() {
        let mut session = session(vec![file("old.txt"), file("first.txt"), file("second.txt")]);
        session.selection.select_only(path("old.txt"));

        session.replace_pending_reveal(vec![path("first.txt"), path("second.txt")]);
        session.take_pending_visible_entries();

        assert_eq!(session.selection.selected().len(), 2);
        assert!(!session.selection.is_selected(Path::new("/folder/old.txt")));
    }

    #[test]
    fn event_batches_reconcile_once_and_refresh_changed_primary_entries() {
        let mut session = session(vec![file("selected.txt"), file("removed.txt")]);
        session.selection.select_only(path("selected.txt"));

        let result = session.apply_events(vec![
            DirectoryEvent::Removed(path("removed.txt")),
            DirectoryEvent::Changed(entry("selected.txt", false, Some(9))),
            DirectoryEvent::Added(dir("folder")),
        ]);

        assert!(matches!(
            result,
            ApplyDirectoryEvents::Applied(ReconcileSelection::Preview(FileEntry {
                ref name,
                size: Some(9),
                ..
            })) if name == "selected.txt"
        ));
        assert_eq!(names(&session), [("folder", None), ("selected.txt", Some(9))]);
    }

    /// The lookup is rebuilt lazily against a revision, so every path that
    /// mutates `entries` must invalidate it. A stale index resolves a path to
    /// whatever entry now occupies that position.
    #[test]
    fn entry_lookup_tracks_every_entries_mutation() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        let a = path("a.txt");
        assert!(session.entry(&a).is_none());

        session.merge_batch(vec![file("a.txt")]);
        assert_eq!(session.entry(&a).map(|e| e.size), Some(Some(1)));

        // An inserted directory sorts ahead of the file and shifts its index.
        session.apply_events(vec![DirectoryEvent::Added(dir("folder"))]);
        assert!(session.entry(&path("folder")).is_some());
        assert_eq!(session.entry(&a).map(|e| e.name.as_str()), Some("a.txt"));

        // A change in place must be observed, not served from the old copy.
        session.apply_events(vec![DirectoryEvent::Changed(entry("a.txt", false, Some(99)))]);
        assert_eq!(session.entry(&a).map(|e| e.size), Some(Some(99)));

        session.apply_events(vec![DirectoryEvent::Removed(a.clone())]);
        assert!(session.entry(&a).is_none());

        session.apply_events(vec![DirectoryEvent::Added(file("b.txt"))]);
        assert!(session.entry(&path("b.txt")).is_some());

        session.begin_virtual_load(LoadKind::Navigate);
        assert!(session.entry(&path("b.txt")).is_none());
    }

    #[test]
    fn fuzzy_filter_is_case_insensitive_ordered_and_prefers_contiguous_early_matches() {
        assert!(fuzzy_score("Cargo.lock", "cgl").is_some());
        assert!(fuzzy_score("Documents", "DOC").is_some());
        assert!(fuzzy_score("Documents", "stm").is_none());
        assert!(
            fuzzy_score("document-backup", "doc").unwrap()
                > fuzzy_score("downloaded-object-copy", "doc").unwrap()
        );
        assert!(fuzzy_score("photo", "pho").unwrap() > fuzzy_score("my-photo", "pho").unwrap());
    }

    #[test]
    fn hidden_names_follow_unix_dotfile_conventions() {
        assert!(is_hidden_name(".git"));
        assert!(is_hidden_name(".env.local"));
        assert!(!is_hidden_name("visible.txt"));
        assert!(!is_hidden_name("."));
        assert!(!is_hidden_name(".."));
    }

    /// The row a view scrolls to is the entry's place in the *visible*
    /// projection, not in the full listing.
    #[test]
    fn visible_position_follows_the_projection() {
        let mut session = session(vec![file("alpha.txt"), file("beta.txt"), file("gamma.txt")]);
        assert_eq!(session.visible_position(&path("gamma.txt")), Some(2));

        session.set_filter_query("gam".to_string());
        assert_eq!(session.visible_position(&path("gamma.txt")), Some(0));
        assert_eq!(session.visible_position(&path("alpha.txt")), None);
    }

    /// When the primary leaves the visible set while other selected items
    /// stay, `retain` promotes a survivor to primary. The preview must follow
    /// that promotion; keeping it "unchanged" left the pane showing a vanished
    /// file while the footer named the new primary.
    #[test]
    fn a_promoted_primary_refreshes_the_preview() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.show_hidden = true;
        session.merge_batch(vec![file(".bashrc"), entry("notes.txt", false, Some(2))]);
        session.selection.add_all([path("notes.txt")]);
        session.selection.add_all([path(".bashrc")]);
        session.selection.make_primary(Path::new("/folder/.bashrc"));

        let result = session.set_show_hidden(false).unwrap();

        assert!(previews(&result, "notes.txt"), "the promoted primary must be previewed");
    }

    /// Deferred work belongs to one load: a new load starts from a clean
    /// slate, and the finished load hands back exactly what accumulated.
    #[test]
    fn deferred_refresh_and_rescan_are_scoped_to_one_load() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.defer_refresh([path("changed.txt")]);
        session.defer_rescan();

        assert!(session.take_pending_rescan());
        assert!(!session.take_pending_rescan(), "taking consumes the flag");
        assert_eq!(session.take_pending_refresh(), vec![path("changed.txt")]);
        assert!(session.take_pending_refresh().is_empty());

        session.defer_refresh([path("stale.txt")]);
        session.defer_rescan();
        session.begin_virtual_load(LoadKind::Navigate);
        assert!(!session.take_pending_rescan());
        assert!(session.take_pending_refresh().is_empty());
    }

    #[test]
    fn projection_revision_advances_when_the_visible_set_changes() {
        let mut session = session(vec![file("alpha.txt"), file("beta.txt")]);
        let before = session.projection_revision();

        session.set_filter_query("bet".to_string());

        assert_ne!(session.projection_revision(), before);
    }

    /// The walker only notices a dropped receiver at its next batch, so a
    /// superseded load must be told to stop through the flag it was given.
    #[test]
    fn superseding_a_load_cancels_its_background_work() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.begin_load(LoadKind::Navigate);
        let first = session.cancellation();
        assert!(!first.load(Ordering::Acquire));

        session.begin_load(LoadKind::Navigate);
        let second = session.cancellation();
        assert!(first.load(Ordering::Acquire), "the superseded load must stop");
        assert!(!second.load(Ordering::Acquire), "the new load must not");

        drop(session);
        assert!(second.load(Ordering::Acquire), "a closed window stops its walker");
    }

    /// Nothing selected and no filter means every batch of a fresh load can
    /// skip the reconcile entirely; the selection revision proves it did.
    #[test]
    fn reconcile_is_skipped_while_nothing_is_selected() {
        let mut plain = DirectorySession::new(PathBuf::from("/folder"));
        let before = plain.selection.revision();
        plain.merge_batch(vec![file("a.txt"), file("b.txt")]);
        assert_eq!(plain.selection.revision(), before, "no selection to touch");

        plain.selection.select_only(path("a.txt"));
        let before = plain.selection.revision();
        plain.merge_batch(vec![file("c.txt")]);
        assert_ne!(plain.selection.revision(), before, "a selection is reconciled");
        assert!(plain.selection.is_selected(Path::new("/folder/a.txt")));

        // A filter picks a first match even from nothing, so it cannot skip.
        let mut filtered = session(vec![file("alpha.txt")]);
        filtered.filter_query = "alp".to_string();
        assert!(previews(&filtered.merge_batch(vec![file("beta.txt")]), "alpha.txt"));
    }

    /// A refresh keeps what is on screen — entries and selection — until the
    /// new listing is complete, then swaps it in and drops only what is gone.
    #[test]
    fn a_refresh_keeps_the_listing_and_selection_until_the_stream_finishes() {
        let mut session = session(vec![file("kept.txt"), file("gone.txt"), file("same.txt")]);
        session.selection.add_all([path("kept.txt"), path("gone.txt")]);
        session.selection.make_primary(Path::new("/folder/gone.txt"));

        session.begin_load(LoadKind::Refresh);
        assert!(session.loading);
        assert_eq!(visible_names(&session), ["gone.txt", "kept.txt", "same.txt"]);
        assert_eq!(session.selection.selected().len(), 2);

        assert!(matches!(
            session.merge_batch(vec![entry("kept.txt", false, Some(7)), file("same.txt")]),
            ReconcileSelection::Unchanged
        ));
        assert!(matches!(
            session.merge_batch(vec![file("new.txt")]),
            ReconcileSelection::Unchanged
        ));
        assert_eq!(visible_names(&session), ["gone.txt", "kept.txt", "same.txt"], "not yet");

        let finished = session.finish_load();
        assert!(!session.loading);
        assert_eq!(visible_names(&session), ["kept.txt", "new.txt", "same.txt"]);
        assert!(previews(&finished.reconcile, "kept.txt"), "the survivor is promoted");
        assert_eq!(session.selection.selected().len(), 1);
        let mut changed = finished.changed;
        changed.sort();
        assert_eq!(changed, [path("gone.txt"), path("kept.txt"), path("new.txt")]);
        assert_eq!(session.entry(&path("kept.txt")).unwrap().size, Some(7));
    }

    /// A refresh that fails has nothing to show but the error, so the listing
    /// it was keeping goes; a fresh load that fails was empty already.
    #[test]
    fn a_failed_refresh_drops_the_stale_listing() {
        let mut session = session(vec![file("a.txt")]);
        session.selection.select_only(path("a.txt"));
        session.begin_load(LoadKind::Refresh);

        let reconcile = session.fail_load("gone".to_string());

        assert!(matches!(reconcile, ReconcileSelection::ClearPreview));
        assert!(session.entries.is_empty());
        assert!(session.selection.selected().is_empty());
        assert_eq!(session.error.as_deref(), Some("gone"));
    }

    /// A navigation clears everything the old folder owned; a refresh keeps
    /// the filter, since the user is still looking at the same folder.
    #[test]
    fn a_navigation_clears_what_a_refresh_keeps() {
        let mut session = session(vec![file("a.txt")]);
        session.filter_query = "a".to_string();
        session.begin_load(LoadKind::Refresh);
        assert_eq!(session.filter_query, "a");
        session.finish_load();

        session.begin_load(LoadKind::Navigate);
        assert!(session.filter_query.is_empty());
        assert!(session.entries.is_empty());
    }

    /// The first rescan of a folder starts at once; another inside the
    /// backoff waits out the remainder and absorbs any further requests.
    #[test]
    fn rescans_of_one_folder_are_spaced_by_the_backoff() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        let start = Instant::now();
        assert_eq!(session.schedule_rescan(start), RescanDecision::Now);

        let soon = start + Duration::from_millis(500);
        assert_eq!(
            session.schedule_rescan(soon),
            RescanDecision::After(RESCAN_BACKOFF - Duration::from_millis(500))
        );
        assert_eq!(session.schedule_rescan(soon), RescanDecision::AlreadyScheduled);

        session.begin_scheduled_rescan(start + RESCAN_BACKOFF);
        assert_eq!(
            session.schedule_rescan(start + RESCAN_BACKOFF + Duration::from_millis(1)),
            RescanDecision::After(RESCAN_BACKOFF - Duration::from_millis(1))
        );

        // Well past the backoff the next one is immediate again.
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        assert_eq!(session.schedule_rescan(start), RescanDecision::Now);
        assert_eq!(session.schedule_rescan(start + RESCAN_BACKOFF), RescanDecision::Now);
    }

    /// The backoff is per folder: leaving and arriving somewhere else starts
    /// fresh, and a new load forgets a rescan that was waiting.
    #[test]
    fn the_rescan_backoff_belongs_to_one_folder() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        let start = Instant::now();
        assert_eq!(session.schedule_rescan(start), RescanDecision::Now);
        assert!(matches!(session.schedule_rescan(start), RescanDecision::After(_)));

        session.current_dir = PathBuf::from("/elsewhere");
        session.begin_load(LoadKind::Navigate);
        assert_eq!(session.schedule_rescan(start), RescanDecision::Now);
    }

    #[test]
    fn large_directory_filter_reuses_entry_folded_names() {
        let mut session =
            session((0..50_000).map(|index| file(&format!("item-{index:05}.txt"))).collect());

        let reconcile = session.set_filter_query("ITEM-49999".to_string());

        assert!(reconcile.is_some());
        assert_eq!(session.visible_entries.len(), 1);
        assert_eq!(session.visible_entry(0).unwrap().name, "item-49999.txt");
    }
}
