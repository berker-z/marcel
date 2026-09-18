use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use async_channel::{Receiver, Sender, TrySendError};
use url::Url;
use zbus::{self, message::Header, zvariant::OwnedValue};

use super::{
    file_chooser::{self, ReplyTracker},
    picker::PickerRequest,
};

pub const APPLICATION_ID: &str = "io.github.berker_z.Marcel";
pub const APPLICATION_OBJECT_PATH: &str = "/io/github/berker_z/Marcel";
pub const FILE_MANAGER_BUS_NAME: &str = "org.freedesktop.FileManager1";
pub const FILE_MANAGER_OBJECT_PATH: &str = "/org/freedesktop/FileManager1";
pub const CLAIM_FILE_MANAGER_ENV: &str = "MARCEL_CLAIM_FILE_MANAGER1";

const MAX_REQUEST_URIS: usize = 64;
const MAX_REQUEST_URI_BYTES: usize = 64 * 1024;
const REQUEST_QUEUE_CAPACITY: usize = 32;

/// How long a launch waits for the running Marcel to take its request.
///
/// The primary validates every path with `metadata` and `canonicalize`
/// before it answers, and on a hung NFS or FUSE mount that never returns.
/// Without a bound the launcher sat there with no window and no message.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

/// Requests one bus peer may make per minute on the request interfaces.
///
/// Every peer on the session bus may ask for a window, and a Flatpak with
/// `--talk-name=org.freedesktop.FileManager1` is a realistic one. A person
/// clicks "show in folder" a few times a minute; thirty leaves room for a
/// launcher opening a batch and still keeps one peer from filling the
/// screen with windows.
pub(crate) const REQUESTS_PER_MINUTE: usize = 30;
pub(crate) const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// Peers remembered at once; past this the quietest is forgotten.
const RATE_LIMIT_TRACKED_SENDERS: usize = 256;

/// How often, and how long, a runtime tries to get back on the bus.
const RECONNECT_ATTEMPTS: u32 = 10;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

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

impl DesktopRequest {
    /// Whether this request may take over a window the user is already using.
    ///
    /// A launch may not. Somebody ran `marcel`, or picked Marcel to open a
    /// folder; they asked for Marcel to show them something, and answering by
    /// navigating the window they were reading loses their place and gives
    /// them nothing extra.
    ///
    /// A reveal may. "Show me where this file is" is a request about a view
    /// that already exists — it is the one case where reusing the window in
    /// front of the user is the answer rather than a shortcut. Properties is
    /// a dialog, and a dialog goes on the window the user is looking at.
    pub fn may_reuse_a_window(&self) -> bool {
        matches!(self, Self::ShowItems(_) | Self::ShowFolders(_) | Self::ShowItemProperties(_))
    }
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
    connection: zbus::Connection,
    roles: BusRoles,
    services: Services,
    requests: Receiver<DesktopRequest>,
    pickers: Receiver<PickerRequest>,
}

impl DesktopRuntime {
    pub fn requests(&self) -> Receiver<DesktopRequest> {
        self.requests.clone()
    }

    /// File-chooser requests from the portal frontend. Empty for the life of
    /// the process unless this instance was asked to be the portal backend.
    pub fn pickers(&self) -> Receiver<PickerRequest> {
        self.pickers.clone()
    }

    /// File-chooser answers still on their way to the bus; see [`ReplyTracker`].
    pub fn replies(&self) -> ReplyTracker {
        self.services.replies.clone()
    }

    /// Hold the bus connection, and rebuild it if the bus drops it.
    ///
    /// Resolves only once the runtime has given up, saying why. A dropped
    /// connection used to go unnoticed: the request receivers stayed open
    /// with nothing feeding them, and "show in folder" silently did nothing
    /// for the rest of the session. The bus itself surviving a disconnect is
    /// unusual but real — `dbus-daemon` restarted at the same socket path, a
    /// bus that closed the connection over a protocol violation — and the
    /// channel senders live here rather than in the services, so a fresh
    /// connection feeds the same receivers the application already reads.
    pub async fn serve_until_lost(mut self) -> String {
        loop {
            self.connection.closed().await;
            eprintln!("Marcel lost its session bus connection; reconnecting");
            match self.reconnect().await {
                Ok(connection) => {
                    eprintln!("Marcel is back on the session bus");
                    self.connection = connection;
                }
                Err(reason) => return reason,
            }
        }
    }

    async fn reconnect(&self) -> Result<zbus::Connection, String> {
        let mut error = match connect(&self.services, self.roles).await {
            Ok(connection) => return Ok(connection),
            Err(error) => error,
        };
        for _ in 1..RECONNECT_ATTEMPTS {
            smol::Timer::after(RECONNECT_DELAY).await;
            match connect(&self.services, self.roles).await {
                Ok(connection) => return Ok(connection),
                Err(next) => error = next,
            }
        }
        Err(match error {
            // The bus may still list the dead connection as the owner for
            // a moment, which is why `NameTaken` is retried like any other
            // failure; held for the whole run, it means another Marcel took
            // the name meanwhile, and requests go there now.
            zbus::Error::NameTaken => {
                format!("another Marcel now owns {APPLICATION_ID}; desktop requests go to it")
            }
            error => format!(
                "could not get back on the session bus after {RECONNECT_ATTEMPTS} attempts: {error}"
            ),
        })
    }
}

/// What the bus services hand requests to, kept apart from any one
/// connection so a reconnect can serve the same channels again.
#[derive(Clone)]
struct Services {
    requests: Sender<DesktopRequest>,
    pickers: Sender<PickerRequest>,
    replies: ReplyTracker,
    /// One budget for both request interfaces: a peer that is refused on
    /// `FileManager1` must not get a fresh allowance on `Application`.
    limiter: SharedRateLimiter,
}

pub(crate) type SharedRateLimiter = Arc<Mutex<RateLimiter>>;

/// Requests admitted per sender in a sliding window.
///
/// Keyed by the unique connection name from the message header, which the
/// bus fills in and a peer cannot forge. Nothing here knows about time
/// passing on its own; the caller says what "now" is, so the tests can too.
pub(crate) struct RateLimiter {
    limit: usize,
    window: Duration,
    senders: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    pub(crate) fn new(limit: usize, window: Duration) -> Self {
        Self { limit, window, senders: HashMap::new() }
    }

    pub(crate) fn shared(limit: usize, window: Duration) -> SharedRateLimiter {
        Arc::new(Mutex::new(Self::new(limit, window)))
    }

    /// Whether `sender` may make one more request at `now`.
    pub(crate) fn admit(&mut self, sender: &str, now: Instant) -> bool {
        let window = self.window;
        // Forgetting quiet senders on every call keeps the map from growing
        // with names that connected once and left.
        self.senders.retain(|_, times| {
            while times.front().is_some_and(|time| now.duration_since(*time) >= window) {
                times.pop_front();
            }
            !times.is_empty()
        });
        if !self.senders.contains_key(sender) && self.senders.len() >= RATE_LIMIT_TRACKED_SENDERS {
            let quietest = self
                .senders
                .iter()
                .min_by_key(|(_, times)| times.back().copied())
                .map(|(name, _)| name.clone());
            if let Some(name) = quietest {
                self.senders.remove(&name);
            }
        }
        let times = self.senders.entry(sender.to_string()).or_default();
        if times.len() >= self.limit {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// Admit the request in `header`, or say why not.
pub(crate) fn admit(limiter: &Mutex<RateLimiter>, header: &Header<'_>) -> zbus::fdo::Result<()> {
    let sender = header.sender().map(|name| name.as_str()).unwrap_or_default();
    let mut limiter = limiter.lock().unwrap_or_else(PoisonError::into_inner);
    if limiter.admit(sender, Instant::now()) {
        return Ok(());
    }
    Err(zbus::fdo::Error::LimitsExceeded(format!(
        "{sender} has asked Marcel more than {} times in the last minute; try again later",
        limiter.limit
    )))
}

/// The optional session-bus roles an instance may take on top of its own name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusRoles {
    /// Answer `org.freedesktop.FileManager1` ("show in folder").
    pub file_manager: bool,
    /// Answer `org.freedesktop.impl.portal.FileChooser` (open and save dialogs).
    pub file_chooser: bool,
}

impl BusRoles {
    pub fn from_environment() -> Self {
        Self {
            file_manager: std::env::var_os(CLAIM_FILE_MANAGER_ENV).is_some(),
            file_chooser: std::env::var_os(file_chooser::CLAIM_FILE_CHOOSER_ENV).is_some(),
        }
    }
}

#[derive(Clone)]
struct ApplicationService {
    requests: Sender<DesktopRequest>,
    limiter: SharedRateLimiter,
}

#[derive(Clone)]
struct FileManagerService {
    requests: Sender<DesktopRequest>,
    limiter: SharedRateLimiter,
}

pub async fn acquire_or_forward(initial_uris: Option<Vec<String>>) -> InstanceStartup {
    acquire_or_forward_with_roles(initial_uris, BusRoles::from_environment()).await
}

async fn acquire_or_forward_with_roles(
    initial_uris: Option<Vec<String>>,
    roles: BusRoles,
) -> InstanceStartup {
    let (sender, receiver) = async_channel::bounded(REQUEST_QUEUE_CAPACITY);
    let (picker_sender, picker_receiver) = file_chooser::request_channel();
    let services = Services {
        requests: sender,
        pickers: picker_sender,
        replies: ReplyTracker::default(),
        limiter: RateLimiter::shared(REQUESTS_PER_MINUTE, RATE_LIMIT_WINDOW),
    };
    match connect(&services, roles).await {
        Ok(connection) => InstanceStartup::Primary(DesktopRuntime {
            connection,
            roles,
            services,
            requests: receiver,
            pickers: picker_receiver,
        }),
        Err(zbus::Error::NameTaken) => {
            match forward_to_primary(initial_uris, FORWARD_TIMEOUT).await {
                Ok(()) => InstanceStartup::Forwarded,
                Err(error) => InstanceStartup::Unavailable(describe_forward_failure(&error)),
            }
        }
        Err(error) => InstanceStartup::Unavailable(error.to_string()),
    }
}

/// Connect to the session bus as the primary Marcel: own the application
/// name, serve the request interfaces, and take on the opt-in roles.
async fn connect(services: &Services, roles: BusRoles) -> zbus::Result<zbus::Connection> {
    let application = ApplicationService {
        requests: services.requests.clone(),
        limiter: services.limiter.clone(),
    };
    let file_manager = FileManagerService {
        requests: services.requests.clone(),
        limiter: services.limiter.clone(),
    };
    let connection = zbus::connection::Builder::session()?
        .serve_at(APPLICATION_OBJECT_PATH, application)?
        .serve_at(FILE_MANAGER_OBJECT_PATH, file_manager)?
        .name(APPLICATION_ID)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .build()
        .await?;
    // Only after the application name is owned: the generic name is an
    // opt-in extra, never a startup condition. Requesting both through
    // the builder conflated them — another file manager owning
    // `org.freedesktop.FileManager1` made `build()` fail with
    // `NameTaken`, which reads as "another Marcel is running" and
    // forwards the launch to an application name nobody owns,
    // re-activating another Marcel that fails the same way.
    if roles.file_manager {
        claim_generic_file_manager_name(&connection).await;
    }
    // Same rule for the portal backend name: an extra, not a
    // condition. A failure here leaves a Marcel that browses files
    // and cannot show pickers, which beats no Marcel at all.
    if roles.file_chooser
        && let Err(error) =
            file_chooser::serve(&connection, services.pickers.clone(), services.replies.clone())
                .await
    {
        eprintln!("could not serve {}: {error}", file_chooser::FILE_CHOOSER_BUS_NAME);
    }
    Ok(connection)
}

/// Why a launch could not be handed to the running Marcel, for the user.
fn describe_forward_failure(error: &zbus::Error) -> String {
    match error {
        zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => format!(
            "the running Marcel did not answer within {} seconds; opening a window here instead",
            FORWARD_TIMEOUT.as_secs()
        ),
        other => format!("could not hand the request to the running Marcel: {other}"),
    }
}

/// Try to own the generic file-manager name, continuing without it when
/// another file manager already does.
async fn claim_generic_file_manager_name(connection: &zbus::Connection) {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};

    match connection
        .request_name_with_flags(FILE_MANAGER_BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) => eprintln!(
            "another file manager owns {FILE_MANAGER_BUS_NAME}; Marcel keeps running without it"
        ),
        Err(error) => eprintln!("could not request {FILE_MANAGER_BUS_NAME}: {error}"),
    }
}

async fn forward_to_primary(
    initial_uris: Option<Vec<String>>,
    timeout: Duration,
) -> zbus::Result<()> {
    let connection = zbus::connection::Builder::session()?.method_timeout(timeout).build().await?;
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

#[zbus::interface(interface = "org.freedesktop.Application")]
impl ApplicationService {
    async fn activate(
        &self,
        #[zbus(header)] header: Header<'_>,
        _platform_data: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        admit(&self.limiter, &header)?;
        enqueue(&self.requests, DesktopRequest::Activate)
    }

    async fn open(
        &self,
        #[zbus(header)] header: Header<'_>,
        uris: Vec<String>,
        _platform_data: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        admit(&self.limiter, &header)?;
        validate_and_enqueue(&self.requests, UriRequestKind::Open, uris).await
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

#[zbus::interface(interface = "org.freedesktop.FileManager1")]
impl FileManagerService {
    async fn show_folders(
        &self,
        #[zbus(header)] header: Header<'_>,
        uris: Vec<String>,
        _startup_id: String,
    ) -> zbus::fdo::Result<()> {
        admit(&self.limiter, &header)?;
        validate_and_enqueue(&self.requests, UriRequestKind::ShowFolders, uris).await
    }

    async fn show_items(
        &self,
        #[zbus(header)] header: Header<'_>,
        uris: Vec<String>,
        _startup_id: String,
    ) -> zbus::fdo::Result<()> {
        admit(&self.limiter, &header)?;
        validate_and_enqueue(&self.requests, UriRequestKind::ShowItems, uris).await
    }

    async fn show_item_properties(
        &self,
        #[zbus(header)] header: Header<'_>,
        uris: Vec<String>,
        _startup_id: String,
    ) -> zbus::fdo::Result<()> {
        admit(&self.limiter, &header)?;
        validate_and_enqueue(&self.requests, UriRequestKind::ShowItemProperties, uris).await
    }
}

/// Validate a URI request off the bus thread, then queue it.
async fn validate_and_enqueue(
    sender: &Sender<DesktopRequest>,
    kind: UriRequestKind,
    uris: Vec<String>,
) -> zbus::fdo::Result<()> {
    let request = smol::unblock(move || validate_uri_request(kind, &uris))
        .await
        .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
    enqueue(sender, request)
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

pub fn validate_uri_request(kind: UriRequestKind, uris: &[String]) -> Result<DesktopRequest> {
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

fn validate_request_bounds(uris: &[String]) -> Result<()> {
    if uris.is_empty() {
        bail!("the request must contain at least one URI");
    }
    if uris.len() > MAX_REQUEST_URIS {
        bail!("the request contains more than {MAX_REQUEST_URIS} URIs");
    }

    let total_bytes = uris.iter().try_fold(0usize, |total, uri| {
        total
            .checked_add(uri.len())
            .ok_or_else(|| anyhow!("the request URI size exceeds the supported limit"))
    })?;
    if total_bytes > MAX_REQUEST_URI_BYTES {
        bail!("the request URI size exceeds {MAX_REQUEST_URI_BYTES} bytes");
    }

    Ok(())
}

fn local_path_from_uri(uri: &str) -> Result<PathBuf> {
    let parsed = Url::parse(uri).map_err(|_| anyhow!("invalid URI: {uri}"))?;
    if parsed.scheme() != "file" {
        bail!("unsupported URI scheme in {uri}");
    }

    parsed.to_file_path().map_err(|_| anyhow!("non-local file URI: {uri}"))
}

fn require_existing(path: &Path) -> Result<PathBuf> {
    fs::metadata(path).map_err(|error| anyhow!("cannot inspect {}: {error}", path.display()))?;
    Ok(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn require_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::metadata(path)
        .map_err(|error| anyhow!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn group_open_targets(paths: Vec<PathBuf>) -> Result<Vec<RevealedLocation>> {
    let mut locations = Vec::with_capacity(paths.len());
    for path in paths {
        let path = require_existing(&path)?;
        if path.is_dir() {
            locations.push(RevealedLocation { directory: path, items: Vec::new() });
        } else {
            let directory = path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
            locations.push(RevealedLocation { directory, items: vec![path] });
        }
    }
    Ok(locations)
}

fn group_revealed_items(paths: Vec<PathBuf>) -> Result<Vec<RevealedLocation>> {
    let mut locations = Vec::<RevealedLocation>::new();
    let mut parent_indices = HashMap::<PathBuf, usize>::new();

    for path in paths {
        let path = require_existing(&path)?;
        let directory = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;

        if let Some(index) = parent_indices.get(&directory).copied() {
            locations[index].items.push(path);
        } else {
            parent_indices.insert(directory.clone(), locations.len());
            locations.push(RevealedLocation { directory, items: vec![path] });
        }
    }

    Ok(locations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desktop::picker::{PickerMode, PickerResponse},
        testing::Sandbox,
    };
    use std::{process::Command, time::Duration};

    /// The defect this rule exists to prevent: running `marcel` while Marcel is
    /// already open navigated the window the user was reading, or — with no
    /// argument at all — raised it and ignored the folder they were standing in.
    #[test]
    fn a_launch_never_takes_over_a_window_and_a_reveal_may() {
        let location =
            || RevealedLocation { directory: PathBuf::from("/folder"), items: Vec::new() };

        assert!(!DesktopRequest::Open(vec![location()]).may_reuse_a_window());
        assert!(!DesktopRequest::Activate.may_reuse_a_window());
        assert!(DesktopRequest::ShowItems(vec![location()]).may_reuse_a_window());
        assert!(DesktopRequest::ShowFolders(vec![PathBuf::from("/folder")]).may_reuse_a_window());
        assert!(
            DesktopRequest::ShowItemProperties(vec![PathBuf::from("/folder/file")])
                .may_reuse_a_window()
        );
    }

    const PRIVATE_BUS_CHILD: &str = "MARCEL_PRIVATE_BUS_TEST_CHILD";
    const PRIVATE_BUS_CONFIG: &str = "MARCEL_TEST_DBUS_SESSION_CONFIG";
    const FILE_MANAGER_ROLE: BusRoles = BusRoles { file_manager: true, file_chooser: false };
    const FILE_CHOOSER_ROLE: BusRoles = BusRoles { file_manager: false, file_chooser: true };

    fn uri(path: &Path) -> String {
        Url::from_file_path(path).unwrap().into()
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    fn revealed(directory: &Path, items: &[&Path]) -> RevealedLocation {
        RevealedLocation {
            directory: canonical(directory),
            items: items.iter().map(|item| canonical(item)).collect(),
        }
    }

    #[test]
    fn show_folders_accepts_directories_and_rejects_regular_files() {
        let sandbox = Sandbox::new();
        let folder = sandbox.dir("folder");
        let file = sandbox.file("file.txt", b"hello");

        assert_eq!(
            validate_uri_request(UriRequestKind::ShowFolders, &[uri(&folder)]).unwrap(),
            DesktopRequest::ShowFolders(vec![canonical(&folder)])
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
        let sandbox = Sandbox::new();
        let first = sandbox.file("first/one", b"1");
        let second = sandbox.file("second/two", b"2");
        let third = sandbox.file("first/three", b"3");

        assert_eq!(
            validate_uri_request(
                UriRequestKind::ShowItems,
                &[uri(&first), uri(&second), uri(&third)],
            )
            .unwrap(),
            DesktopRequest::ShowItems(vec![
                revealed(&sandbox.path("first"), &[&first, &third]),
                revealed(&sandbox.path("second"), &[&second]),
            ])
        );
    }

    #[test]
    fn open_keeps_each_requested_location() {
        let sandbox = Sandbox::new();
        let folder = sandbox.dir("folder");
        let file = sandbox.file("file.txt", b"hello");

        assert_eq!(
            validate_uri_request(UriRequestKind::Open, &[uri(&folder), uri(&file)]).unwrap(),
            DesktopRequest::Open(vec![revealed(&folder, &[]), revealed(sandbox.root(), &[&file]),])
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
        assert_eq!(format!("/{}", APPLICATION_ID.replace('.', "/")), APPLICATION_OBJECT_PATH);
    }

    #[test]
    fn a_sender_is_refused_past_its_budget_and_admitted_once_the_window_moves_on() {
        let mut limiter = RateLimiter::new(3, Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.admit(":1.7", start));
        assert!(limiter.admit(":1.7", start + Duration::from_secs(10)));
        assert!(limiter.admit(":1.7", start + Duration::from_secs(20)));
        assert!(!limiter.admit(":1.7", start + Duration::from_secs(30)));
        // A refusal does not count against the budget, so the first request
        // ages out on schedule and the peer gets one back.
        assert!(!limiter.admit(":1.7", start + Duration::from_secs(59)));
        assert!(limiter.admit(":1.7", start + Duration::from_secs(60)));
        assert!(!limiter.admit(":1.7", start + Duration::from_secs(61)));
    }

    #[test]
    fn budgets_are_per_sender() {
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        let now = Instant::now();
        assert!(limiter.admit(":1.7", now));
        assert!(!limiter.admit(":1.7", now));
        assert!(limiter.admit(":1.8", now));
        assert!(!limiter.admit(":1.8", now));
    }

    #[test]
    fn quiet_senders_are_forgotten_and_the_table_stays_bounded() {
        let mut limiter = RateLimiter::new(2, Duration::from_secs(60));
        let start = Instant::now();
        for peer in 0..RATE_LIMIT_TRACKED_SENDERS * 2 {
            assert!(
                limiter.admit(&format!(":1.{peer}"), start + Duration::from_millis(peer as u64))
            );
        }
        assert!(limiter.senders.len() <= RATE_LIMIT_TRACKED_SENDERS);
        // Being forgotten is not the same as being reset: the peer whose
        // entry was evicted was the quietest, and the newest is still held.
        let newest = format!(":1.{}", RATE_LIMIT_TRACKED_SENDERS * 2 - 1);
        assert!(limiter.senders.contains_key(&newest));
        assert!(!limiter.senders.contains_key(":1.0"));

        assert!(limiter.admit(":1.0", start + Duration::from_secs(61)));
        assert_eq!(limiter.senders.len(), 1, "everything older than the window is gone");
    }

    #[test]
    fn a_forwarding_timeout_is_explained_as_the_primary_not_answering() {
        let timeout =
            zbus::Error::InputOutput(Arc::new(std::io::Error::from(std::io::ErrorKind::TimedOut)));
        let message = describe_forward_failure(&timeout);
        assert!(message.contains("did not answer within 10 seconds"), "{message}");
        assert!(message.contains("opening a window here"), "{message}");
        let other = describe_forward_failure(&zbus::Error::NameTaken);
        assert!(other.contains("could not hand the request"), "{other}");
    }

    #[test]
    fn private_session_bus_integration() {
        if std::env::var_os(PRIVATE_BUS_CHILD).is_some() {
            return;
        }

        // libtest names tests without the crate, which `module_path!`
        // includes; with it, `--exact` matched nothing and the child passed
        // by running no test at all.
        let module = module_path!()
            .strip_prefix(concat!(env!("CARGO_PKG_NAME"), "::"))
            .unwrap_or(module_path!());
        let mut command = Command::new("dbus-run-session");
        if let Some(config) = std::env::var_os(PRIVATE_BUS_CONFIG) {
            command.arg("--config-file").arg(config);
        }
        let output = command
            .arg("--")
            .arg(std::env::current_exe().expect("test executable must have a path"))
            .arg("--exact")
            .arg(format!("{module}::private_session_bus_child"))
            .arg("--nocapture")
            .env(PRIVATE_BUS_CHILD, "1")
            .output()
            .expect("dbus-run-session must be available in Marcel's development environment");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "private session-bus child failed:\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "the child must have run exactly one test:\n{stdout}\n{stderr}"
        );
    }

    /// Wait for the bus to release the application name, then own it.
    async fn become_primary(roles: BusRoles) -> Result<DesktopRuntime, Option<String>> {
        let mut last_error = None;
        for _ in 0..80 {
            match acquire_or_forward_with_roles(None, roles).await {
                InstanceStartup::Primary(runtime) => return Ok(runtime),
                InstanceStartup::Forwarded => {}
                InstanceStartup::Unavailable(error) => last_error = Some(error),
            }
            smol::Timer::after(Duration::from_millis(25)).await;
        }
        Err(last_error)
    }

    async fn owns_name(bus: &zbus::fdo::DBusProxy<'_>, name: &str) -> bool {
        bus.list_names()
            .await
            .expect("bus names must be readable")
            .iter()
            .any(|owned| owned.as_str() == name)
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
            let client =
                zbus::Connection::session().await.expect("client must connect to the private bus");
            let bus = zbus::fdo::DBusProxy::new(&client).await.expect("bus proxy must initialize");
            assert!(
                owns_name(&bus, APPLICATION_ID).await,
                "primary must own Marcel's application name"
            );
            assert!(
                !owns_name(&bus, FILE_MANAGER_BUS_NAME).await,
                "the ordinary process must not claim the generic file-manager name"
            );

            let sandbox = Sandbox::new();
            let file = sandbox.file("folder/file.txt", b"hello");
            let location = revealed(&sandbox.path("folder"), &[&file]);

            assert!(matches!(
                acquire_or_forward(Some(vec![uri(&file)])).await,
                InstanceStartup::Forwarded
            ));
            assert_eq!(receive(&requests).await, DesktopRequest::Open(vec![location.clone()]));

            let proxy = |name: &'static str, path: &'static str, interface: &'static str| {
                zbus::Proxy::new(&client, name, path, interface)
            };
            let application =
                proxy(APPLICATION_ID, APPLICATION_OBJECT_PATH, "org.freedesktop.Application")
                    .await
                    .expect("application proxy must initialize");
            application
                .call::<_, _, ()>("Activate", &(HashMap::<String, OwnedValue>::new(),))
                .await
                .expect("warm activation must succeed");
            assert_eq!(receive(&requests).await, DesktopRequest::Activate);

            let file_manager =
                proxy(APPLICATION_ID, FILE_MANAGER_OBJECT_PATH, "org.freedesktop.FileManager1")
                    .await
                    .expect("file-manager proxy must initialize");
            let invalid: zbus::Result<()> =
                file_manager.call("ShowFolders", &(vec![uri(&file)], String::new())).await;
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
            assert_eq!(receive(&requests).await, DesktopRequest::ShowItems(vec![location]));

            // One peer gets a budget, not the run of the session. The
            // budget is per unique name, so a flooding peer does not use up
            // the client's.
            let flooder = zbus::Connection::session().await.expect("flooding peer must connect");
            let flooding_application = zbus::Proxy::new(
                &flooder,
                APPLICATION_ID,
                APPLICATION_OBJECT_PATH,
                "org.freedesktop.Application",
            )
            .await
            .expect("flooding proxy must initialize");
            for _ in 0..REQUESTS_PER_MINUTE {
                flooding_application
                    .call::<_, _, ()>("Activate", &(HashMap::<String, OwnedValue>::new(),))
                    .await
                    .expect("requests within the budget must be taken");
                assert_eq!(receive(&requests).await, DesktopRequest::Activate);
            }
            let refused: zbus::Result<()> = flooding_application
                .call("Activate", &(HashMap::<String, OwnedValue>::new(),))
                .await;
            match refused.expect_err("the request past the budget must be refused") {
                zbus::Error::MethodError(name, _, _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.LimitsExceeded")
                }
                error => panic!("expected a typed LimitsExceeded reply, got {error}"),
            }
            let flooder_file_manager = zbus::Proxy::new(
                &flooder,
                APPLICATION_ID,
                FILE_MANAGER_OBJECT_PATH,
                "org.freedesktop.FileManager1",
            )
            .await
            .expect("flooding file-manager proxy must initialize");
            let refused: zbus::Result<()> =
                flooder_file_manager.call("ShowItems", &(vec![uri(&file)], String::new())).await;
            assert!(
                matches!(refused, Err(zbus::Error::MethodError(ref name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.LimitsExceeded"),
                "the budget is shared by both request interfaces"
            );
            assert!(requests.is_empty(), "refused requests must not be queued");
            application
                .call::<_, _, ()>("Activate", &(HashMap::<String, OwnedValue>::new(),))
                .await
                .expect("another peer's flood must not cost the client its budget");
            assert_eq!(receive(&requests).await, DesktopRequest::Activate);
            drop(flooder);

            // Losing the connection is survived: the runtime comes back on
            // the bus and the same receiver keeps getting requests.
            let old_connection = primary.connection.clone();
            let old_name = old_connection.unique_name().expect("primary has a unique name").clone();
            let watcher = smol::spawn(primary.serve_until_lost());
            old_connection.close().await.expect("closing the primary's socket");
            let marcel_name = zbus::names::BusName::try_from(APPLICATION_ID).unwrap();
            let mut reclaimed = false;
            for _ in 0..200 {
                if let Ok(owner) = bus.get_name_owner(marcel_name.clone()).await
                    && owner.as_str() != old_name.as_str()
                {
                    reclaimed = true;
                    break;
                }
                smol::Timer::after(Duration::from_millis(25)).await;
            }
            assert!(reclaimed, "the runtime must reclaim its name after a disconnect");
            application
                .call::<_, _, ()>("Activate", &(HashMap::<String, OwnedValue>::new(),))
                .await
                .expect("the reconnected primary must answer");
            assert_eq!(receive(&requests).await, DesktopRequest::Activate);

            // Dropping the watcher drops the runtime and, with it, the name.
            drop(watcher);
            let replacement = become_primary(FILE_MANAGER_ROLE).await.unwrap_or_else(|error| {
                panic!("application name was not released after primary exit: {error:?}")
            });
            assert!(
                replacement.requests().is_empty(),
                "replacement primary must start with an empty queue"
            );
            assert!(
                owns_name(&bus, FILE_MANAGER_BUS_NAME).await,
                "the opt-in primary must own the generic file-manager name"
            );

            // Another file manager owning the generic name must not read as
            // "another Marcel is running": that misreading forwarded the
            // launch to an application name nobody owned, re-activating
            // another Marcel that failed the same way.
            drop(replacement);
            let foreign =
                zbus::Connection::session().await.expect("foreign file manager must connect");
            foreign
                .request_name(FILE_MANAGER_BUS_NAME)
                .await
                .expect("foreign file manager must own the generic name");
            let standalone = become_primary(FILE_MANAGER_ROLE).await.unwrap_or_else(|_| {
                panic!("Marcel must start as primary while another file manager owns {FILE_MANAGER_BUS_NAME}")
            });
            let owner = bus
                .get_name_owner(zbus::names::BusName::try_from(FILE_MANAGER_BUS_NAME).unwrap())
                .await
                .expect("the generic name must stay owned");
            assert_eq!(
                owner.as_str(),
                foreign.unique_name().expect("foreign connection has a unique name").as_str(),
                "Marcel must not have displaced the owner of the generic name"
            );

            // The portal backend role: the frontend's method call blocks
            // until a window answers, and its `Close` withdraws the request.
            drop(standalone);
            let backend = become_primary(FILE_CHOOSER_ROLE)
                .await
                .expect("the portal backend variant must start as primary");
            let pickers = backend.pickers();
            assert!(
                owns_name(&bus, file_chooser::FILE_CHOOSER_BUS_NAME).await,
                "the opt-in primary must own the portal backend name"
            );
            let chooser = proxy(
                file_chooser::FILE_CHOOSER_BUS_NAME,
                file_chooser::PORTAL_OBJECT_PATH,
                "org.freedesktop.impl.portal.FileChooser",
            )
            .await
            .expect("file-chooser proxy must initialize");
            let version: u32 = chooser
                .get_property("version")
                .await
                .expect("the backend must advertise its interface version");
            assert_eq!(version, 4);

            type ChooserReply = (u32, HashMap<String, OwnedValue>);
            let call_open_file = |handle: &str| {
                let chooser = chooser.clone();
                let handle =
                    zbus::zvariant::OwnedObjectPath::try_from(handle).expect("valid handle path");
                smol::spawn(async move {
                    chooser
                        .call::<_, _, ChooserReply>(
                            "OpenFile",
                            &(
                                handle,
                                "org.example.App",
                                "",
                                "Pick something",
                                HashMap::<String, OwnedValue>::new(),
                            ),
                        )
                        .await
                })
            };

            // Answered by a window.
            let call = call_open_file("/org/freedesktop/portal/desktop/request/test/1");
            let request = receive(&pickers).await;
            assert_eq!(backend.replies().pending(), 1);
            assert_eq!(request.title, "Pick something");
            assert_eq!(request.mode, PickerMode::OpenFiles);
            request
                .reply
                .send(PickerResponse::Chosen { paths: vec![file.clone()], filter: None })
                .await
                .expect("the backend must still be waiting for the answer");
            let (code, results) = call.await.expect("OpenFile must succeed");
            assert_eq!(code, file_chooser::RESPONSE_SUCCESS);
            assert_eq!(Vec::<String>::try_from(results["uris"].clone()).unwrap(), vec![uri(&file)]);
            // The reply has reached the client, so the backend must stop
            // counting it; the quit hook waits on exactly this.
            for _ in 0..200 {
                if backend.replies().pending() == 0 {
                    break;
                }
                smol::Timer::after(Duration::from_millis(5)).await;
            }
            assert_eq!(backend.replies().pending(), 0);

            // Withdrawn by the frontend before the window answered. The
            // Request object exists for exactly as long as the call.
            let handle = "/org/freedesktop/portal/desktop/request/test/2";
            let call = call_open_file(handle);
            let request = receive(&pickers).await;
            let request_object = proxy(
                file_chooser::FILE_CHOOSER_BUS_NAME,
                handle,
                "org.freedesktop.impl.portal.Request",
            )
            .await
            .expect("request proxy must initialize");
            request_object
                .call::<_, _, ()>("Close", &())
                .await
                .expect("Close must reach the request object while the call is pending");
            request.closed.recv().await.expect("the window must be told the request was withdrawn");
            request
                .reply
                .send(PickerResponse::Closed)
                .await
                .expect("the backend must still be waiting");
            let (code, _) = call.await.expect("a withdrawn OpenFile still replies");
            assert_eq!(code, file_chooser::RESPONSE_OTHER);
            assert!(
                request_object.call::<_, _, ()>("Close", &()).await.is_err(),
                "the request object must be gone once the call has replied"
            );

            // A request nobody ever showed — the window failed to open — is
            // reported as an error, not as the user cancelling.
            let call = call_open_file("/org/freedesktop/portal/desktop/request/test/3");
            let request = receive(&pickers).await;
            drop(request);
            let (code, _) = call.await.expect("a dropped request still replies");
            assert_eq!(code, file_chooser::RESPONSE_OTHER);

            // A second request on a handle that is still pending is refused,
            // and the first keeps its request object: `Close` still reaches
            // it. Removing the object for the refused one used to take that
            // path away from the request that owned it.
            let handle = "/org/freedesktop/portal/desktop/request/test/4";
            let call = call_open_file(handle);
            let request = receive(&pickers).await;
            match call_open_file(handle).await.expect_err("a duplicate handle must be refused") {
                zbus::Error::MethodError(name, _, _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs")
                }
                error => panic!("expected a typed InvalidArgs reply, got {error}"),
            }
            assert!(pickers.is_empty(), "the duplicate must never reach a window");
            let request_object = proxy(
                file_chooser::FILE_CHOOSER_BUS_NAME,
                handle,
                "org.freedesktop.impl.portal.Request",
            )
            .await
            .expect("request proxy must initialize");
            request_object
                .call::<_, _, ()>("Close", &())
                .await
                .expect("the first request's object must still be served");
            request.closed.recv().await.expect("the first request is told it was withdrawn");
            request.reply.send(PickerResponse::Closed).await.expect("the backend is waiting");
            let (code, _) = call.await.expect("the first request still replies");
            assert_eq!(code, file_chooser::RESPONSE_OTHER);

            // A title that would abort the compositor connection is refused
            // before any window sees it.
            let long_title = "x".repeat(8192);
            let refused: zbus::Result<ChooserReply> = chooser
                .call(
                    "OpenFile",
                    &(
                        zbus::zvariant::OwnedObjectPath::try_from(
                            "/org/freedesktop/portal/desktop/request/test/5",
                        )
                        .unwrap(),
                        "org.example.App",
                        "",
                        long_title.as_str(),
                        HashMap::<String, OwnedValue>::new(),
                    ),
                )
                .await;
            match refused.expect_err("an over-long title must be refused") {
                zbus::Error::MethodError(name, _, _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs")
                }
                error => panic!("expected a typed InvalidArgs reply, got {error}"),
            }
            assert!(pickers.is_empty());
            assert_eq!(backend.replies().pending(), 0, "a refused request is never pending");

            // One peer's dialogs are budgeted too. The client has spent a
            // few already; the rest are spent here until one is refused.
            let mut refused_within_budget = None;
            for index in 0..=file_chooser::PICKERS_PER_MINUTE {
                let handle = format!("/org/freedesktop/portal/desktop/request/test/budget{index}");
                let mut call = call_open_file(&handle);
                // Either a window is asked, or the call comes back refused;
                // borrowing the task keeps it alive whichever way it goes.
                let step =
                    smol::future::race(async { Step::Asked(receive(&pickers).await) }, async {
                        Step::Replied((&mut call).await)
                    })
                    .await;
                match step {
                    Step::Asked(request) => {
                        request
                            .reply
                            .send(PickerResponse::Cancelled)
                            .await
                            .expect("the backend is waiting");
                        let (code, _) = call.await.expect("an answered request replies");
                        assert_eq!(code, file_chooser::RESPONSE_CANCELLED);
                    }
                    Step::Replied(reply) => {
                        match reply.expect_err("a call that replied unasked was refused") {
                            zbus::Error::MethodError(name, _, _) => assert_eq!(
                                name.as_str(),
                                "org.freedesktop.DBus.Error.LimitsExceeded"
                            ),
                            error => panic!("expected a typed LimitsExceeded reply, got {error}"),
                        }
                        refused_within_budget = Some(index);
                        break;
                    }
                }
            }
            let refused_at = refused_within_budget.expect("the budget must run out");
            assert!(refused_at > 0 && refused_at < file_chooser::PICKERS_PER_MINUTE);

            // A primary that never answers — validating a path on a hung
            // mount — must not hang the launcher with it.
            drop(backend);
            let hanging = own_name_with(HangingApplication).await;
            let started = Instant::now();
            let error = forward_to_primary(None, Duration::from_millis(300))
                .await
                .expect_err("forwarding to a primary that never answers must time out");
            assert!(
                matches!(&error, zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut),
                "expected a timeout, got {error}"
            );
            assert!(started.elapsed() < Duration::from_secs(5), "the timeout must be the bound");
            drop(hanging);
        });
    }

    /// What happened first to a budgeted dialog request.
    enum Step {
        Asked(PickerRequest),
        Replied(zbus::Result<(u32, HashMap<String, OwnedValue>)>),
    }

    /// An application service whose methods never return.
    #[derive(Clone)]
    struct HangingApplication;

    #[zbus::interface(interface = "org.freedesktop.Application")]
    impl HangingApplication {
        async fn activate(&self, _platform_data: HashMap<String, OwnedValue>) {
            std::future::pending::<()>().await
        }
    }

    /// Own Marcel's application name with `service`, waiting for the bus to
    /// release it first.
    async fn own_name_with<S>(service: S) -> zbus::Connection
    where
        S: zbus::object_server::Interface + Clone,
    {
        for _ in 0..80 {
            let attempt = zbus::connection::Builder::session()
                .expect("session bus address")
                .serve_at(APPLICATION_OBJECT_PATH, service.clone())
                .expect("serve the application object")
                .name(APPLICATION_ID)
                .expect("valid application name")
                .build()
                .await;
            match attempt {
                Ok(connection) => return connection,
                Err(zbus::Error::NameTaken) => {
                    smol::Timer::after(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("could not own the application name: {error}"),
            }
        }
        panic!("the application name was never released");
    }

    /// The next message on `channel`, or a panic once the bus has clearly
    /// stopped delivering.
    async fn receive<T>(channel: &Receiver<T>) -> T {
        smol::future::race(async { channel.recv().await.expect("channel must stay open") }, async {
            smol::Timer::after(Duration::from_secs(3)).await;
            panic!("timed out waiting for a request")
        })
        .await
    }
}
