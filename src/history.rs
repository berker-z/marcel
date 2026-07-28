use std::path::{Path, PathBuf};

/// Navigation history adapted from Yazi's MIT-licensed backstack:
/// https://github.com/sxyazi/yazi/blob/main/yazi-core/src/tab/backstack.rs
#[derive(Debug, Default)]
pub struct NavigationHistory {
    cursor: usize,
    stack: Vec<PathBuf>,
}

impl NavigationHistory {
    pub fn new(path: PathBuf) -> Self {
        Self {
            cursor: 0,
            stack: vec![path],
        }
    }

    pub fn push(&mut self, path: &Path) {
        if self.current().is_some_and(|current| current == path) {
            return;
        }

        self.cursor += 1;
        if self.cursor == self.stack.len() {
            self.stack.push(path.to_path_buf());
        } else {
            self.stack[self.cursor] = path.to_path_buf();
            self.stack.truncate(self.cursor + 1);
        }

        if self.stack.len() > 60 {
            let start = self.cursor.saturating_sub(30);
            self.stack.drain(..start);
            self.cursor -= start;
        }
    }

    pub fn current(&self) -> Option<&Path> {
        self.stack.get(self.cursor).map(PathBuf::as_path)
    }

    pub fn can_go_back(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.cursor + 1 < self.stack.len()
    }

    pub fn go_back(&mut self) -> Option<PathBuf> {
        if !self.can_go_back() {
            return None;
        }
        self.cursor -= 1;
        self.stack.get(self.cursor).cloned()
    }

    pub fn go_forward(&mut self) -> Option<PathBuf> {
        if !self.can_go_forward() {
            return None;
        }
        self.cursor += 1;
        self.stack.get(self.cursor).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_forward_history_after_new_navigation() {
        let mut history = NavigationHistory::new("/one".into());
        history.push(Path::new("/two"));
        history.push(Path::new("/three"));

        assert_eq!(history.go_back(), Some("/two".into()));
        history.push(Path::new("/four"));

        assert!(!history.can_go_forward());
        assert_eq!(history.go_back(), Some("/two".into()));
        assert_eq!(history.go_back(), Some("/one".into()));
        assert_eq!(history.go_back(), None);
    }
}
