use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocationTarget {
    pub directory: PathBuf,
    pub reveal: Option<PathBuf>,
}

pub fn start_path<I>(arguments: I, current_dir: PathBuf) -> PathBuf
where
    I: IntoIterator<Item = OsString>,
{
    arguments
        .into_iter()
        .find_map(|argument| local_path(&argument, &current_dir))
        .unwrap_or(current_dir)
}

/// The location a launch hands to a Marcel that is already running.
///
/// Always a location, argument or not. `marcel` in a terminal means "show me
/// this folder", and a launch that forwarded nothing could only ask the running
/// Marcel to raise whichever window it already had — which is what it used to
/// do, ignoring the folder the user was standing in.
pub fn launch_uris(start_path: &Path) -> Option<Vec<String>> {
    url::Url::from_file_path(start_path)
        .map(|uri| vec![String::from(uri)])
        .ok()
}

pub fn resolve_location(
    value: &str,
    current_dir: &Path,
    home_dir: Option<&Path>,
) -> Result<LocationTarget, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a folder path or file:// URI".to_string());
    }

    let path = if value == "~" {
        home_dir
            .map(Path::to_path_buf)
            .ok_or_else(|| "Cannot expand ~ because HOME is unavailable".to_string())?
    } else if let Some(relative) = value.strip_prefix("~/") {
        home_dir
            .map(|home| home.join(relative))
            .ok_or_else(|| "Cannot expand ~ because HOME is unavailable".to_string())?
    } else if value.starts_with("file:") {
        let url = Url::parse(value).map_err(|_| "This file URI is invalid".to_string())?;
        url.to_file_path()
            .map_err(|_| "Only local file:// URIs can be opened".to_string())?
    } else if value.contains("://") {
        return Err("Only local paths and file:// URIs can be opened".to_string());
    } else {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            current_dir.join(path)
        }
    };

    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Cannot open “{}”: {error}", path.display()))?;
    let path = fs::canonicalize(&path).unwrap_or(path);
    if metadata.is_dir() {
        return Ok(LocationTarget {
            directory: path,
            reveal: None,
        });
    }
    if metadata.is_file() {
        let directory = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("“{}” has no parent folder", path.display()))?;
        return Ok(LocationTarget {
            directory,
            reveal: Some(path),
        });
    }

    Err(format!(
        "“{}” is not a regular file or folder",
        path.display()
    ))
}

fn local_path(argument: &OsString, current_dir: &Path) -> Option<PathBuf> {
    if let Some(value) = argument.to_str()
        && let Ok(url) = Url::parse(value)
    {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }

    let path = PathBuf::from(argument);
    Some(if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn no_arguments_starts_in_the_process_directory() {
        assert_eq!(
            start_path([], PathBuf::from("/home/test")),
            PathBuf::from("/home/test")
        );
    }

    #[test]
    fn relative_paths_are_resolved_from_the_process_directory() {
        assert_eq!(
            start_path([OsString::from("Downloads")], PathBuf::from("/home/test")),
            PathBuf::from("/home/test/Downloads")
        );
    }

    #[test]
    fn local_file_uris_are_decoded() {
        assert_eq!(
            start_path(
                [OsString::from("file:///home/test/My%20Files")],
                PathBuf::from("/")
            ),
            PathBuf::from("/home/test/My Files")
        );
    }

    #[test]
    fn unsupported_uris_are_skipped_for_a_later_local_target() {
        assert_eq!(
            start_path(
                [
                    OsString::from("https://example.com/folder"),
                    OsString::from("/home/test/Documents"),
                ],
                PathBuf::from("/")
            ),
            PathBuf::from("/home/test/Documents")
        );
    }

    #[test]
    fn address_locations_expand_home_and_resolve_relative_folders() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let documents = home.join("Documents");
        fs::create_dir_all(&documents).unwrap();

        assert_eq!(
            resolve_location("~/Documents", temp.path(), Some(&home)).unwrap(),
            LocationTarget {
                directory: documents.canonicalize().unwrap(),
                reveal: None,
            }
        );
        assert_eq!(
            resolve_location("home/Documents", temp.path(), Some(&home)).unwrap(),
            LocationTarget {
                directory: documents.canonicalize().unwrap(),
                reveal: None,
            }
        );
    }

    #[test]
    fn address_file_uris_open_the_parent_and_reveal_the_file() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("My File.txt");
        fs::write(&file, b"hello").unwrap();
        let uri = Url::from_file_path(&file).unwrap();

        assert_eq!(
            resolve_location(uri.as_str(), Path::new("/"), None).unwrap(),
            LocationTarget {
                directory: temp.path().canonicalize().unwrap(),
                reveal: Some(file.canonicalize().unwrap()),
            }
        );
    }

    #[test]
    fn address_locations_reject_remote_uris_and_missing_paths() {
        assert_eq!(
            resolve_location("https://example.com/folder", Path::new("/"), None),
            Err("Only local paths and file:// URIs can be opened".to_string())
        );
        assert!(resolve_location("/definitely/missing/marcel-path", Path::new("/"), None).is_err());
    }
}
