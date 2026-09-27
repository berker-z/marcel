use super::location::Location;

/// Navigation history adapted from Yazi's MIT-licensed backstack:
/// https://github.com/sxyazi/yazi/blob/main/yazi-core/src/tab/backstack.rs
///
/// It holds locations rather than paths, so a visit to the Trash is a step
/// like any other and Back from it returns to the folder before.
#[derive(Debug, Default)]
pub struct NavigationHistory {
    cursor: usize,
    stack: Vec<Location>,
}

impl NavigationHistory {
    pub fn new(location: Location) -> Self {
        Self { cursor: 0, stack: vec![location] }
    }

    pub fn push(&mut self, location: &Location) {
        if self.current().is_some_and(|current| current == location) {
            return;
        }

        self.cursor += 1;
        if self.cursor == self.stack.len() {
            self.stack.push(location.clone());
        } else {
            self.stack[self.cursor] = location.clone();
            self.stack.truncate(self.cursor + 1);
        }

        if self.stack.len() > 60 {
            let start = self.cursor.saturating_sub(30);
            self.stack.drain(..start);
            self.cursor -= start;
        }
    }

    pub fn current(&self) -> Option<&Location> {
        self.stack.get(self.cursor)
    }

    pub fn can_go_back(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.cursor + 1 < self.stack.len()
    }

    pub fn go_back(&mut self) -> Option<Location> {
        if !self.can_go_back() {
            return None;
        }
        self.cursor -= 1;
        self.stack.get(self.cursor).cloned()
    }

    pub fn go_forward(&mut self) -> Option<Location> {
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
    use crate::browse::location::TrashScope;

    fn folder(path: &str) -> Location {
        Location::Folder(path.into())
    }

    #[test]
    fn truncates_forward_history_after_new_navigation() {
        let mut history = NavigationHistory::new(folder("/one"));
        history.push(&folder("/two"));
        history.push(&folder("/three"));

        assert_eq!(history.go_back(), Some(folder("/two")));
        history.push(&folder("/four"));

        assert!(!history.can_go_forward());
        assert_eq!(history.go_back(), Some(folder("/two")));
        assert_eq!(history.go_back(), Some(folder("/one")));
        assert_eq!(history.go_back(), None);
    }

    #[test]
    fn the_trash_is_a_step_back_and_forward_like_a_folder() {
        let trash = Location::Trash(TrashScope::Home);
        let mut history = NavigationHistory::new(folder("/home/me/Documents"));
        history.push(&trash);

        assert_eq!(history.go_back(), Some(folder("/home/me/Documents")));
        assert_eq!(history.go_forward(), Some(trash));
    }
}
