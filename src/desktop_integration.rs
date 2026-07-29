use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
};

use async_channel::{Receiver, Sender, TrySendError};
use url::Url;
use zbus::{self, zvariant::OwnedValue};

pub const APPLICATION_ID: &str = "io.github.berker_z.Marcel";
pub const APPLICATION_OBJECT_PATH: &str = "/io/github/berker_z/Marcel";
pub const FILE_MANAGER_OBJECT_PATH: &str = "/org/freedesktop/FileManager1";

const MAX_REQUEST_URIS: usize = 64;
const MAX_REQUEST_URI_BYTES: usize = 64 * 1024;
const REQUEST_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealedLocation {
    pub directory: PathBuf,
    pub items: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopRequest {
    Activate,
    Open(Vec<RevealedLocation>),
    ShowFolders(Vec<PathBuf>),
    ShowItems(Vec<RevealedLocation>),
    ShowItemProperties(Vec<PathBuf>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UriRequestKind {
    Open,
    ShowFolders,
    ShowItems,
    ShowItemProperties,
}

pub enum InstanceStartup {
    Primary(DesktopRuntime),
    Forwarded,
    Unavailable(String),
}

pub struct DesktopRuntime {
    _connection: zbus::Connection,
    requests: Receiver<DesktopRequest>,
}

impl DesktopRuntime {
    pub fn requests(&self) -> Receiver<DesktopRequest> {
        self.requests.clone()
    }
}

#[derive(Clone)]
struct ApplicationService {
    requests: Sender<DesktopRequest>,
}

#[derive(Clone)]
struct FileManagerService {
    requests: Sender<DesktopRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopRequestError(String);

impl DesktopRequestError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for DesktopRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DesktopRequestError {}

pub async fn acquire_or_forward(initial_uris: Option<Vec<String>>) -> InstanceStartup {
    let (sender, receiver) = async_channel::bounded(REQUEST_QUEUE_CAPACITY);
    let builder = match zbus::connection::Builder::session() {
        Ok(builder) => builder,
        Err(error) => return InstanceStartup::Unavailable(error.to_string()),
    };
    let builder = match builder
        .serve_at(
            APPLICATION_OBJECT_PATH,
            ApplicationService {
                requests: sender.clone(),
            },
        )
        .and_then(|builder| {
            builder.serve_at(
                FILE_MANAGER_OBJECT_PATH,
                FileManagerService { requests: sender },
            )
        })
        .and_then(|builder| builder.name(APPLICATION_ID))
    {
        Ok(builder) => builder
            .allow_name_replacements(false)
            .replace_existing_names(false),
        Err(error) => return InstanceStartup::Unavailable(error.to_string()),
    };

    match builder.build().await {
        Ok(connection) => InstanceStartup::Primary(DesktopRuntime {
            _connection: connection,
            requests: receiver,
        }),
        Err(zbus::Error::NameTaken) => match forward_to_primary(initial_uris).await {
            Ok(()) => InstanceStartup::Forwarded,
            Err(error) => InstanceStartup::Unavailable(error.to_string()),
        },
        Err(error) => InstanceStartup::Unavailable(error.to_string()),
    }
}

async fn forward_to_primary(initial_uris: Option<Vec<String>>) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        APPLICATION_ID,
        APPLICATION_OBJECT_PATH,
        "org.freedesktop.Application",
    )
    .await?;
    let platform_data = HashMap::<String, OwnedValue>::new();

    if let Some(uris) = initial_uris {
        proxy.call("Open", &(uris, platform_data)).await
    } else {
        proxy.call("Activate", &(platform_data,)).await
    }
}

impl ApplicationService {
    fn enqueue(&self, request: DesktopRequest) -> zbus::fdo::Result<()> {
        enqueue(&self.requests, request)
    }

    async fn validate_and_enqueue(
        &self,
        kind: UriRequestKind,
        uris: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        let request = smol::unblock(move || validate_uri_request(kind, &uris))
            .await
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        self.enqueue(request)
    }
}

#[zbus::interface(interface = "org.freedesktop.Application")]
impl ApplicationService {
    async fn activate(&self, _platform_data: HashMap<String, OwnedValue>) -> zbus::fdo::Result<()> {
        self.enqueue(DesktopRequest::Activate)
    }

    async fn open(
        &self,
        uris: Vec<String>,
        _platform_data: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        self.validate_and_enqueue(UriRequestKind::Open, uris).await
    }

    async fn activate_action(
        &self,
        _action_name: String,
        _parameter: Vec<OwnedValue>,
        _platform_data: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Marcel does not expose desktop actions yet".to_string(),
        ))
    }
}

impl FileManagerService {
    async fn validate_and_enqueue(
        &self,
        kind: UriRequestKind,
        uris: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        let request = smol::unblock(move || validate_uri_request(kind, &uris))
            .await
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        enqueue(&self.requests, request)
    }
}

#[zbus::interface(interface = "org.freedesktop.FileManager1")]
impl FileManagerService {
    async fn show_folders(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.validate_and_enqueue(UriRequestKind::ShowFolders, uris)
            .await
    }

    async fn show_items(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.validate_and_enqueue(UriRequestKind::ShowItems, uris)
            .await
    }

    async fn show_item_properties(
        &self,
        _uris: Vec<String>,
        _startup_id: String,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Marcel Properties is not implemented yet".to_string(),
        ))
    }
}

fn enqueue(sender: &Sender<DesktopRequest>, request: DesktopRequest) -> zbus::fdo::Result<()> {
    sender.try_send(request).map_err(|error| match error {
        TrySendError::Full(_) => {
            zbus::fdo::Error::LimitsExceeded("Marcel's desktop request queue is full".to_string())
        }
        TrySendError::Closed(_) => {
            zbus::fdo::Error::Failed("Marcel's desktop request receiver has stopped".to_string())
        }
    })
}

pub fn validate_uri_request(
    kind: UriRequestKind,
    uris: &[String],
) -> Result<DesktopRequest, DesktopRequestError> {
    validate_request_bounds(uris)?;

    let mut paths = Vec::with_capacity(uris.len());
    for uri in uris {
        paths.push(local_path_from_uri(uri)?);
    }

    match kind {
        UriRequestKind::Open => Ok(DesktopRequest::Open(group_open_targets(paths)?)),
        UriRequestKind::ShowFolders => {
            let folders = paths
                .into_iter()
                .map(|path| require_directory(&path))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(DesktopRequest::ShowFolders(folders))
        }
        UriRequestKind::ShowItems => Ok(DesktopRequest::ShowItems(group_revealed_items(paths)?)),
        UriRequestKind::ShowItemProperties => {
            let paths = paths
                .into_iter()
                .map(|path| require_existing(&path))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(DesktopRequest::ShowItemProperties(paths))
        }
    }
}

fn validate_request_bounds(uris: &[String]) -> Result<(), DesktopRequestError> {
    if uris.is_empty() {
        return Err(DesktopRequestError::new(
            "the request must contain at least one URI",
        ));
    }
    if uris.len() > MAX_REQUEST_URIS {
        return Err(DesktopRequestError::new(format!(
            "the request contains more than {MAX_REQUEST_URIS} URIs"
        )));
    }

    let total_bytes = uris.iter().try_fold(0usize, |total, uri| {
        total.checked_add(uri.len()).ok_or_else(|| {
            DesktopRequestError::new("the request URI size exceeds the supported limit")
        })
    })?;
    if total_bytes > MAX_REQUEST_URI_BYTES {
        return Err(DesktopRequestError::new(format!(
            "the request URI size exceeds {MAX_REQUEST_URI_BYTES} bytes"
        )));
    }

    Ok(())
}

fn local_path_from_uri(uri: &str) -> Result<PathBuf, DesktopRequestError> {
    let parsed =
        Url::parse(uri).map_err(|_| DesktopRequestError::new(format!("invalid URI: {uri}")))?;
    if parsed.scheme() != "file" {
        return Err(DesktopRequestError::new(format!(
            "unsupported URI scheme in {uri}"
        )));
    }

    parsed
        .to_file_path()
        .map_err(|_| DesktopRequestError::new(format!("non-local file URI: {uri}")))
}

fn require_existing(path: &Path) -> Result<PathBuf, DesktopRequestError> {
    fs::metadata(path).map_err(|error| {
        DesktopRequestError::new(format!("cannot inspect {}: {error}", path.display()))
    })?;
    Ok(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn require_directory(path: &Path) -> Result<PathBuf, DesktopRequestError> {
    let metadata = fs::metadata(path).map_err(|error| {
        DesktopRequestError::new(format!("cannot inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_dir() {
        return Err(DesktopRequestError::new(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn group_open_targets(paths: Vec<PathBuf>) -> Result<Vec<RevealedLocation>, DesktopRequestError> {
    let mut locations = Vec::with_capacity(paths.len());
    for path in paths {
        let path = require_existing(&path)?;
        if path.is_dir() {
            locations.push(RevealedLocation {
                directory: path,
                items: Vec::new(),
            });
        } else {
            let directory = path.parent().map(Path::to_path_buf).ok_or_else(|| {
                DesktopRequestError::new(format!("{} has no parent directory", path.display()))
            })?;
            locations.push(RevealedLocation {
                directory,
                items: vec![path],
            });
        }
    }
    Ok(locations)
}

fn group_revealed_items(paths: Vec<PathBuf>) -> Result<Vec<RevealedLocation>, DesktopRequestError> {
    let mut locations = Vec::<RevealedLocation>::new();
    let mut parent_indices = HashMap::<PathBuf, usize>::new();

    for path in paths {
        let path = require_existing(&path)?;
        let directory = path.parent().map(Path::to_path_buf).ok_or_else(|| {
            DesktopRequestError::new(format!("{} has no parent directory", path.display()))
        })?;

        if let Some(index) = parent_indices.get(&directory).copied() {
            locations[index].items.push(path);
        } else {
            parent_indices.insert(directory.clone(), locations.len());
            locations.push(RevealedLocation {
                directory,
                items: vec![path],
            });
        }
    }

    Ok(locations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn uri(path: &Path) -> String {
        Url::from_file_path(path).unwrap().into()
    }

    #[test]
    fn show_folders_accepts_directories_and_rejects_regular_files() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        let file = temp.path().join("file.txt");
        fs::create_dir(&folder).unwrap();
        fs::write(&file, b"hello").unwrap();

        assert_eq!(
            validate_uri_request(UriRequestKind::ShowFolders, &[uri(&folder)]).unwrap(),
            DesktopRequest::ShowFolders(vec![folder.canonicalize().unwrap()])
        );
        assert!(
            validate_uri_request(UriRequestKind::ShowFolders, &[uri(&file)])
                .unwrap_err()
                .to_string()
                .contains("is not a directory")
        );
    }

    #[test]
    fn show_items_groups_targets_by_parent_in_first_seen_order() {
        let temp = tempdir().unwrap();
        let first_folder = temp.path().join("first");
        let second_folder = temp.path().join("second");
        fs::create_dir(&first_folder).unwrap();
        fs::create_dir(&second_folder).unwrap();
        let first = first_folder.join("one");
        let second = second_folder.join("two");
        let third = first_folder.join("three");
        fs::write(&first, b"1").unwrap();
        fs::write(&second, b"2").unwrap();
        fs::write(&third, b"3").unwrap();

        assert_eq!(
            validate_uri_request(
                UriRequestKind::ShowItems,
                &[uri(&first), uri(&second), uri(&third)],
            )
            .unwrap(),
            DesktopRequest::ShowItems(vec![
                RevealedLocation {
                    directory: first_folder.canonicalize().unwrap(),
                    items: vec![first.canonicalize().unwrap(), third.canonicalize().unwrap()],
                },
                RevealedLocation {
                    directory: second_folder.canonicalize().unwrap(),
                    items: vec![second.canonicalize().unwrap()],
                },
            ])
        );
    }

    #[test]
    fn open_keeps_each_requested_location() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        let file = temp.path().join("file.txt");
        fs::create_dir(&folder).unwrap();
        fs::write(&file, b"hello").unwrap();

        assert_eq!(
            validate_uri_request(UriRequestKind::Open, &[uri(&folder), uri(&file)]).unwrap(),
            DesktopRequest::Open(vec![
                RevealedLocation {
                    directory: folder.canonicalize().unwrap(),
                    items: Vec::new(),
                },
                RevealedLocation {
                    directory: temp.path().canonicalize().unwrap(),
                    items: vec![file.canonicalize().unwrap()],
                },
            ])
        );
    }

    #[test]
    fn requests_reject_remote_empty_and_oversized_batches() {
        assert!(validate_uri_request(UriRequestKind::ShowItems, &[]).is_err());
        assert!(
            validate_uri_request(
                UriRequestKind::ShowItems,
                &["https://example.com/file".to_string()]
            )
            .is_err()
        );

        let oversized = vec!["file:///tmp/a".to_string(); MAX_REQUEST_URIS + 1];
        assert!(validate_uri_request(UriRequestKind::ShowItems, &oversized).is_err());
    }

    #[test]
    fn application_id_and_object_path_match() {
        assert_eq!(
            format!("/{}", APPLICATION_ID.replace('.', "/")),
            APPLICATION_OBJECT_PATH
        );
    }
}
