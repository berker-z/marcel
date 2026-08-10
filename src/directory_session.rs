use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::Task;

use crate::fs::{FileEntry, merge_sorted_entries, sort_entries};
use crate::selection::SelectionModel;

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
    pub(crate) selection: SelectionModel,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) warning: Option<String>,
    pub(crate) generation: u64,
    pub(crate) load_task: Option<Task<()>>,
    pub(crate) watch_task: Option<Task<()>>,
    watch_cancel: Option<Arc<AtomicBool>>,
    pub(crate) pending_reveal: Vec<PathBuf>,
    entries_revision: u64,
    projection_revision: u64,
    entry_index: RefCell<EntryIndex>,
}

impl DirectorySession {
    pub fn new(current_dir: PathBuf) -> Self {
        Self {
            current_dir,
            entries: Vec::new(),
            visible_entries: Vec::new(),
            filter_query: String::new(),
            show_hidden: true,
            selection: SelectionModel::default(),
            loading: false,
            error: None,
            warning: None,
            generation: 0,
            load_task: None,
            watch_task: None,
            watch_cancel: None,
            pending_reveal: Vec::new(),
            entries_revision: 1,
            projection_revision: 1,
            entry_index: RefCell::new(EntryIndex::default()),
        }
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

    pub fn apply_event(&mut self, event: DirectoryEvent) -> ApplyDirectoryEvent {
        let applied = apply_directory_event(&mut self.entries, event);
        self.mark_entries_changed();
        applied
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
        sort_entries(&mut upserts);
        self.entries = merge_sorted_entries(std::mem::take(&mut self.entries), upserts);
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

    pub fn begin_load(&mut self, clear_filter: bool) -> (u64, PathBuf) {
        let generation = self.begin_virtual_load(clear_filter);
        (generation, self.current_dir.clone())
    }

    pub fn begin_virtual_load(&mut self, clear_filter: bool) -> u64 {
        self.stop_watcher();
        self.generation = self.generation.wrapping_add(1);
        self.load_task.take();
        self.entries.clear();
        self.mark_entries_changed();
        self.visible_entries.clear();
        self.projection_revision = self.projection_revision.wrapping_add(1);
        if clear_filter {
            self.filter_query.clear();
        }
        self.error = None;
        self.warning = None;
        self.loading = true;
        self.generation
    }

    pub fn set_watcher(&mut self, cancel: Arc<AtomicBool>, task: Task<()>) {
        self.stop_watcher();
        self.watch_cancel = Some(cancel);
        self.watch_task = Some(task);
    }

    fn stop_watcher(&mut self) {
        if let Some(cancel) = self.watch_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.watch_task.take();
    }

    pub fn merge_batch(&mut self, batch: Vec<FileEntry>) -> ReconcileSelection {
        self.entries = merge_sorted_entries(std::mem::take(&mut self.entries), batch);
        self.mark_entries_changed();
        self.rebuild_visible_entries();
        self.reconcile_selection()
    }

    pub fn finish_load(&mut self) {
        self.loading = false;
        self.pending_reveal.clear();
    }

    pub fn fail_load(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
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
        self.selection
            .add_all(revealed.iter().map(|entry| entry.path.clone()));
        revealed
    }

    pub fn replace_pending_reveal(&mut self, paths: Vec<PathBuf>) {
        self.selection.clear();
        self.pending_reveal = paths;
    }

    pub fn visible_entry(&self, index: usize) -> Option<&FileEntry> {
        self.visible_entries
            .get(index)
            .and_then(|entry_index| self.entries.get(*entry_index))
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
                .filter_map(|(index, entry)| {
                    (!crate::file_ops::is_internal_working_name(&entry.name_os)
                        && (self.show_hidden || !is_hidden_os_name(&entry.name_os)))
                    .then_some(index)
                })
                .collect();
            return;
        }

        let folded_query = self.filter_query.to_lowercase().chars().collect::<Vec<_>>();
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if crate::file_ops::is_internal_working_name(&entry.name_os)
                    || (!self.show_hidden && is_hidden_os_name(&entry.name_os))
                {
                    return None;
                }
                fuzzy_score_folded(&entry.folded_name, &folded_query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        });
        self.visible_entries = matches.into_iter().map(|(index, _)| index).collect();
    }

    pub fn reconcile_selection(&mut self) -> ReconcileSelection {
        let visible = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.as_path())
            .collect::<HashSet<_>>();
        let ordered = self.visible_paths();
        self.selection
            .retain(&ordered, |path| visible.contains(path));
        if self.selection.primary().is_some() {
            return ReconcileSelection::Unchanged;
        }

        let selected_entry = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .find(|entry| self.selection.is_selected(&entry.path))
            .cloned();
        let entry = selected_entry.clone().or_else(|| {
            (!self.filter_query.is_empty())
                .then(|| self.visible_entry(0).cloned())
                .flatten()
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
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = name.as_bytes();
        bytes.first() == Some(&b'.') && bytes != b"." && bytes != b".."
    }
    #[cfg(not(unix))]
    {
        name.to_str().is_some_and(is_hidden_name)
    }
}

fn fuzzy_score_folded(candidate: &[char], query: &[char]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut search_from = 0;
    let mut previous_match = None;
    let mut score = 0i64;

    for needle in query {
        let relative = candidate
            .get(search_from..)?
            .iter()
            .position(|character| character == needle)?;
        let position = search_from + relative;

        score += if previous_match.is_some_and(|previous| position == previous + 1) {
            24
        } else {
            4
        };
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyDirectoryEvent {
    Applied,
    RescanRequired,
}

fn apply_directory_event(
    entries: &mut Vec<FileEntry>,
    event: DirectoryEvent,
) -> ApplyDirectoryEvent {
    match event {
        DirectoryEvent::Added(entry) | DirectoryEvent::Changed(entry) => {
            remove_path(entries, &entry.path);
            let current = std::mem::take(entries);
            *entries = merge_sorted_entries(current, vec![entry]);
        }
        DirectoryEvent::Removed(path) => remove_path(entries, &path),
        DirectoryEvent::Renamed { from, entry } => {
            remove_path(entries, &from);
            remove_path(entries, &entry.path);
            let current = std::mem::take(entries);
            *entries = merge_sorted_entries(current, vec![entry]);
        }
        DirectoryEvent::RescanRequired => return ApplyDirectoryEvent::RescanRequired,
    }
    ApplyDirectoryEvent::Applied
}

fn remove_path(entries: &mut Vec<FileEntry>, path: &PathBuf) {
    entries.retain(|entry| &entry.path != path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::EntryKind;
    use std::path::Path;

    fn entry(name: &str, navigable: bool, size: Option<u64>) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/folder").join(name),
            name: name.to_string(),
            name_os: name.into(),
            folded_name: name.to_lowercase().chars().collect(),
            kind: if navigable {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            navigable,
            size,
            icon_path: None,
        }
    }

    /// Hidden entries are shown by default, so without filtering, replacing a
    /// file would make a cryptic sibling appear beside it and vanish later.
    /// Permanent-delete quarantines stay visible because their recovery
    /// guidance tells the user to go and look at them.
    #[test]
    fn marcel_working_files_stay_out_of_the_browser() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.show_hidden = true;
        session.entries = vec![
            entry("report.txt", false, Some(1)),
            entry(".marcel-replaced-1-0-report.txt", false, Some(1)),
            entry(".marcel-copy-1-0-staging", true, None),
            entry(".marcel-archive-abc", true, None),
            entry(".marcel-delete-1-0-old", true, None),
            entry(".config", true, None),
        ];
        session.rebuild_visible_entries();

        let visible = session
            .visible_entries
            .iter()
            .map(|index| session.entries[*index].name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(visible, ["report.txt", ".marcel-delete-1-0-old", ".config"]);
    }

    #[test]
    fn additions_and_changes_preserve_directory_first_sorting() {
        let mut entries = vec![entry("b", false, Some(1))];
        apply_directory_event(&mut entries, DirectoryEvent::Added(entry("z", true, None)));
        apply_directory_event(
            &mut entries,
            DirectoryEvent::Changed(entry("b", false, Some(9))),
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.size))
                .collect::<Vec<_>>(),
            vec![("z", None), ("b", Some(9))]
        );
    }

    #[test]
    fn rename_replaces_stale_source_and_destination_names() {
        let mut entries = vec![entry("old", false, Some(1)), entry("new", false, Some(2))];
        apply_directory_event(
            &mut entries,
            DirectoryEvent::Renamed {
                from: PathBuf::from("/folder/old"),
                entry: entry("new", false, Some(3)),
            },
        );
        assert_eq!(entries, vec![entry("new", false, Some(3))]);
    }

    #[test]
    fn removal_and_rescan_are_explicit() {
        let mut entries = vec![entry("gone", false, Some(1))];
        assert_eq!(
            apply_directory_event(
                &mut entries,
                DirectoryEvent::Removed(PathBuf::from("/folder/gone"))
            ),
            ApplyDirectoryEvent::Applied
        );
        assert!(entries.is_empty());
        assert_eq!(
            apply_directory_event(&mut entries, DirectoryEvent::RescanRequired),
            ApplyDirectoryEvent::RescanRequired
        );
    }

    #[test]
    fn filtering_reconciles_selection_and_prefers_first_match() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.entries = vec![
            entry("alpha.txt", false, Some(1)),
            entry("beta.txt", false, Some(1)),
        ];
        session.rebuild_visible_entries();
        session
            .selection
            .select_only(PathBuf::from("/folder/alpha.txt"));

        let result = session.set_filter_query("bet".to_string());

        assert!(matches!(
            result,
            Some(ReconcileSelection::Preview(FileEntry { ref name, .. }))
                if name == "beta.txt"
        ));
        assert_eq!(
            session.selection.primary().map(PathBuf::as_path),
            Some(Path::new("/folder/beta.txt")),
        );
    }

    #[test]
    fn pending_reveal_waits_until_the_entry_is_visible() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.pending_reveal = vec![PathBuf::from("/folder/later.txt")];
        assert!(session.take_pending_visible_entries().is_empty());

        session.entries.push(entry("later.txt", false, Some(1)));
        session.rebuild_visible_entries();
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
        session.pending_reveal = vec![
            PathBuf::from("/folder/first.txt"),
            PathBuf::from("/folder/second.txt"),
        ];

        session.entries.push(entry("second.txt", false, Some(1)));
        session.rebuild_visible_entries();
        assert_eq!(session.take_pending_visible_entries().len(), 1);

        session.entries.push(entry("first.txt", false, Some(1)));
        sort_entries(&mut session.entries);
        session.rebuild_visible_entries();
        assert_eq!(session.take_pending_visible_entries().len(), 1);

        assert!(session.pending_reveal.is_empty());
        assert_eq!(session.selection.selected().len(), 2);
        assert!(session.selection.primary().is_some());
    }

    #[test]
    fn pending_reveal_can_replace_an_existing_selection_before_streaming() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.entries = vec![
            entry("old.txt", false, Some(1)),
            entry("first.txt", false, Some(1)),
            entry("second.txt", false, Some(1)),
        ];
        sort_entries(&mut session.entries);
        session.rebuild_visible_entries();
        session
            .selection
            .select_only(PathBuf::from("/folder/old.txt"));

        session.replace_pending_reveal(vec![
            PathBuf::from("/folder/first.txt"),
            PathBuf::from("/folder/second.txt"),
        ]);
        session.take_pending_visible_entries();

        assert_eq!(session.selection.selected().len(), 2);
        assert!(!session.selection.is_selected(Path::new("/folder/old.txt")));
    }

    #[test]
    fn event_batches_reconcile_once_and_refresh_changed_primary_entries() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.entries = vec![
            entry("selected.txt", false, Some(1)),
            entry("removed.txt", false, Some(1)),
        ];
        session.rebuild_visible_entries();
        session
            .selection
            .select_only(PathBuf::from("/folder/selected.txt"));

        let result = session.apply_events(vec![
            DirectoryEvent::Removed(PathBuf::from("/folder/removed.txt")),
            DirectoryEvent::Changed(entry("selected.txt", false, Some(9))),
            DirectoryEvent::Added(entry("folder", true, None)),
        ]);

        assert!(matches!(
            result,
            ApplyDirectoryEvents::Applied(ReconcileSelection::Preview(FileEntry {
                ref name,
                size: Some(9),
                ..
            })) if name == "selected.txt"
        ));
        assert_eq!(
            session
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["folder", "selected.txt"]
        );
    }

    /// The lookup is rebuilt lazily against a revision, so every path that
    /// mutates `entries` must invalidate it. A stale index resolves a path to
    /// whatever entry now occupies that position.
    #[test]
    fn entry_lookup_tracks_every_entries_mutation() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        assert!(session.entry(Path::new("/folder/a.txt")).is_none());

        session.merge_batch(vec![entry("a.txt", false, Some(1))]);
        assert_eq!(
            session.entry(Path::new("/folder/a.txt")).map(|e| e.size),
            Some(Some(1))
        );

        // An inserted directory sorts ahead of the file and shifts its index.
        session.apply_events(vec![DirectoryEvent::Added(entry("folder", true, None))]);
        assert!(session.entry(Path::new("/folder/folder")).is_some());
        assert_eq!(
            session
                .entry(Path::new("/folder/a.txt"))
                .map(|e| e.name.as_str()),
            Some("a.txt")
        );

        // A change in place must be observed, not served from the old copy.
        session.apply_events(vec![DirectoryEvent::Changed(entry(
            "a.txt",
            false,
            Some(99),
        ))]);
        assert_eq!(
            session.entry(Path::new("/folder/a.txt")).map(|e| e.size),
            Some(Some(99))
        );

        session.apply_events(vec![DirectoryEvent::Removed(PathBuf::from(
            "/folder/a.txt",
        ))]);
        assert!(session.entry(Path::new("/folder/a.txt")).is_none());

        session.apply_event(DirectoryEvent::Added(entry("b.txt", false, Some(2))));
        assert!(session.entry(Path::new("/folder/b.txt")).is_some());

        session.begin_virtual_load(true);
        assert!(session.entry(Path::new("/folder/b.txt")).is_none());
    }

    #[test]
    fn projection_revision_advances_when_the_visible_set_changes() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.merge_batch(vec![
            entry("alpha.txt", false, Some(1)),
            entry("beta.txt", false, Some(1)),
        ]);
        let before = session.projection_revision();

        session.set_filter_query("bet".to_string());

        assert_ne!(session.projection_revision(), before);
    }

    #[test]
    fn large_directory_filter_reuses_entry_folded_names() {
        let mut session = DirectorySession::new(PathBuf::from("/folder"));
        session.entries = (0..50_000)
            .map(|index| entry(&format!("item-{index:05}.txt"), false, Some(1)))
            .collect();
        sort_entries(&mut session.entries);
        session.rebuild_visible_entries();

        let reconcile = session.set_filter_query("ITEM-49999".to_string());

        assert!(reconcile.is_some());
        assert_eq!(session.visible_entries.len(), 1);
        assert_eq!(session.visible_entry(0).unwrap().name, "item-49999.txt");
    }
}
