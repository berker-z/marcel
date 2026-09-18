use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use url::Url;

use super::{bus, file_chooser};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocationTarget {
    pub directory: PathBuf,
    pub reveal: Option<PathBuf>,
}

/// What the command line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Show a folder. `explicit` says whether the user named one, or a URI
    /// that could not be used; a bare `marcel` is not explicit.
    Open {
        start_path: PathBuf,
        explicit: bool,
    },
    Help,
    Version,
}

/// The text `--help` prints. Also the reference for what the parser accepts.
pub const USAGE: &str = "\
Usage: marcel-rs [OPTIONS] [LOCATION]...

Open Marcel at LOCATION: a folder path, a file path (its folder is shown), or
a file:// URI. Relative paths resolve from the current directory. With no
LOCATION the current directory is shown. When several are given, the first
usable one is opened. Other URI schemes are ignored.

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit
  --             Treat every remaining argument as a location
";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Read the command line.
///
/// Options are recognised only before a `--`; anything after it is a location
/// even when it starts with a dash. An option the program does not know is an
/// error rather than a path, because a typo like `--reveal` used to become the
/// folder `./--reveal`, be refused by the running Marcel, and leave the user
/// with a second, bus-less window at nothing.
pub fn parse_arguments<I>(arguments: I, current_dir: PathBuf) -> Result<Invocation, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut locations = Vec::new();
    let mut options_ended = false;
    for argument in arguments {
        if options_ended {
            locations.push(argument);
            continue;
        }
        match argument.to_str() {
            Some("--") => options_ended = true,
            Some("-h" | "--help") => return Ok(Invocation::Help),
            Some("-V" | "--version") => return Ok(Invocation::Version),
            // A lone dash is a file name by convention, and nothing here reads
            // standard input, so it is a location like any other.
            Some(option) if option.starts_with('-') && option != "-" => {
                return Err(format!(
                    "marcel-rs: unrecognized option '{option}'\nTry 'marcel-rs --help' for more information."
                ));
            }
            _ => locations.push(argument),
        }
    }
    let explicit = !locations.is_empty();
    Ok(Invocation::Open { start_path: start_path(locations, current_dir), explicit })
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

/// The folder a bus-started Marcel shows when it has to show something.
///
/// A service start has no folder of its own: its working directory is the
/// daemon's, which is `/` or wherever the session manager happened to be.
/// Home is what a user expects from a Marcel they did not point anywhere.
pub fn service_start_path(daemon_dir: PathBuf) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .unwrap_or(daemon_dir)
}

/// The location a launch hands to a Marcel that is already running.
///
/// Always a location, argument or not. `marcel` in a terminal means "show me
/// this folder", and a launch that forwarded nothing could only ask the running
/// Marcel to raise whichever window it already had — which is what it used to
/// do, ignoring the folder the user was standing in.
pub fn launch_uris(start_path: &Path) -> Option<Vec<String>> {
    url::Url::from_file_path(start_path).map(|uri| vec![String::from(uri)]).ok()
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
        url.to_file_path().map_err(|_| "Only local file:// URIs can be opened".to_string())?
    } else if value.contains("://") {
        return Err("Only local paths and file:// URIs can be opened".to_string());
    } else {
        let path = PathBuf::from(value);
        if path.is_absolute() { path } else { current_dir.join(path) }
    };

    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Cannot open “{}”: {error}", path.display()))?;
    let path = fs::canonicalize(&path).unwrap_or(path);
    if metadata.is_dir() {
        return Ok(LocationTarget { directory: path, reveal: None });
    }
    if metadata.is_file() {
        let directory = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("“{}” has no parent folder", path.display()))?;
        return Ok(LocationTarget { directory, reveal: Some(path) });
    }

    Err(format!("“{}” is not a regular file or folder", path.display()))
}

/// A command-line location as a path, or `None` for a URI Marcel cannot show.
///
/// An argument is a URI when it starts with `file:` or has a scheme followed
/// by `//`; everything else is a path. `Url::parse` alone was the wrong test:
/// it accepts any `word:rest`, so a folder called `notes:today` parsed as a
/// URI with scheme `notes` and was skipped.
fn local_path(argument: &OsString, current_dir: &Path) -> Option<PathBuf> {
    if let Some(value) = argument.to_str()
        && (value.starts_with("file:") || value.contains("://"))
    {
        let url = Url::parse(value).ok()?;
        return (url.scheme() == "file").then(|| url.to_file_path().ok()).flatten();
    }

    let path = PathBuf::from(argument);
    Some(if path.is_absolute() { path } else { current_dir.join(path) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_starts_in_the_process_directory() {
        assert_eq!(start_path([], PathBuf::from("/home/test")), PathBuf::from("/home/test"));
        assert_eq!(
            parse_arguments([], PathBuf::from("/home/test")),
            Ok(Invocation::Open { start_path: PathBuf::from("/home/test"), explicit: false })
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
            start_path([OsString::from("file:///home/test/My%20Files")], PathBuf::from("/")),
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
    fn a_folder_with_a_colon_in_its_name_is_a_path_not_a_scheme() {
        assert_eq!(
            start_path(args(&["notes:today"]), PathBuf::from("/home/test")),
            PathBuf::from("/home/test/notes:today")
        );
        assert_eq!(
            start_path(args(&["/srv/backup:2026"]), PathBuf::from("/")),
            PathBuf::from("/srv/backup:2026")
        );
    }

    #[test]
    fn help_and_version_win_over_locations() {
        assert_eq!(parse_arguments(args(&["--help"]), PathBuf::from("/")), Ok(Invocation::Help));
        assert_eq!(
            parse_arguments(args(&["/tmp", "-h"]), PathBuf::from("/")),
            Ok(Invocation::Help)
        );
        assert_eq!(
            parse_arguments(args(&["--version"]), PathBuf::from("/")),
            Ok(Invocation::Version)
        );
        assert_eq!(parse_arguments(args(&["-V"]), PathBuf::from("/")), Ok(Invocation::Version));
        assert!(USAGE.contains("--help") && USAGE.contains("--version") && USAGE.contains("--"));
    }

    #[test]
    fn unknown_options_are_errors_rather_than_folders() {
        let error = parse_arguments(args(&["--reveal", "/tmp"]), PathBuf::from("/")).unwrap_err();
        assert!(error.contains("unrecognized option '--reveal'"), "{error}");
        assert!(error.contains("--help"), "{error}");
        assert!(parse_arguments(args(&["-x"]), PathBuf::from("/")).is_err());
    }

    #[test]
    fn a_double_dash_makes_everything_after_it_a_location() {
        assert_eq!(
            parse_arguments(args(&["--", "--help"]), PathBuf::from("/home/test")),
            Ok(Invocation::Open { start_path: PathBuf::from("/home/test/--help"), explicit: true })
        );
        assert_eq!(
            parse_arguments(args(&["--", "-"]), PathBuf::from("/home/test")),
            Ok(Invocation::Open { start_path: PathBuf::from("/home/test/-"), explicit: true })
        );
        assert_eq!(
            parse_arguments(args(&["-"]), PathBuf::from("/home/test")),
            Ok(Invocation::Open { start_path: PathBuf::from("/home/test/-"), explicit: true })
        );
    }

    #[test]
    fn a_launch_that_named_only_an_unusable_uri_still_counts_as_explicit() {
        assert_eq!(
            parse_arguments(args(&["https://example.com"]), PathBuf::from("/home/test")),
            Ok(Invocation::Open { start_path: PathBuf::from("/home/test"), explicit: true })
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
            LocationTarget { directory: documents.canonicalize().unwrap(), reveal: None }
        );
        assert_eq!(
            resolve_location("home/Documents", temp.path(), Some(&home)).unwrap(),
            LocationTarget { directory: documents.canonicalize().unwrap(), reveal: None }
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

/// Whether this process was started by the session bus to answer a request.
///
/// `dbus-daemon` says so by setting `DBUS_STARTER_BUS_TYPE` or
/// `DBUS_STARTER_ADDRESS` in the child. `dbus-broker` starts services through
/// systemd instead and sets neither; what it leaves behind is the transient
/// unit it asked for, `dbus-:1.4-<bus name>@0.service`, which is this
/// process's cgroup. Missing that meant every portal- or FileManager1-started
/// Marcel on a dbus-broker system opened a browsing window at the daemon's
/// working directory before the real request arrived.
pub fn started_by_bus_activation() -> bool {
    if std::env::var_os("DBUS_STARTER_BUS_TYPE").is_some()
        || std::env::var_os("DBUS_STARTER_ADDRESS").is_some()
    {
        return true;
    }
    fs::read_to_string("/proc/self/cgroup")
        .map(|cgroup| cgroup_is_bus_activation(&cgroup))
        .unwrap_or(false)
}

/// The bus names whose activation starts this program. A transient unit for
/// any other name is somebody else's activation.
const ACTIVATABLE_NAMES: [&str; 3] =
    [bus::APPLICATION_ID, bus::FILE_MANAGER_BUS_NAME, file_chooser::FILE_CHOOSER_BUS_NAME];

/// Whether the cgroup is a dbus-broker activation unit for one of Marcel's
/// own names.
///
/// Any `dbus-*.service` used to count. Under dbus-broker every child of an
/// activated service shares its unit, and gnome-terminal-server, kgx, and
/// ptyxis are all bus-activated, so `marcel-rs` typed into one of those
/// terminals saw itself as bus-started, opened no window, and waited for a
/// request that never came.
fn cgroup_is_bus_activation(cgroup: &str) -> bool {
    cgroup.lines().any(|line| {
        line.rsplit('/')
            .next()
            .and_then(activated_bus_name)
            .is_some_and(|name| ACTIVATABLE_NAMES.contains(&name))
    })
}

/// The bus name in a dbus-broker transient unit, `dbus-:1.4-<name>@0.service`.
fn activated_bus_name(unit: &str) -> Option<&str> {
    let body = unit.strip_prefix("dbus-:")?.strip_suffix(".service")?;
    let (_unique_name, rest) = body.split_once('-')?;
    let (name, _instance) = rest.rsplit_once('@')?;
    Some(name)
}

#[cfg(test)]
mod activation_tests {
    use super::cgroup_is_bus_activation;

    fn cgroup(unit: &str) -> String {
        format!("0::/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}\n")
    }

    #[test]
    fn marcels_own_transient_units_are_recognised() {
        assert!(cgroup_is_bus_activation(&cgroup(
            "dbus-:1.4-org.freedesktop.impl.portal.desktop.marcel@0.service"
        )));
        assert!(cgroup_is_bus_activation(&cgroup("dbus-:1.4-io.github.berker_z.Marcel@0.service")));
        assert!(cgroup_is_bus_activation(&cgroup(
            "dbus-:1.12-org.freedesktop.FileManager1@3.service"
        )));
    }

    #[test]
    fn another_applications_activation_unit_is_not() {
        assert!(!cgroup_is_bus_activation(&cgroup("dbus-:1.4-org.gnome.Terminal@0.service")));
        assert!(!cgroup_is_bus_activation(&cgroup("dbus-:1.4-org.gnome.Console@0.service")));
        assert!(!cgroup_is_bus_activation(&cgroup(
            "dbus-:1.4-io.github.berker_z.Marcel.Helper@0.service"
        )));
    }

    #[test]
    fn ordinary_units_and_scopes_are_not() {
        assert!(!cgroup_is_bus_activation(&cgroup("app-kitty-1234.scope")));
        assert!(!cgroup_is_bus_activation(
            "0::/user.slice/user-1000.slice/user@1000.service/session.slice/dbus.service\n"
        ));
        assert!(!cgroup_is_bus_activation(&cgroup("app-hyprland-marcel-rs-99.scope")));
        assert!(!cgroup_is_bus_activation(""));
    }
}
