use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct IconProvider {
    #[cfg(target_os = "linux")]
    theme: String,
    cache: HashMap<String, Option<PathBuf>>,
}

impl IconProvider {
    pub fn discover() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            theme: discover_linux_theme(),
            cache: HashMap::new(),
        }
    }

    pub fn icon_for(&mut self, path: &Path, directory: bool) -> Option<PathBuf> {
        let candidates = icon_candidates(path, directory);
        for name in candidates {
            if let Some(icon) = self.lookup(&name) {
                return Some(icon);
            }
        }
        None
    }

    pub fn icon_for_place(&mut self, label: &str) -> Option<PathBuf> {
        for name in place_icon_candidates(label) {
            if let Some(icon) = self.lookup(name) {
                return Some(icon);
            }
        }
        None
    }

    fn lookup(&mut self, name: &str) -> Option<PathBuf> {
        if let Some(cached) = self.cache.get(name) {
            return cached.clone();
        }

        #[cfg(target_os = "linux")]
        let icon = freedesktop_icons::lookup(name)
            .with_theme(&self.theme)
            .with_size(32)
            .with_cache()
            .find();
        #[cfg(not(target_os = "linux"))]
        let icon = None;

        self.cache.insert(name.to_string(), icon.clone());
        icon
    }
}

fn place_icon_candidates(label: &str) -> &'static [&'static str] {
    match label {
        "Home" => &["user-home", "folder-home", "folder"],
        "Desktop" => &["user-desktop", "folder-desktop", "folder"],
        "Documents" => &["folder-documents", "folder"],
        "Downloads" => &["folder-download", "folder-downloads", "folder"],
        "Music" => &["folder-music", "folder"],
        "Pictures" => &["folder-pictures", "folder-images", "folder"],
        "Public" => &["folder-publicshare", "folder-public", "folder"],
        "Templates" => &["folder-templates", "folder"],
        "Videos" => &["folder-videos", "folder-video", "folder"],
        _ => &["folder"],
    }
}

fn icon_candidates(path: &Path, directory: bool) -> Vec<String> {
    if directory {
        return vec!["folder".to_string()];
    }

    let Some(mime) = mime_guess::from_path(path).first() else {
        return vec!["application-x-generic".to_string(), "unknown".to_string()];
    };
    let essence = mime.essence_str();
    let mut candidates = vec![essence.replace('/', "-")];
    let generic = match essence {
        "application/pdf" => "application-pdf",
        value
            if value.starts_with("application/")
                && (value.contains("zip")
                    || value.contains("tar")
                    || value.contains("compressed")
                    || value.contains("archive")) =>
        {
            "package-x-generic"
        }
        value if value.starts_with("text/") => "text-x-generic",
        value if value.starts_with("image/") => "image-x-generic",
        value if value.starts_with("audio/") => "audio-x-generic",
        value if value.starts_with("video/") => "video-x-generic",
        _ => "application-x-generic",
    };
    if candidates
        .first()
        .is_none_or(|specific| specific != generic)
    {
        candidates.push(generic.to_string());
    }
    candidates.push("unknown".to_string());
    candidates
}

#[cfg(target_os = "linux")]
fn discover_linux_theme() -> String {
    std::env::var("MARCEL_ICON_THEME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(read_gtk_icon_theme)
        .or_else(freedesktop_icons::default_theme_gtk)
        .unwrap_or_else(|| "hicolor".to_string())
}

#[cfg(target_os = "linux")]
fn read_gtk_icon_theme() -> Option<String> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"]
        .into_iter()
        .find_map(|relative| {
            let contents = std::fs::read_to_string(config_home.join(relative)).ok()?;
            contents.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "gtk-icon-theme-name")
                    .then(|| value.trim().trim_matches(['\'', '"']).to_string())
                    .filter(|value| !value.is_empty())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_use_the_standard_folder_icon() {
        assert_eq!(icon_candidates(Path::new("/tmp/photos"), true), ["folder"]);
    }

    #[test]
    fn mime_candidates_fall_back_from_specific_to_generic() {
        assert_eq!(
            icon_candidates(Path::new("notes.md"), false),
            ["text-markdown", "text-x-generic", "unknown"]
        );
        assert_eq!(
            icon_candidates(Path::new("photo.png"), false),
            ["image-png", "image-x-generic", "unknown"]
        );
    }

    #[test]
    fn archives_receive_package_fallbacks() {
        let candidates = icon_candidates(Path::new("source.tar.gz"), false);
        assert_eq!(candidates[1], "package-x-generic");
    }

    #[test]
    fn places_use_freedesktop_semantic_icon_names() {
        assert_eq!(
            place_icon_candidates("Home"),
            ["user-home", "folder-home", "folder"]
        );
        assert_eq!(
            place_icon_candidates("Pictures"),
            ["folder-pictures", "folder-images", "folder"]
        );
        assert_eq!(place_icon_candidates("Other"), ["folder"]);
    }
}
