use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct SelectionModel {
    selected: HashSet<PathBuf>,
    anchor: Option<PathBuf>,
    primary: Option<PathBuf>,
}

impl SelectionModel {
    pub fn clear(&mut self) {
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
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.primary = Some(path);
    }

    pub fn toggle(&mut self, path: PathBuf) {
        if self.selected.remove(&path) {
            if self.primary.as_ref() == Some(&path) {
                self.primary = None;
            }
        } else {
            self.selected.insert(path.clone());
            self.primary = Some(path.clone());
        }
        self.anchor = Some(path);
    }

    pub fn select_range(&mut self, path: PathBuf, ordered: &[PathBuf], additive: bool) {
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

    pub fn replace_from_marquee(
        &mut self,
        base: &HashSet<PathBuf>,
        intersecting: impl IntoIterator<Item = PathBuf>,
        additive: bool,
    ) {
        self.selected.clear();
        if additive {
            self.selected.extend(base.iter().cloned());
        }
        self.selected.extend(intersecting);
        self.primary = None;
        self.anchor = None;
    }
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
        selection.toggle(path("b"));
        selection.select_only(path("c"));

        assert_eq!(selection.selected(), &HashSet::from([path("c")]));
        assert_eq!(selection.primary(), Some(&path("c")));
    }

    #[test]
    fn toggle_adds_and_removes_an_item() {
        let mut selection = SelectionModel::default();
        selection.toggle(path("a"));
        assert!(selection.is_selected(Path::new("a")));

        selection.toggle(path("a"));
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
}
