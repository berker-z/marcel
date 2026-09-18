//! The user's bookmarks, and the application's single writer for them.
//!
//! The file is published atomically, but that only makes each write
//! indivisible; it does not make two writers agree. While every window kept its
//! own list and its own save task, a window that had not seen the other's
//! addition would write its stale list over the top, and the lost bookmark left
//! no parse error behind to notice. Browser view state can tolerate
//! last-writer-wins; user data cannot.

use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Task};
use url::Url;

use crate::{
    config,
    names::display_path_name,
    surface::{self, Report},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub path: PathBuf,
}

impl Bookmark {
    pub fn label(&self) -> String {
        display_path_name(&self.path)
    }
}

/// What one read of the bookmark file produced.
///
/// `rejected` counts lines Marcel could not turn into a bookmark. They matter
/// because Marcel never writes such lines itself: a nonzero count means the
/// file holds something Marcel does not understand, and saving over it would
/// silently destroy whatever that was.
pub struct LoadedBookmarks {
    pub bookmarks: Vec<Bookmark>,
    pub rejected: usize,
}

pub fn load(path: &Path) -> Result<LoadedBookmarks> {
    let Some(contents) = config::read_own_file(path).context("Could not read bookmarks")? else {
        return Ok(LoadedBookmarks { bookmarks: Vec::new(), rejected: 0 });
    };

    let mut seen = HashSet::new();
    let mut rejected = 0;
    let bookmarks = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let path = Url::parse(line.trim())
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .filter(|path| path.is_absolute());
            let Some(path) = path else {
                rejected += 1;
                return None;
            };
            // A duplicate of a bookmark Marcel already has is not user data at
            // risk; collapsing it loses nothing.
            seen.insert(path.clone()).then_some(Bookmark { path })
        })
        .collect();
    Ok(LoadedBookmarks { bookmarks, rejected })
}

pub fn save(path: &Path, bookmarks: &[Bookmark]) -> Result<()> {
    config::write_atomically(path, |file| {
        for bookmark in bookmarks {
            if !bookmark.path.is_absolute() {
                bail!("Cannot save relative bookmark “{}”", bookmark.path.display());
            }
            let url = Url::from_file_path(&bookmark.path)
                .map_err(|_| anyhow::anyhow!("Invalid bookmark “{}”", bookmark.path.display()))?;
            writeln!(file, "{url}")?;
        }
        Ok(())
    })
}

/// Move the bookmark at `from` to the slot before `insertion`, saying whether
/// the list changed.
pub fn reorder(bookmarks: &mut Vec<Bookmark>, from: usize, insertion: usize) -> bool {
    if from >= bookmarks.len() || insertion > bookmarks.len() {
        return false;
    }
    let adjusted = if from < insertion { insertion - 1 } else { insertion };
    if adjusted == from {
        return false;
    }
    let bookmark = bookmarks.remove(from);
    bookmarks.insert(adjusted, bookmark);
    true
}

struct GlobalBookmarks(Entity<BookmarkStore>);

impl Global for GlobalBookmarks {}

/// The application's bookmark store, created and loaded on first use.
pub fn global(home: &Path, cx: &mut App) -> Entity<BookmarkStore> {
    if let Some(existing) = cx.try_global::<GlobalBookmarks>() {
        return existing.0.clone();
    }
    let store = cx.new(|cx| BookmarkStore::load(config::path(home, "bookmarks"), cx));
    cx.set_global(GlobalBookmarks(store.clone()));
    store
}

/// One list, one writer, however many windows are showing it.
pub struct BookmarkStore {
    path: PathBuf,
    bookmarks: Vec<Bookmark>,
    icons: HashMap<PathBuf, PathBuf>,
    loading: bool,
    /// Why the store must not be modified, when it must not be.
    ///
    /// A load that failed — or that found lines Marcel cannot represent —
    /// leaves an in-memory list that does not match the file, and the very
    /// next save would atomically destroy whatever the file still holds. A
    /// store in that state answers every mutation with this reason instead.
    read_only: Option<String>,
    _load_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
}

impl BookmarkStore {
    fn load(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let load_path = path.clone();
        let loaded = cx.background_executor().spawn(smol::unblock(move || {
            let loaded = load(&load_path)?;
            let mut icon_provider = crate::desktop::icons::IconProvider::discover();
            let icons = loaded
                .bookmarks
                .iter()
                .filter_map(|bookmark| {
                    icon_provider
                        .icon_for(&bookmark.path, true)
                        .map(|icon| (bookmark.path.clone(), icon))
                })
                .collect();
            anyhow::Ok((loaded, icons))
        }));

        let load_task = cx.spawn(async move |this, cx| {
            let result = loaded.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((loaded, icons)) => {
                        this.bookmarks = loaded.bookmarks;
                        this.icons = icons;
                        if loaded.rejected > 0 {
                            this.read_only = Some(format!(
                                "{} line(s) in “{}” are not bookmarks Marcel understands; \
                                 fix or remove them to change bookmarks, or they would be lost",
                                loaded.rejected,
                                this.path.display()
                            ));
                        }
                    }
                    Err(error) => {
                        this.read_only = Some(format!(
                            "Bookmarks could not be loaded, so they cannot be changed: {error:#}"
                        ));
                    }
                }
                cx.notify();
            });
        });

        Self {
            path,
            bookmarks: Vec::new(),
            icons: HashMap::new(),
            loading: true,
            read_only: None,
            _load_task: Some(load_task),
            save_task: None,
        }
    }

    /// Refuse a mutation while the list is not the user's list yet, telling
    /// them why on the window that asked.
    ///
    /// While the load is still running a save would overwrite the file with
    /// whatever slice of it has been observed so far.
    fn writable(&self, origin: AnyWindowHandle, cx: &mut Context<Self>) -> bool {
        let reason = if self.loading {
            Some("Bookmarks are still loading; try again in a moment".to_string())
        } else {
            self.read_only.clone()
        };
        let Some(reason) = reason else {
            return true;
        };
        cx.spawn(async move |_, cx| {
            surface::deliver(origin, Some(Report::Error(reason)), cx);
        })
        .detach();
        false
    }

    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }

    pub fn icon(&self, path: &Path) -> Option<&Path> {
        self.icons.get(path).map(PathBuf::as_path)
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Add every path that is not bookmarked already, returning how many were.
    ///
    /// `None` means the store refused the mutation entirely and has already
    /// told the user why; the caller must not report anything of its own.
    pub fn add(
        &mut self,
        paths: &[(PathBuf, Option<PathBuf>)],
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.writable(origin, cx) {
            return None;
        }
        let mut added = 0;
        for (path, icon) in paths {
            if !path.is_absolute() || self.bookmarks.iter().any(|b| &b.path == path) {
                continue;
            }
            self.bookmarks.push(Bookmark { path: path.clone() });
            if let Some(icon) = icon {
                self.icons.insert(path.clone(), icon.clone());
            }
            added += 1;
        }
        if added > 0 {
            self.changed(origin, cx);
        }
        Some(added)
    }

    /// Remove the bookmark at `index`, provided it is still `expected`.
    ///
    /// Indices come from a context menu or a drag that opened on one window's
    /// rendering of the list, and another window can mutate the shared store
    /// while that gesture is in flight. The path is what the user aimed at;
    /// an index pointing at something else must not delete it.
    pub fn remove_at(
        &mut self,
        index: usize,
        expected: &Path,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Bookmark> {
        if !self.writable(origin, cx) || !self.still_at(index, expected) {
            return None;
        }
        let bookmark = self.bookmarks.remove(index);
        self.icons.remove(&bookmark.path);
        self.changed(origin, cx);
        Some(bookmark)
    }

    /// Reorder the bookmark at `from` — verified to still be `dragged` — to
    /// the insertion slot.
    pub fn move_to(
        &mut self,
        from: usize,
        dragged: &Path,
        insertion: usize,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.writable(origin, cx)
            || !self.still_at(from, dragged)
            || !reorder(&mut self.bookmarks, from, insertion)
        {
            return false;
        }
        self.changed(origin, cx);
        true
    }

    fn still_at(&self, index: usize, expected: &Path) -> bool {
        self.bookmarks.get(index).is_some_and(|bookmark| bookmark.path == expected)
    }

    fn changed(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.start_save(origin, cx);
        cx.notify();
    }

    /// Write the current list, then write again if it moved on while saving.
    ///
    /// Coalescing here rather than queueing one write per edit is safe now that
    /// there is one list: the follow-up write always publishes the newest
    /// state, whichever window produced it.
    fn start_save(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        // Backstop: nothing above reaches here in a read-only or still-loading
        // store, but a save from such a state would destroy the file's
        // contents, so the writer refuses on its own as well.
        if self.loading || self.read_only.is_some() || self.save_task.is_some() {
            return;
        }
        let path = self.path.clone();
        let snapshot = self.bookmarks.clone();
        let saved_snapshot = snapshot.clone();
        let saving = cx.background_executor().spawn(smol::unblock(move || save(&path, &snapshot)));

        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = saving.await;
            let report = this.update(cx, |this, cx| {
                // Clearing this drops the handle to the task running right now,
                // which cancels whatever it has left to do. Everything after it
                // — including delivering the report — must therefore stay
                // synchronous, with no further await.
                this.save_task = None;
                cx.notify();
                // Edits made while this save ran must reach the disk whether
                // or not the save succeeded; each retry snapshots afresh, so a
                // persistent failure stops as soon as the list stops moving.
                if this.bookmarks != saved_snapshot {
                    this.start_save(origin, cx);
                }
                match result {
                    Err(error) => Some(Report::Error(format!("Could not save bookmarks: {error}"))),
                    Ok(()) => None,
                }
            });
            surface::deliver(origin, report.ok().flatten(), cx);
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn round_trips_paths_that_need_uri_escaping() {
        let sandbox = Sandbox::new();
        let file = sandbox.path("config/bookmarks");
        let bookmarks = vec![
            Bookmark { path: sandbox.path("Work Notes") },
            Bookmark { path: sandbox.path("line\nbreak") },
        ];

        save(&file, &bookmarks).unwrap();
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.bookmarks, bookmarks);
        assert_eq!(loaded.rejected, 0);
    }

    /// Lines Marcel cannot represent are counted, not silently pruned: the
    /// store uses that count to refuse saves that would erase them for good.
    /// A duplicate of a bookmark already loaded carries no data and is not
    /// counted.
    #[test]
    fn unrepresentable_lines_are_counted_and_duplicates_are_collapsed() {
        let sandbox = Sandbox::new();
        let file = sandbox.file(
            "bookmarks",
            "https://example.com/\nnot a url\nfile:///tmp/photos\nfile:///tmp/photos\n",
        );

        let loaded = load(&file).unwrap();
        assert_eq!(loaded.bookmarks, vec![Bookmark { path: PathBuf::from("/tmp/photos") }]);
        assert_eq!(loaded.rejected, 2);
    }

    /// `persist` is a rename, and a rename over a symlink replaces the link
    /// itself. A user keeping the bookmark file as a link into a dotfiles
    /// repository must get their target updated, not their link destroyed.
    #[test]
    fn saving_through_a_symlinked_bookmark_file_updates_the_target() {
        let sandbox = Sandbox::new();
        let target = sandbox.file("dotfiles/bookmarks", "");
        let link = sandbox.path("bookmarks");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let bookmarks = vec![Bookmark { path: PathBuf::from("/tmp/photos") }];

        save(&link, &bookmarks).unwrap();

        assert!(
            fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link must survive the save"
        );
        assert_eq!(load(&target).unwrap().bookmarks, bookmarks);
    }

    /// The bookmark file is a few URLs. One that has grown past what Marcel
    /// would ever write is refused unread, so the store it would have fed
    /// goes read-only instead of replacing it with an empty list.
    #[test]
    fn an_oversized_bookmark_file_is_refused_unread() {
        let sandbox = Sandbox::new();
        let file = sandbox.path("bookmarks");
        fs::File::create(&file).unwrap().set_len(config::MAX_FILE_SIZE + 1).unwrap();

        let error = load(&file).map(|loaded| loaded.rejected).unwrap_err();
        assert!(format!("{error:#}").contains("larger than"), "{error:#}");
    }

    #[test]
    fn reorders_by_insertion_slot() {
        let mut bookmarks = ["a", "b", "c"]
            .into_iter()
            .map(|name| Bookmark { path: PathBuf::from(format!("/{name}")) })
            .collect();

        assert!(reorder(&mut bookmarks, 0, 3));
        assert_eq!(bookmarks.iter().map(Bookmark::label).collect::<Vec<_>>(), ["b", "c", "a"]);
        assert!(reorder(&mut bookmarks, 2, 0));
        assert_eq!(bookmarks.iter().map(Bookmark::label).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(!reorder(&mut bookmarks, 1, 1), "a no-op slot changes nothing");
        assert!(!reorder(&mut bookmarks, 1, 2), "the slot after itself is the same place");
    }
}
