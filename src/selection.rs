use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct SelectionModel {
    selected: HashSet<PathBuf>,
    anchor: Option<PathBuf>,
    primary: Option<PathBuf>,
    revision: u64,
}

impl SelectionModel {
    /// Bumped by every mutator so per-frame derived state — notably the
    /// drag payload — can tell when it may be reused instead of rebuilt.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn clear(&mut self) {
        self.touch();
        self.selected.clear();
        self.anchor = None;
        self.primary = None;
    }

    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected.contains(path)
    }

    pub fn selected(&self) -> &HashSet<PathBuf> {
        &self.selected
    }

    pub fn primary(&self) -> Option<&PathBuf> {
        self.primary.as_ref()
    }

    pub fn select_only(&mut self, path: PathBuf) {
        self.touch();
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.primary = Some(path);
    }

    pub fn make_primary(&mut self, path: &Path) {
        self.touch();
        if self.selected.contains(path) {
            self.primary = Some(path.to_path_buf());
        }
    }

    pub fn toggle(&mut self, path: PathBuf, ordered: &[PathBuf]) {
        self.touch();
        if self.selected.remove(&path) {
            if self.primary.as_ref() == Some(&path) {
                self.primary = nearest_selected(&path, ordered, &self.selected);
            }
        } else {
            self.selected.insert(path.clone());
            self.primary = Some(path.clone());
        }
        self.anchor = self.primary.clone().or(Some(path));
    }

    pub fn select_range(&mut self, path: PathBuf, ordered: &[PathBuf], additive: bool) {
        self.touch();
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|anchor| ordered.iter().position(|candidate| candidate == anchor));
        let target = ordered.iter().position(|candidate| candidate == &path);

        let (Some(anchor), Some(target)) = (anchor, target) else {
            self.select_only(path);
            return;
        };

        if !additive {
            self.selected.clear();
        }
        let (start, end) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        self.selected.extend(ordered[start..=end].iter().cloned());
        self.primary = Some(path);
    }

    pub fn select_all(&mut self, ordered: &[PathBuf]) {
        self.touch();
        if ordered.is_empty() {
            self.clear();
            return;
        }

        let primary = self
            .primary
            .as_ref()
            .filter(|primary| ordered.contains(primary))
            .cloned()
            .unwrap_or_else(|| ordered[0].clone());
        self.selected = ordered.iter().cloned().collect();
        self.anchor = Some(primary.clone());
        self.primary = Some(primary);
    }

    pub fn retain(&mut self, ordered: &[PathBuf], mut predicate: impl FnMut(&Path) -> bool) {
        self.touch();
        self.selected.retain(|path| predicate(path));
        if self.primary.as_ref().is_some_and(|path| !predicate(path)) {
            self.primary = None;
        }
        if self.anchor.as_ref().is_some_and(|path| !predicate(path)) {
            self.anchor = None;
        }
        self.ensure_primary(ordered);
    }

    pub fn add_all(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.touch();
        for path in paths {
            self.primary.get_or_insert_with(|| path.clone());
            self.anchor.get_or_insert_with(|| path.clone());
            self.selected.insert(path);
        }
    }

    pub fn ensure_primary(&mut self, ordered: &[PathBuf]) {
        self.touch();
        if self.primary.is_none() && !self.selected.is_empty() {
            self.primary = ordered
                .iter()
                .find(|path| self.selected.contains(*path))
                .cloned();
            self.anchor = self.primary.clone();
        }
    }

    pub fn replace_from_marquee(
        &mut self,
        base: &HashSet<PathBuf>,
        intersecting: impl IntoIterator<Item = PathBuf>,
        additive: bool,
    ) {
        self.touch();
        self.selected.clear();
        if additive {
            self.selected.extend(base.iter().cloned());
        }
        self.selected.extend(intersecting);
        self.primary = None;
        self.anchor = None;
    }
}

fn nearest_selected(
    removed: &Path,
    ordered: &[PathBuf],
    selected: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let removed_index = ordered.iter().position(|path| path == removed)?;
    ordered[removed_index..]
        .iter()
        .chain(ordered[..removed_index].iter().rev())
        .find(|path| selected.contains(*path))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn plain_selection_replaces_existing_items() {
        let mut selection = SelectionModel::default();
        selection.select_only(path("a"));
        selection.toggle(path("b"), &[path("a"), path("b")]);
        selection.select_only(path("c"));

        assert_eq!(selection.selected(), &HashSet::from([path("c")]));
        assert_eq!(selection.primary(), Some(&path("c")));
    }

    #[test]
    fn toggle_adds_and_removes_an_item() {
        let mut selection = SelectionModel::default();
        selection.toggle(path("a"), &[path("a")]);
        assert!(selection.is_selected(Path::new("a")));

        selection.toggle(path("a"), &[path("a")]);
        assert!(!selection.is_selected(Path::new("a")));
        assert_eq!(selection.primary(), None);
    }

    #[test]
    fn range_selection_uses_visible_order_in_both_directions() {
        let ordered = [path("a"), path("b"), path("c"), path("d")];
        let mut selection = SelectionModel::default();
        selection.select_only(path("d"));
        selection.select_range(path("b"), &ordered, false);

        assert_eq!(
            selection.selected(),
            &HashSet::from([path("b"), path("c"), path("d")])
        );
        assert_eq!(selection.primary(), Some(&path("b")));
    }

    #[test]
    fn additive_marquee_preserves_selection_snapshot() {
        let base = HashSet::from([path("a")]);
        let mut selection = SelectionModel::default();
        selection.replace_from_marquee(&base, [path("c"), path("d")], true);

        assert_eq!(
            selection.selected(),
            &HashSet::from([path("a"), path("c"), path("d")])
        );
    }

    #[test]
    fn select_all_preserves_an_existing_primary_item() {
        let ordered = [path("a"), path("b"), path("c")];
        let mut selection = SelectionModel::default();
        selection.select_only(path("b"));
        selection.select_all(&ordered);

        assert_eq!(selection.selected(), &HashSet::from(ordered));
        assert_eq!(selection.primary(), Some(&path("b")));
    }

    #[test]
    fn making_a_selected_item_primary_preserves_the_selection() {
        let mut selection = SelectionModel::default();
        selection.select_only(path("a"));
        selection.toggle(path("b"), &[path("a"), path("b")]);

        selection.make_primary(Path::new("a"));

        assert_eq!(selection.selected(), &HashSet::from([path("a"), path("b")]));
        assert_eq!(selection.primary(), Some(&path("a")));
    }

    #[test]
    fn retaining_visible_paths_removes_hidden_selection_state() {
        let mut selection = SelectionModel::default();
        selection.select_only(path("hidden"));
        selection.toggle(path("visible"), &[path("hidden"), path("visible")]);
        selection.make_primary(Path::new("hidden"));

        selection.retain(&[path("visible")], |candidate| {
            candidate == Path::new("visible")
        });

        assert_eq!(selection.selected(), &HashSet::from([path("visible")]));
        assert_eq!(selection.primary(), Some(&path("visible")));
    }

    /// The cached drag payload keys off this revision, so a mutator that
    /// forgets to bump it would hand a drag the previous selection.
    #[test]
    fn every_mutation_advances_the_revision() {
        let ordered = [path("a"), path("b")];
        let mut selection = SelectionModel::default();
        let mut previous = selection.revision();
        let mut assert_advanced = |selection: &SelectionModel, label: &str| {
            assert_ne!(selection.revision(), previous, "{label} kept the revision");
            previous = selection.revision();
        };

        selection.select_only(path("a"));
        assert_advanced(&selection, "select_only");
        selection.toggle(path("b"), &ordered);
        assert_advanced(&selection, "toggle");
        selection.select_range(path("a"), &ordered, false);
        assert_advanced(&selection, "select_range");
        selection.select_all(&ordered);
        assert_advanced(&selection, "select_all");
        selection.make_primary(Path::new("b"));
        assert_advanced(&selection, "make_primary");
        selection.add_all([path("a")]);
        assert_advanced(&selection, "add_all");
        selection.retain(&ordered, |_| true);
        assert_advanced(&selection, "retain");
        selection.ensure_primary(&ordered);
        assert_advanced(&selection, "ensure_primary");
        selection.replace_from_marquee(&HashSet::new(), [path("a")], false);
        assert_advanced(&selection, "replace_from_marquee");
        selection.clear();
        assert_advanced(&selection, "clear");
    }

    #[test]
    fn removing_the_primary_promotes_the_nearest_selected_item() {
        let ordered = [path("a"), path("b"), path("c")];
        let mut selection = SelectionModel::default();
        selection.select_only(path("a"));
        selection.toggle(path("b"), &ordered);
        selection.toggle(path("c"), &ordered);

        selection.toggle(path("b"), &ordered);

        assert_eq!(selection.primary(), Some(&path("c")));
        assert_eq!(selection.selected(), &HashSet::from([path("a"), path("c")]));
    }
}
