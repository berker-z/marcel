use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct IconProvider {
    #[cfg(target_os = "linux")]
    explicit_theme: Option<String>,
    #[cfg(target_os = "linux")]
    ambient_theme: String,
    bundled_dir: Option<PathBuf>,
    cache: HashMap<Vec<String>, Option<PathBuf>>,
}

impl IconProvider {
    pub fn discover() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            explicit_theme: explicit_linux_theme(),
            #[cfg(target_os = "linux")]
            ambient_theme: discover_ambient_linux_theme(),
            bundled_dir: discover_bundled_icon_dir(),
            cache: HashMap::new(),
        }
    }

    pub fn icon_for(&mut self, path: &Path, directory: bool) -> Option<PathBuf> {
        let candidates = icon_candidates(path, directory);
        self.lookup(&candidates)
    }

    pub fn icon_for_place(&mut self, label: &str) -> Option<PathBuf> {
        let candidates = place_icon_candidates(label)
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        self.lookup(&candidates)
    }

    fn lookup(&mut self, candidates: &[String]) -> Option<PathBuf> {
        if let Some(cached) = self.cache.get(candidates) {
            return cached.clone();
        }

        enum Layer<'a> {
            #[cfg(target_os = "linux")]
            Theme(&'a str),
            Bundled(&'a Path),
        }

        let mut layers = Vec::new();
        #[cfg(target_os = "linux")]
        if let Some(theme) = self.explicit_theme.as_deref() {
            layers.push(Layer::Theme(theme));
        }
        if let Some(directory) = self.bundled_dir.as_deref() {
            layers.push(Layer::Bundled(directory));
        }
        #[cfg(target_os = "linux")]
        layers.push(Layer::Theme(&self.ambient_theme));

        let icon = resolve_layered(candidates, layers.len(), |layer, name| {
            match layers[layer] {
                #[cfg(target_os = "linux")]
                Layer::Theme(theme) => freedesktop_icons::lookup(name)
                    .with_theme(theme)
                    .with_size(32)
                    .with_cache()
                    .find(),
                Layer::Bundled(directory) => {
                    let path = directory.join(format!("{name}.svg"));
                    path.is_file().then_some(path)
                }
            }
        });

        self.cache.insert(candidates.to_vec(), icon.clone());
        icon
    }
}

fn resolve_layered<T>(
    candidates: &[String],
    layer_count: usize,
    mut lookup: impl FnMut(usize, &str) -> Option<T>,
) -> Option<T> {
    (0..layer_count).find_map(|layer| {
        candidates
            .iter()
            .find_map(|candidate| lookup(layer, candidate))
    })
}

fn discover_bundled_icon_dir() -> Option<PathBuf> {
    let configured = std::env::var_os("MARCEL_ASSET_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join("icons/nordzy"));
    let installed = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(Path::to_path_buf))
        .map(|prefix| prefix.join("share/marcel/icons/nordzy"));
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons/nordzy");

    configured
        .into_iter()
        .chain(installed)
        .chain([development])
        .find(|directory| directory.is_dir())
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
        "Trash" => &["user-trash", "user-trash-full", "folder"],
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
fn explicit_linux_theme() -> Option<String> {
    std::env::var("MARCEL_ICON_THEME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn discover_ambient_linux_theme() -> String {
    read_gtk_icon_theme()
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
        assert_eq!(
            place_icon_candidates("Trash"),
            ["user-trash", "user-trash-full", "folder"]
        );
        assert_eq!(place_icon_candidates("Other"), ["folder"]);
    }

    #[test]
    fn icon_layers_take_priority_over_more_specific_lower_layer_names() {
        let candidates = ["image-png".to_string(), "image-x-generic".to_string()];
        let resolved = resolve_layered(&candidates, 3, |layer, candidate| {
            match (layer, candidate) {
                (1, "image-x-generic") => Some("bundled generic"),
                (2, "image-png") => Some("ambient specific"),
                _ => None,
            }
        });
        assert_eq!(resolved, Some("bundled generic"));
    }

    #[test]
    fn explicit_layer_precedes_bundled_and_ambient_layers() {
        let candidates = ["folder".to_string()];
        let resolved = resolve_layered(&candidates, 3, |layer, _| match layer {
            0 => Some("explicit"),
            1 => Some("bundled"),
            2 => Some("ambient"),
            _ => None,
        });
        assert_eq!(resolved, Some("explicit"));
    }

    #[test]
    fn development_bundle_contains_every_curated_semantic_icon() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons/nordzy");
        let names = [
            "folder",
            "user-home",
            "user-desktop",
            "folder-documents",
            "folder-download",
            "folder-music",
            "folder-pictures",
            "folder-publicshare",
            "folder-templates",
            "folder-videos",
            "user-trash",
            "user-trash-full",
            "application-pdf",
            "package-x-generic",
            "text-x-generic",
            "image-x-generic",
            "audio-x-generic",
            "video-x-generic",
            "application-x-generic",
            "unknown",
        ];
        for name in names {
            assert!(directory.join(format!("{name}.svg")).is_file(), "{name}");
        }
    }
}
