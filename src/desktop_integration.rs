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
    use std::{process::Command, time::Duration};
    use tempfile::tempdir;

    const PRIVATE_BUS_CHILD: &str = "MARCEL_PRIVATE_BUS_TEST_CHILD";

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

    #[test]
    fn private_session_bus_integration() {
        if std::env::var_os(PRIVATE_BUS_CHILD).is_some() {
            return;
        }

        let status = Command::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe().expect("test executable must have a path"))
            .arg("--exact")
            .arg("desktop_integration::tests::private_session_bus_child")
            .arg("--nocapture")
            .env(PRIVATE_BUS_CHILD, "1")
            .status()
            .expect("dbus-run-session must be available in Marcel's development environment");

        assert!(status.success(), "private session-bus child failed");
    }

    #[test]
    fn private_session_bus_child() {
        if std::env::var_os(PRIVATE_BUS_CHILD).is_none() {
            return;
        }

        smol::block_on(async {
            let primary = match acquire_or_forward(None).await {
                InstanceStartup::Primary(runtime) => runtime,
                InstanceStartup::Forwarded => panic!("fresh private bus unexpectedly had an owner"),
                InstanceStartup::Unavailable(error) => {
                    panic!("failed to own the application name: {error}")
                }
            };
            let requests = primary.requests();
            let client = zbus::Connection::session()
                .await
                .expect("client must connect to the private bus");
            let bus = zbus::fdo::DBusProxy::new(&client)
                .await
                .expect("bus proxy must initialize");
            let owned_names = bus.list_names().await.expect("bus names must be readable");
            assert!(
                owned_names
                    .iter()
                    .any(|name| name.as_str() == APPLICATION_ID),
                "primary must own Marcel's application name"
            );
            assert!(
                owned_names
                    .iter()
                    .all(|name| name.as_str() != "org.freedesktop.FileManager1"),
                "the ordinary process must not claim the generic file-manager name"
            );

            let temp = tempdir().expect("fixture directory must be created");
            let folder = temp.path().join("folder");
            let file = folder.join("file.txt");
            fs::create_dir(&folder).expect("fixture folder must be created");
            fs::write(&file, b"hello").expect("fixture file must be created");

            assert!(matches!(
                acquire_or_forward(Some(vec![uri(&file)])).await,
                InstanceStartup::Forwarded
            ));
            assert_eq!(
                receive_request(&requests).await,
                DesktopRequest::Open(vec![RevealedLocation {
                    directory: folder.canonicalize().unwrap(),
                    items: vec![file.canonicalize().unwrap()],
                }])
            );

            let application = zbus::Proxy::new(
                &client,
                APPLICATION_ID,
                APPLICATION_OBJECT_PATH,
                "org.freedesktop.Application",
            )
            .await
            .expect("application proxy must initialize");
            application
                .call::<_, _, ()>("Activate", &(HashMap::<String, OwnedValue>::new(),))
                .await
                .expect("warm activation must succeed");
            assert_eq!(receive_request(&requests).await, DesktopRequest::Activate);

            let file_manager = zbus::Proxy::new(
                &client,
                APPLICATION_ID,
                FILE_MANAGER_OBJECT_PATH,
                "org.freedesktop.FileManager1",
            )
            .await
            .expect("file-manager proxy must initialize");
            let invalid: zbus::Result<()> = file_manager
                .call("ShowFolders", &(vec![uri(&file)], String::new()))
                .await;
            match invalid.expect_err("a regular file is not a ShowFolders target") {
                zbus::Error::MethodError(name, _, _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs")
                }
                error => panic!("expected a typed InvalidArgs reply, got {error}"),
            }

            file_manager
                .call::<_, _, ()>("ShowItems", &(vec![uri(&file)], String::new()))
                .await
                .expect("ShowItems must accept a local file");
            assert_eq!(
                receive_request(&requests).await,
                DesktopRequest::ShowItems(vec![RevealedLocation {
                    directory: folder.canonicalize().unwrap(),
                    items: vec![file.canonicalize().unwrap()],
                }])
            );

            drop(primary);
            let mut last_error = None;
            let mut replacement = None;
            for _ in 0..80 {
                match acquire_or_forward(None).await {
                    InstanceStartup::Primary(runtime) => {
                        replacement = Some(runtime);
                        break;
                    }
                    InstanceStartup::Forwarded => {}
                    InstanceStartup::Unavailable(error) => last_error = Some(error),
                }
                smol::Timer::after(Duration::from_millis(25)).await;
            }
            let replacement = replacement.unwrap_or_else(|| {
                panic!("application name was not released after primary exit: {last_error:?}")
            });
            assert!(
                replacement.requests().is_empty(),
                "replacement primary must start with an empty queue; last error: {last_error:?}"
            );
        });
    }

    async fn receive_request(requests: &Receiver<DesktopRequest>) -> DesktopRequest {
        smol::future::race(
            async {
                requests
                    .recv()
                    .await
                    .expect("request channel must stay open")
            },
            async {
                smol::Timer::after(Duration::from_secs(3)).await;
                panic!("timed out waiting for a desktop request")
            },
        )
        .await
    }
}
