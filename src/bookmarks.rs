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
        .unwrap_or_else(|| home.join(".config"))
        .join("marcel")
        .join("bookmarks")
}

pub fn load(path: &Path) -> Result<Vec<Bookmark>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read bookmarks from “{}”", path.display()));
        }
    };

    let mut seen = HashSet::new();
    Ok(contents
        .lines()
        .filter_map(|line| {
            let url = Url::parse(line.trim()).ok()?;
            let path = url.to_file_path().ok()?;
            (path.is_absolute() && seen.insert(path.clone())).then_some(Bookmark { path })
        })
        .collect())
}

pub fn save(path: &Path, bookmarks: &[Bookmark]) -> Result<()> {
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
    _load_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
}

impl BookmarkStore {
    fn load(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let load_path = path.clone();
        let loaded = cx.background_executor().spawn(smol::unblock(move || {
            let bookmarks = load(&load_path)?;
            let mut icon_provider = crate::icons::IconProvider::discover();
            let icons = bookmarks
                .iter()
                .filter_map(|bookmark| {
                    icon_provider
                        .icon_for(&bookmark.path, true)
                        .map(|icon| (bookmark.path.clone(), icon))
                })
                .collect();
            anyhow::Ok((bookmarks, icons))
        }));

        let load_task = cx.spawn(async move |this, cx| {
            let result = loaded.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((bookmarks, icons)) => {
                        this.bookmarks = bookmarks;
                        this.icons = icons;
                    }
                    Err(error) => eprintln!("Could not load Marcel bookmarks: {error:#}"),
                }
                cx.notify();
            });
        });

        Self {
            path,
            bookmarks: Vec::new(),
            icons: HashMap::new(),
            loading: true,
            _load_task: Some(load_task),
            save_task: None,
        }
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
    pub fn add(
        &mut self,
        paths: &[(PathBuf, Option<PathBuf>)],
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> usize {
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
        added
    }

    pub fn remove_at(
        &mut self,
        index: usize,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Bookmark> {
        let bookmark = remove(&mut self.bookmarks, index)?;
        self.icons.remove(&bookmark.path);
        self.start_save(origin, cx);
        cx.notify();
        Some(bookmark)
    }

    pub fn move_to(
        &mut self,
        from: usize,
        insertion: usize,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> bool {
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
        if self.save_task.is_some() {
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
                match result {
                    Err(error) => Some(Report::Error(format!("Could not save bookmarks: {error}"))),
                    Ok(()) => {
                        if this.bookmarks != saved_snapshot {
                            this.start_save(origin, cx);
                        }
                        None
                    }
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
        assert_eq!(load(&file).unwrap(), bookmarks);
    }

    #[test]
    fn ignores_invalid_duplicate_and_non_file_entries() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("bookmarks");
        fs::write(
            &file,
            "https://example.com/\nnot a url\nfile:///tmp/photos\nfile:///tmp/photos\n",
        )
        .unwrap();

        assert_eq!(
            load(&file).unwrap(),
            vec![Bookmark {
                path: PathBuf::from("/tmp/photos")
            }]
        );
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
