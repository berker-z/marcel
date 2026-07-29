use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use url::Url;

pub fn start_path<I>(arguments: I, current_dir: PathBuf) -> PathBuf
where
    I: IntoIterator<Item = OsString>,
{
    arguments
        .into_iter()
        .find_map(|argument| local_path(&argument, &current_dir))
        .unwrap_or(current_dir)
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
}
