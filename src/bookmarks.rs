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
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Task};
use url::Url;

use crate::surface::{self, Report};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub path: PathBuf,
}

impl Bookmark {
    pub fn label(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

pub fn default_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        // The XDG base-directory spec says a relative value must be ignored;
        // honoring one would scatter the user's bookmarks across whatever
        // directory Marcel happened to be launched from.
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("marcel")
        .join("bookmarks")
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
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedBookmarks {
                bookmarks: Vec::new(),
                rejected: 0,
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read bookmarks from “{}”", path.display()));
        }
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
    Ok(LoadedBookmarks {
        bookmarks,
        rejected,
    })
}

pub fn save(path: &Path, bookmarks: &[Bookmark]) -> Result<()> {
    // Resolve a symlinked bookmark file to its target: `persist` is a rename,
    // and renaming over the link would silently replace the user's link (to a
    // dotfiles repository, say) with a regular file.
    let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = path
        .parent()
        .context("Bookmark file has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create “{}”", parent.display()))?;
    // Bookmarks are user data, and every window runs its own save task under
    // the same process id. Reserve the temporary file atomically so two
    // windows cannot interleave writes into one predictable path.
    let mut file = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "Could not create a temporary file in “{}”",
            parent.display()
        )
    })?;
    for bookmark in bookmarks {
        if !bookmark.path.is_absolute() {
            bail!(
                "Cannot save relative bookmark “{}”",
                bookmark.path.display()
            );
        }
        let url = Url::from_file_path(&bookmark.path)
            .map_err(|_| anyhow::anyhow!("Invalid bookmark “{}”", bookmark.path.display()))?;
        writeln!(file, "{url}")?;
    }
    file.as_file()
        .sync_all()
        .with_context(|| format!("Could not flush bookmarks for “{}”", path.display()))?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Could not update “{}”", path.display()))?;
    crate::state::sync_directory(parent);
    Ok(())
}

pub fn add(bookmarks: &mut Vec<Bookmark>, path: PathBuf) -> bool {
    if !path.is_absolute() || bookmarks.iter().any(|bookmark| bookmark.path == path) {
        return false;
    }
    bookmarks.push(Bookmark { path });
    true
}

pub fn remove(bookmarks: &mut Vec<Bookmark>, index: usize) -> Option<Bookmark> {
    (index < bookmarks.len()).then(|| bookmarks.remove(index))
}

struct GlobalBookmarks(Entity<BookmarkStore>);

impl Global for GlobalBookmarks {}

/// The application's bookmark store, created and loaded on first use.
pub fn global(home: &Path, cx: &mut App) -> Entity<BookmarkStore> {
    if let Some(existing) = cx.try_global::<GlobalBookmarks>() {
        return existing.0.clone();
    }
    let store = cx.new(|cx| BookmarkStore::load(default_path(home), cx));
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
            let mut icon_provider = crate::icons::IconProvider::discover();
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
                        eprintln!("Could not load Marcel bookmarks: {error:#}");
                        this.read_only = Some(format!(
                            "Bookmarks could not be loaded, so they cannot be changed: {error}"
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

    /// Why a mutation cannot be accepted right now, if it cannot.
    ///
    /// While the load is still running the in-memory list is not the user's
    /// list yet, and a save would overwrite the file with whatever slice of it
    /// has been observed so far.
    fn unavailable_reason(&self) -> Option<String> {
        if self.loading {
            return Some("Bookmarks are still loading; try again in a moment".to_string());
        }
        self.read_only.clone()
    }

    /// Refuse a mutation, telling the user why on the window that asked.
    fn refuse(&self, reason: String, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        cx.spawn(async move |_, cx| {
            surface::deliver(origin, Some(Report::Error(reason)), cx);
        })
        .detach();
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
        if let Some(reason) = self.unavailable_reason() {
            self.refuse(reason, origin, cx);
            return None;
        }
        let mut added = 0;
        for (path, icon) in paths {
            if !add(&mut self.bookmarks, path.clone()) {
                continue;
            }
            if let Some(icon) = icon {
                self.icons.insert(path.clone(), icon.clone());
            }
            added += 1;
        }
        if added > 0 {
            self.start_save(origin, cx);
            cx.notify();
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
        if let Some(reason) = self.unavailable_reason() {
            self.refuse(reason, origin, cx);
            return None;
        }
        if self
            .bookmarks
            .get(index)
            .is_none_or(|bookmark| bookmark.path != expected)
        {
            return None;
        }
        let bookmark = remove(&mut self.bookmarks, index)?;
        self.icons.remove(&bookmark.path);
        self.start_save(origin, cx);
        cx.notify();
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
        if let Some(reason) = self.unavailable_reason() {
            self.refuse(reason, origin, cx);
            return false;
        }
        if self
            .bookmarks
            .get(from)
            .is_none_or(|bookmark| bookmark.path != dragged)
        {
            return false;
        }
        if !reorder(&mut self.bookmarks, from, insertion) {
            return false;
        }
        self.start_save(origin, cx);
        cx.notify();
        true
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
        let saving = cx
            .background_executor()
            .spawn(smol::unblock(move || save(&path, &snapshot)));

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

pub fn reorder(bookmarks: &mut Vec<Bookmark>, from: usize, insertion: usize) -> bool {
    if from >= bookmarks.len() || insertion > bookmarks.len() {
        return false;
    }
    let bookmark = bookmarks.remove(from);
    let adjusted = if from < insertion {
        insertion.saturating_sub(1)
    } else {
        insertion
    };
    if adjusted == from {
        bookmarks.insert(from, bookmark);
        return false;
    }
    bookmarks.insert(adjusted, bookmark);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_paths_that_need_uri_escaping() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("config/bookmarks");
        let bookmarks = vec![
            Bookmark {
                path: root.path().join("Work Notes"),
            },
            Bookmark {
                path: root.path().join("line\nbreak"),
            },
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
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("bookmarks");
        fs::write(
            &file,
            "https://example.com/\nnot a url\nfile:///tmp/photos\nfile:///tmp/photos\n",
        )
        .unwrap();

        let loaded = load(&file).unwrap();
        assert_eq!(
            loaded.bookmarks,
            vec![Bookmark {
                path: PathBuf::from("/tmp/photos")
            }]
        );
        assert_eq!(loaded.rejected, 2);
    }

    /// `persist` is a rename, and a rename over a symlink replaces the link
    /// itself. A user keeping the bookmark file as a link into a dotfiles
    /// repository must get their target updated, not their link destroyed.
    #[test]
    fn saving_through_a_symlinked_bookmark_file_updates_the_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("dotfiles/bookmarks");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "").unwrap();
        let link = root.path().join("bookmarks");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let bookmarks = vec![Bookmark {
            path: PathBuf::from("/tmp/photos"),
        }];

        save(&link, &bookmarks).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link must survive the save"
        );
        assert_eq!(load(&target).unwrap().bookmarks, bookmarks);
    }

    #[test]
    fn reorders_by_insertion_slot() {
        let mut bookmarks = ["a", "b", "c"]
            .into_iter()
            .map(|name| Bookmark {
                path: PathBuf::from(format!("/{name}")),
            })
            .collect();

        assert!(reorder(&mut bookmarks, 0, 3));
        assert_eq!(
            bookmarks.iter().map(Bookmark::label).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        assert!(reorder(&mut bookmarks, 2, 0));
        assert_eq!(
            bookmarks.iter().map(Bookmark::label).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        assert_eq!(remove(&mut bookmarks, 1).unwrap().label(), "b");
        assert_eq!(
            bookmarks.iter().map(Bookmark::label).collect::<Vec<_>>(),
            ["a", "c"]
        );
    }
}
