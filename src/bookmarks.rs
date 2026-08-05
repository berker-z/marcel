use std::{
    collections::HashSet,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use url::Url;

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
