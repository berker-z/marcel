use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const USER_DIRS: [(&str, &str, &str); 8] = [
    ("DESKTOP", "Desktop", "Desktop"),
    ("DOCUMENTS", "Documents", "Documents"),
    ("DOWNLOAD", "Downloads", "Downloads"),
    ("MUSIC", "Music", "Music"),
    ("PICTURES", "Pictures", "Pictures"),
    ("PUBLICSHARE", "Public", "Public"),
    ("TEMPLATES", "Templates", "Templates"),
    ("VIDEOS", "Videos", "Videos"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub kind: PlaceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    Filesystem,
    Trash,
}

impl Place {
    pub fn home(path: PathBuf) -> Self {
        Self {
            label: "Home".to_string(),
            path,
            kind: PlaceKind::Filesystem,
        }
    }

    pub fn trash() -> Self {
        Self {
            label: "Trash".to_string(),
            path: PathBuf::from("trash:///"),
            kind: PlaceKind::Trash,
        }
    }

    pub fn is_trash(&self) -> bool {
        self.kind == PlaceKind::Trash
    }
}

pub fn discover(home: &Path) -> Vec<Place> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let config = fs::read_to_string(config_home.join("user-dirs.dirs")).ok();

    build_places(home, config.as_deref(), Path::is_dir)
}

fn build_places(home: &Path, config: Option<&str>, is_dir: impl Fn(&Path) -> bool) -> Vec<Place> {
    let configured = config.map(parse_user_dirs);
    let mut seen = HashSet::from([home.to_path_buf()]);
    let mut places = vec![Place::home(home.to_path_buf())];

    for (key, label, fallback_name) in USER_DIRS {
        let path = match &configured {
            Some(values) => values
                .get(key)
                .and_then(|value| expand_user_dir(value, home)),
            None => Some(home.join(fallback_name)),
        };
        let Some(path) = path else {
            continue;
        };

        if is_dir(&path) && seen.insert(path.clone()) {
            places.push(Place {
                label: label.to_string(),
                path,
                kind: PlaceKind::Filesystem,
            });
        }
    }

    places.push(Place::trash());
    places
}

fn parse_user_dirs(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }

            let (name, value) = line.split_once('=')?;
            let key = name
                .trim()
                .strip_prefix("XDG_")?
                .strip_suffix("_DIR")?
                .to_string();
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            Some((key, unescape(value)))
        })
        .collect()
}

fn expand_user_dir(value: &str, home: &Path) -> Option<PathBuf> {
    if value == "$HOME" || value == "${HOME}" {
        return Some(home.to_path_buf());
    }
    if let Some(relative) = value
        .strip_prefix("$HOME/")
        .or_else(|| value.strip_prefix("${HOME}/"))
    {
        return Some(home.join(relative));
    }

    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn unescape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            if let Some(escaped) = chars.next() {
                output.push(escaped);
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_existing_conventional_directories_without_a_config() {
        let home = Path::new("/home/test");
        let places = build_places(home, None, |path| {
            path == Path::new("/home/test/Documents") || path == Path::new("/home/test/Pictures")
        });

        assert_eq!(
            places,
            vec![
                Place::home(home.to_path_buf()),
                Place {
                    label: "Documents".to_string(),
                    path: home.join("Documents"),
                    kind: PlaceKind::Filesystem,
                },
                Place {
                    label: "Pictures".to_string(),
                    path: home.join("Pictures"),
                    kind: PlaceKind::Filesystem,
                },
                Place::trash(),
            ]
        );
    }

    #[test]
    fn honors_custom_xdg_paths_and_filters_disabled_duplicates() {
        let home = Path::new("/home/test");
        let config = r#"
            XDG_DESKTOP_DIR="$HOME"
            XDG_DOCUMENTS_DIR="$HOME/Work Notes"
            XDG_DOWNLOAD_DIR="/data/downloads"
            XDG_PICTURES_DIR="$HOME/Photos"
        "#;
        let places = build_places(home, Some(config), |_| true);

        assert_eq!(
            places,
            vec![
                Place::home(home.to_path_buf()),
                Place {
                    label: "Documents".to_string(),
                    path: home.join("Work Notes"),
                    kind: PlaceKind::Filesystem,
                },
                Place {
                    label: "Downloads".to_string(),
                    path: PathBuf::from("/data/downloads"),
                    kind: PlaceKind::Filesystem,
                },
                Place {
                    label: "Pictures".to_string(),
                    path: home.join("Photos"),
                    kind: PlaceKind::Filesystem,
                },
                Place::trash(),
            ]
        );
    }

    #[test]
    fn unescapes_xdg_quoted_values_without_evaluating_shell_code() {
        let values = parse_user_dirs(r#"XDG_DOCUMENTS_DIR="$HOME/Work\ Files""#);
        assert_eq!(
            values.get("DOCUMENTS"),
            Some(&"$HOME/Work Files".to_string())
        );
        assert!(expand_user_dir("$(touch /tmp/nope)", Path::new("/home/test")).is_none());
    }
}
