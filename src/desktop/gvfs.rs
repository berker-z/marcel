//! Network shares through GVfs, over the session bus: which are connected,
//! where their directories are, and the two calls that connect and
//! disconnect one.
//!
//! Marcel has no SFTP or SMB client of its own. GVfs's daemon owns the
//! connection, its backends speak the protocols, and `gvfsd-fuse` exposes
//! every mount as a directory under `/run/user/<uid>/gvfs/`, from where
//! `std::fs` and all of `fsops` work unchanged. This module speaks the
//! daemon's own D-Bus protocol (`common/gvfsdbus.xml` in GVfs), the one GIO's
//! client module speaks, because linking GIO for a dozen calls is not worth a
//! GLib main loop in a GPUI process.
//!
//! The protocol has two sides. Marcel calls `org.gtk.vfs.MountTracker` on
//! `org.gtk.vfs.Daemon` to list and mount. In return a mount can ask
//! questions: the daemon calls back into an `org.gtk.vfs.MountOperation`
//! object Marcel exports for the duration of the call, for a password, for
//! "trust this host?", and to say what is holding an unmount up. Those
//! arrive here as [`Prompt`]s on a channel and are answered by whoever holds
//! the other end; `NetworkStore` in `crate::network` puts them on screen.
//!
//! Nothing here knows about GPUI.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use percent_encoding::percent_decode_str;
use url::Url;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const DAEMON: &str = "org.gtk.vfs.Daemon";
const TRACKER_PATH: &str = "/org/gtk/vfs/mounttracker";
const TRACKER: &str = "org.gtk.vfs.MountTracker";
const MOUNT: &str = "org.gtk.vfs.Mount";
const OPERATION_PATH: &str = "/io/github/berker_z/Marcel/MountOperation";

// ---------------------------------------------------------------------------
// Mount specs: what GVfs calls a location, minus the path inside it.

/// The identity of a GVfs mount: which backend, and what it connects to.
///
/// GVfs keys every mount by one of these. Two URIs that differ only in the
/// path inside the share (`sftp://host/a` and `sftp://host/b`) are one mount,
/// so a saved server and a live mount are matched through their specs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MountSpec {
    /// The backend: `sftp`, `smb-share`, `dav`, `ftp`.
    pub kind: String,
    /// What the backend needs to connect (`host`, `user`, `port`, `server`,
    /// `share`, `ssl`), under the keys GVfs's own URI mappers use.
    pub items: BTreeMap<String, String>,
    /// The directory on the server the mount is rooted at, `/` for most.
    pub prefix: String,
}

impl MountSpec {
    fn new(kind: &str) -> Self {
        Self { kind: kind.to_string(), items: BTreeMap::new(), prefix: "/".to_string() }
    }

    fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.items.insert(key.to_string(), value);
        }
    }

    pub fn host(&self) -> Option<&str> {
        self.items.get("host").or_else(|| self.items.get("server")).map(String::as_str)
    }

    /// Whether a live mount answers for this spec: same backend, everything
    /// this spec asks for present and equal. The mount may know more (a
    /// backend can add the user it ended up as), and a DAV mount can settle
    /// on a shorter prefix than the one asked for once it has found where
    /// the server's DAV root is.
    pub fn is_served_by(&self, mount: &MountSpec) -> bool {
        self.kind == mount.kind
            && self.items.iter().all(|(key, value)| mount.items.get(key) == Some(value))
            && path_starts_with(&self.prefix, &mount.prefix)
    }

    /// The wire form, `(aya{sv})`: the prefix and every item as GLib
    /// bytestrings, NUL terminator included, with the backend as the `type`
    /// item the way `g_mount_spec_to_dbus` sends it.
    fn to_dbus(&self) -> (Vec<u8>, HashMap<String, Value<'static>>) {
        let bytestring = |text: &str| -> Value<'static> {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            Value::from(bytes)
        };
        let mut items: HashMap<String, Value<'static>> =
            self.items.iter().map(|(key, value)| (key.clone(), bytestring(value))).collect();
        items.insert("type".to_string(), bytestring(&self.kind));
        let mut prefix = self.prefix.as_bytes().to_vec();
        prefix.push(0);
        (prefix, items)
    }

    fn from_dbus(prefix: Vec<u8>, items: HashMap<String, OwnedValue>) -> Self {
        let mut spec = Self { kind: String::new(), items: BTreeMap::new(), prefix: text(prefix) };
        for (key, value) in items {
            let Ok(bytes) = Vec::<u8>::try_from(value) else {
                continue;
            };
            if key == "type" {
                spec.kind = text(bytes);
            } else {
                spec.items.insert(key, text(bytes));
            }
        }
        if spec.prefix.is_empty() {
            spec.prefix = "/".to_string();
        }
        spec
    }
}

/// Whether `path` is `prefix` or lies under it, component-wise, treating the
/// root prefix as matching everything.
fn path_starts_with(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || path.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&trim_nul(bytes)).into_owned()
}

/// GLib bytestrings carry their C terminator.
fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

// ---------------------------------------------------------------------------
// Locations: a spec plus a path inside it, from and to a URI.

/// A place on a server: the mount to reach and the directory inside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Location {
    pub spec: MountSpec,
    /// The directory inside the mount, as the server sees it (`/home/me`),
    /// or empty for "wherever the server puts me": the home directory over
    /// SFTP, the root of a share.
    pub path: String,
}

impl Location {
    /// What the user typed or saved: a URI with one of the schemes GVfs has a
    /// backend for here, or a bare `[user@]host[:port]`, which means SFTP
    /// because a shell user who types a hostname means `ssh`.
    ///
    /// The mapping from URI to spec follows GVfs's own: the generic rule in
    /// `client/gdaemonvfs.c` for anything with a host, `client/smburi.c` for
    /// `smb://`, and `client/httpuri.c` for `dav://`, so a URI Nautilus
    /// accepts lands on the same mount here.
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("Enter a server address such as sftp://host/ or smb://host/share".into());
        }
        let text = if input.contains("://") {
            input.to_string()
        } else if looks_like_host(input) {
            format!("sftp://{input}/")
        } else {
            return Err(format!("“{input}” is not a server address"));
        };
        let url = Url::parse(&text).map_err(|_| format!("“{input}” is not a valid address"))?;
        let host = url.host_str().map(str::to_string);
        let user = decode(url.username());
        let scheme = url.scheme().to_ascii_lowercase();
        let need_host = |kind: &str| -> Result<(MountSpec, String), String> {
            let host = host.clone().ok_or_else(|| format!("“{input}” names no host"))?;
            let mut spec = MountSpec::new(kind);
            spec.set("host", host);
            spec.set("user", user.clone());
            Ok((spec, decode(url.path())))
        };
        let (mut spec, path) = match scheme.as_str() {
            "sftp" | "ssh" => {
                let (mut spec, path) = need_host("sftp")?;
                if let Some(port) = url.port().filter(|port| *port != 22) {
                    spec.set("port", port.to_string());
                }
                (spec, path)
            }
            "ftp" | "ftps" | "ftpis" => {
                let (mut spec, path) = need_host(&scheme)?;
                let default = if scheme == "ftpis" { 990 } else { 21 };
                if let Some(port) = url.port().filter(|port| *port != default) {
                    spec.set("port", port.to_string());
                }
                (spec, path)
            }
            "dav" | "davs" => {
                let (mut spec, path) = need_host("dav")?;
                if scheme == "davs" {
                    spec.set("ssl", "true");
                }
                if let Some(port) = url.port() {
                    spec.set("port", port.to_string());
                }
                // A DAV mount is rooted where the URI points; the backend
                // shortens the prefix itself if it finds the DAV root higher.
                spec.prefix = if path.is_empty() { "/".to_string() } else { path.clone() };
                (spec, path)
            }
            "smb" => smb_location(host.as_deref(), &user, url.path()),
            "file" => return Err("That is a local path; type it in the location bar".into()),
            _ => {
                return Err(format!(
                    "Marcel can connect to sftp://, smb://, ftp://, and dav:// addresses, not {scheme}://"
                ));
            }
        };
        if spec.prefix.len() > 1 {
            spec.prefix = spec.prefix.trim_end_matches('/').to_string();
        }
        let path = if path == "/" { String::new() } else { path.trim_end_matches('/').to_string() };
        Ok(Self { spec, path })
    }

    /// The URI form, the one saved to the servers file. Round-trips through
    /// [`Location::parse`] for every spec this module produces.
    pub fn to_uri(&self) -> String {
        let spec = &self.spec;
        let authority = |host_key: &str| {
            let host = spec.items.get(host_key).map(String::as_str).unwrap_or_default();
            let user = spec.items.get("user").map(|user| format!("{}@", encode(user)));
            let port = spec.items.get("port").map(|port| format!(":{port}"));
            format!("{}{host}{}", user.unwrap_or_default(), port.unwrap_or_default())
        };
        let path = encode_path(&self.path);
        match spec.kind.as_str() {
            "smb-share" => {
                let share = spec.items.get("share").map(String::as_str).unwrap_or_default();
                format!("smb://{}/{}{path}", authority("server"), encode(share))
            }
            "smb-server" => format!("smb://{}/", authority("server")),
            "smb-network" => "smb://".to_string(),
            "dav" => {
                let scheme = if spec.items.get("ssl").is_some_and(|ssl| ssl == "true") {
                    "davs"
                } else {
                    "dav"
                };
                let path = if self.path.is_empty() { &spec.prefix } else { &self.path };
                format!("{scheme}://{}{}", authority("host"), encode_path(path))
            }
            kind => format!(
                "{kind}://{}{}",
                authority("host"),
                if path.is_empty() { "/" } else { &path }
            ),
        }
    }

    /// What to call this when the mount has not said: the host, or the share
    /// on its server.
    pub fn label(&self) -> String {
        let spec = &self.spec;
        match spec.kind.as_str() {
            "smb-share" => format!(
                "{} on {}",
                spec.items.get("share").map(String::as_str).unwrap_or_default(),
                spec.items.get("server").map(String::as_str).unwrap_or_default()
            ),
            "smb-network" => "Windows Network".to_string(),
            _ => spec.host().unwrap_or("Server").to_string(),
        }
    }
}

/// `smb://` is the network, `smb://server/` a server's shares, and
/// `smb://server/share/...` a share, each a different backend.
fn smb_location(host: Option<&str>, user: &str, raw_path: &str) -> (MountSpec, String) {
    let Some(server) = host else {
        return (MountSpec::new("smb-network"), String::new());
    };
    let mut segments = raw_path.split('/').filter(|segment| !segment.is_empty());
    let Some(share) = segments.next() else {
        let mut spec = MountSpec::new("smb-server");
        spec.set("server", server);
        spec.set("user", user);
        return (spec, String::new());
    };
    let mut spec = MountSpec::new("smb-share");
    spec.set("server", server);
    spec.set("share", decode(share));
    spec.set("user", user);
    let rest: Vec<String> = segments.map(decode).collect();
    let path = if rest.is_empty() { String::new() } else { format!("/{}", rest.join("/")) };
    (spec, path)
}

/// `host`, `user@host`, `host:2222`: what someone types after `ssh`.
fn looks_like_host(input: &str) -> bool {
    let host = input.rsplit_once('@').map_or(input, |(_, host)| host);
    let host = host.rsplit_once(':').map_or(host, |(host, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) { host } else { input }
    });
    !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

fn encode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, USERINFO).to_string()
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| percent_encoding::utf8_percent_encode(segment, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// What the `url` crate leaves as-is in a user name: RFC 3986's unreserved
/// and sub-delimiter characters.
const USERINFO: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=');
const SEGMENT: &percent_encoding::AsciiSet = &USERINFO.remove(b':').remove(b'@');

// ---------------------------------------------------------------------------
// Live mounts.

/// One connected share, as the tracker reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The backend's bus name and object, which Unmount addresses.
    pub owner: String,
    pub object_path: OwnedObjectPath,
    /// What GVfs calls it: the host for SFTP, "share on server" for SMB.
    pub name: String,
    pub spec: MountSpec,
    /// Where `gvfsd-fuse` shows the mount, when it is running.
    pub fuse_root: Option<PathBuf>,
    /// Where the server puts a visitor with no path in mind: the home
    /// directory over SFTP, empty for the root.
    pub default_location: String,
}

impl Mount {
    /// The directory a location inside this mount is at, through FUSE.
    pub fn directory_for(&self, path: &str) -> Option<PathBuf> {
        let root = self.fuse_root.as_ref()?;
        let inside = if path.is_empty() { self.default_location.as_str() } else { path };
        let prefix = self.spec.prefix.trim_end_matches('/');
        let relative = inside.strip_prefix(prefix).unwrap_or(inside).trim_start_matches('/');
        Some(if relative.is_empty() { root.clone() } else { root.join(relative) })
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.fuse_root.as_ref().is_some_and(|root| path.starts_with(root))
    }
}

/// `(sossssssbay(aya{sv})ay)`: what `ListMounts2`, `Mounted`, and
/// `Unmounted` carry per mount.
type MountInfo = (
    String,
    OwnedObjectPath,
    String,
    String,
    String,
    String,
    String,
    String,
    bool,
    Vec<u8>,
    (Vec<u8>, HashMap<String, OwnedValue>),
    Vec<u8>,
);

fn mount_from(info: MountInfo) -> Mount {
    let (
        owner,
        object_path,
        name,
        _stable,
        _content,
        _icon,
        _symbolic,
        _encoding,
        _visible,
        fuse,
        spec,
        default_location,
    ) = info;
    let fuse = trim_nul(fuse);
    Mount {
        owner,
        object_path,
        name,
        spec: MountSpec::from_dbus(spec.0, spec.1),
        fuse_root: (!fuse.is_empty()).then(|| PathBuf::from(OsString::from_vec(fuse))),
        default_location: text(default_location),
    }
}

// ---------------------------------------------------------------------------
// Prompts: what a mount asks while it connects.

/// A request from a backend, put to whoever answers for the user.
///
/// Dropping the reply sender, or replying `None`, tells the backend the user
/// gave up; the mount then fails as cancelled, which is not an error to show.
pub enum Prompt {
    Password(PasswordRequest),
    Question(QuestionRequest),
    /// The backend has stopped waiting, whatever is on screen should go.
    Aborted,
}

/// `GAskPasswordFlags`: which fields the backend wants filled in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PasswordFlags(u32);

impl PasswordFlags {
    pub fn needs_password(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn needs_username(self) -> bool {
        self.0 & 2 != 0
    }
    pub fn needs_domain(self) -> bool {
        self.0 & 4 != 0
    }
    pub fn can_save(self) -> bool {
        self.0 & 8 != 0
    }
    pub fn allows_anonymous(self) -> bool {
        self.0 & 16 != 0
    }
}

pub struct PasswordRequest {
    pub message: String,
    pub default_user: String,
    pub default_domain: String,
    pub flags: PasswordFlags,
    pub reply: Sender<Option<PasswordReply>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PasswordReply {
    pub username: String,
    pub domain: String,
    pub password: String,
    pub anonymous: bool,
    /// Keep the password in the keyring, which GVfs does on the user's
    /// behalf when asked (`G_PASSWORD_SAVE_PERMANENTLY`).
    pub remember: bool,
}

pub struct QuestionRequest {
    pub message: String,
    pub choices: Vec<String>,
    /// The index of the chosen answer.
    pub reply: Sender<Option<usize>>,
}

/// The object a backend calls back into. One per mount or unmount call, so
/// two windows connecting at once each see their own dialogs.
struct MountOperation {
    prompts: Sender<Prompt>,
    /// Set when the user declined, so the failure that follows is read as
    /// theirs rather than the server's.
    cancelled: Arc<AtomicBool>,
}

impl MountOperation {
    async fn ask<T>(&self, build: impl FnOnce(Sender<Option<T>>) -> Prompt) -> Option<T> {
        let (reply, answer) = async_channel::bounded(1);
        if self.prompts.send(build(reply)).await.is_err() {
            return None;
        }
        answer.recv().await.ok().flatten()
    }

    fn give_up(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[zbus::interface(name = "org.gtk.vfs.MountOperation")]
impl MountOperation {
    /// Returns `(handled, aborted, password, username, domain, anonymous,
    /// password_save)`.
    async fn ask_password(
        &self,
        message_string: String,
        default_user: String,
        default_domain: String,
        flags_as_int: u32,
    ) -> (bool, bool, String, String, String, bool, u32) {
        let answer = self
            .ask(|reply| {
                Prompt::Password(PasswordRequest {
                    message: message_string,
                    default_user,
                    default_domain,
                    flags: PasswordFlags(flags_as_int),
                    reply,
                })
            })
            .await;
        match answer {
            Some(reply) => (
                true,
                false,
                reply.password,
                reply.username,
                reply.domain,
                reply.anonymous,
                if reply.remember { 2 } else { 0 },
            ),
            None => {
                self.give_up();
                (true, true, String::new(), String::new(), String::new(), false, 0)
            }
        }
    }

    /// Returns `(handled, aborted, choice)`.
    async fn ask_question(
        &self,
        message_string: String,
        choices: Vec<String>,
    ) -> (bool, bool, u32) {
        self.choose(message_string, choices).await
    }

    /// The unmount side of a question: what is holding the mount open, and
    /// whether to wait or force it. The processes themselves are not shown;
    /// the message names them.
    async fn show_processes(
        &self,
        message_string: String,
        choices: Vec<String>,
        _processes: Vec<i32>,
    ) -> (bool, bool, u32) {
        self.choose(message_string, choices).await
    }

    fn show_unmount_progress(&self, _message_string: String, _time_left: i64, _bytes_left: i64) {}

    async fn aborted(&self) {
        let _ = self.prompts.send(Prompt::Aborted).await;
    }
}

impl MountOperation {
    async fn choose(&self, message: String, choices: Vec<String>) -> (bool, bool, u32) {
        match self.ask(|reply| Prompt::Question(QuestionRequest { message, choices, reply })).await
        {
            Some(choice) => (true, false, choice as u32),
            None => {
                self.give_up();
                (true, true, 0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The client.

/// Why a mount or unmount did not happen.
#[derive(Debug)]
pub enum MountError {
    /// The user closed a prompt; nothing to report.
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for MountError {}

/// What [`GvfsClient::changed`] woke for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GvfsChange {
    /// A share was connected or disconnected: the list needs re-reading.
    Mounts,
    /// The daemon itself was replaced, by a restart or a crash. Every mount
    /// record held anywhere is stale, and the connection to the old daemon's
    /// signals is worth nothing, so the caller starts over.
    DaemonReplaced,
}

/// A connection to GVfs's daemon, or the reason there is none.
pub struct GvfsClient {
    connection: zbus::Connection,
    tracker: zbus::Proxy<'static>,
    next_operation: AtomicU64,
}

impl GvfsClient {
    /// Reach the daemon on the session bus, starting it if the bus knows how
    /// (`org.gtk.vfs.Daemon.service`). Fails where there is no session bus or
    /// no GVfs, which is what a machine without `services.gvfs` looks like.
    pub async fn connect() -> Result<Self> {
        let connection = zbus::Connection::session().await.context("No session bus")?;
        let tracker = zbus::Proxy::new(&connection, DAEMON, TRACKER_PATH, TRACKER)
            .await
            .context("Could not address GVfs")?;
        tracker
            .call::<_, _, Vec<String>>("ListMountTypes", &())
            .await
            .context("GVfs is not on the session bus")?;
        Ok(Self { connection, tracker, next_operation: AtomicU64::new(0) })
    }

    /// Wait until something owns the daemon's name, without starting one.
    ///
    /// [`connect`](Self::connect) activates GVfs, which is the right thing
    /// when a file manager opens. Reaching for it again after it has gone is
    /// not the same: a user who stopped GVfs on purpose would find Marcel
    /// starting it again every couple of seconds. This waits for a daemon
    /// somebody else brought up.
    pub async fn wait_for_daemon() -> Result<()> {
        use smol::stream::StreamExt as _;

        let connection = zbus::Connection::session().await.context("No session bus")?;
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        // Subscribed before asking, or a daemon that appears between the two
        // is missed and the wait never ends.
        let mut changes = dbus.receive_name_owner_changed_with_args(&[(0, DAEMON)]).await?;
        if dbus.name_has_owner(DAEMON.try_into()?).await? {
            return Ok(());
        }
        while let Some(change) = changes.next().await {
            if change.args()?.new_owner().is_some() {
                return Ok(());
            }
        }
        Err(anyhow!("The bus stopped reporting name changes"))
    }

    /// Every mount GVfs shows users, in the daemon's order.
    pub async fn mounts(&self) -> Result<Vec<Mount>> {
        let infos: Vec<MountInfo> =
            self.tracker.call("ListMounts2", &(true,)).await.context("Could not list shares")?;
        Ok(infos.into_iter().map(mount_from).collect())
    }

    /// Resolve once a mount has come or gone, or the daemon behind the name
    /// has been replaced.
    ///
    /// The second case is why this reports which happened. A `Mount` records
    /// the unique bus name of the backend serving it (`:1.227769`), and a
    /// daemon that restarts takes every one of those with it: the records a
    /// caller is holding name peers that no longer exist, and calling
    /// `Unmount` on one fails with "The name is not activatable" no matter
    /// whether the share is still mounted. Nothing in the tracker's own
    /// signals says this has happened, so the bus's `NameOwnerChanged` is
    /// what has to say it.
    pub async fn changed(&self) -> Result<GvfsChange> {
        use smol::stream::StreamExt as _;

        let mounted = self.tracker.receive_signal("Mounted").await?.map(|_| GvfsChange::Mounts);
        let unmounted = self.tracker.receive_signal("Unmounted").await?.map(|_| GvfsChange::Mounts);
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        let replaced = dbus
            .receive_name_owner_changed_with_args(&[(0, DAEMON)])
            .await?
            .map(|_| GvfsChange::DaemonReplaced);
        let mut any = mounted.race(unmounted).race(replaced);
        any.next().await.ok_or_else(|| anyhow!("GVfs stopped sending changes"))
    }

    /// Connect a share. Returns once the backend has it mounted, however
    /// long the server and the user's answers to `prompts` take.
    pub async fn mount(&self, spec: &MountSpec, prompts: Sender<Prompt>) -> Result<(), MountError> {
        let operation = self.operation(prompts).await?;
        let result = self
            .tracker
            .call::<_, _, ()>("MountLocation", &(spec.to_dbus(), operation.source()))
            .await;
        operation.finish(result).await
    }

    /// Disconnect a share. Files still open on it make the backend ask, via
    /// `prompts`, whether to wait or force it.
    ///
    /// A record whose backend is no longer on the bus is not an error to
    /// report: it means the daemon was replaced since the mount was listed
    /// (see [`GvfsChange::DaemonReplaced`]). What the user asked for is that
    /// this share stop being mounted, so the list is re-read and the answer
    /// comes from what is mounted now — the current record is disconnected,
    /// or, if nothing serves the spec any more, it already is.
    pub async fn unmount(&self, mount: &Mount, prompts: Sender<Prompt>) -> Result<(), MountError> {
        match self.unmount_record(mount, prompts.clone()).await {
            Err(UnmountFailure::OwnerGone) => {}
            Err(UnmountFailure::Mount(error)) => return Err(error),
            Ok(()) => return Ok(()),
        }
        let mounts = self.mounts().await.map_err(|error| MountError::Failed(error.to_string()))?;
        let Some(live) = mounts.into_iter().find(|live| mount.spec.is_served_by(&live.spec)) else {
            return Ok(());
        };
        self.unmount_record(&live, prompts).await.map_err(MountError::from)
    }

    /// One `Unmount` call against exactly the backend a record names.
    async fn unmount_record(
        &self,
        mount: &Mount,
        prompts: Sender<Prompt>,
    ) -> Result<(), UnmountFailure> {
        let operation = self.operation(prompts).await?;
        let proxy = zbus::Proxy::new(
            &self.connection,
            mount.owner.clone(),
            mount.object_path.clone(),
            MOUNT,
        )
        .await
        .map_err(|error| MountError::Failed(error.to_string()))?;
        // Unlike `MountLocation`, this takes the source flattened: `(sou)`.
        let (name, path) = operation.source();
        let result = proxy.call::<_, _, ()>("Unmount", &(name, path, 0u32)).await;
        let vanished = matches!(&result, Err(error) if owner_is_gone(error));
        let finished = operation.finish(result).await;
        match finished {
            Err(error) if vanished => Err(UnmountFailure::from_vanished(error)),
            other => other.map_err(UnmountFailure::Mount),
        }
    }

    /// Export a fresh `MountOperation` for one call.
    async fn operation(
        &self,
        prompts: Sender<Prompt>,
    ) -> Result<ExportedOperation<'_>, MountError> {
        let number = self.next_operation.fetch_add(1, Ordering::Relaxed);
        let path = ObjectPath::try_from(format!("{OPERATION_PATH}/{number}"))
            .map_err(|error| MountError::Failed(error.to_string()))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let operation = MountOperation { prompts, cancelled: Arc::clone(&cancelled) };
        self.connection
            .object_server()
            .at(path.clone(), operation)
            .await
            .map_err(|error| MountError::Failed(format!("Could not answer GVfs: {error}")))?;
        Ok(ExportedOperation { client: self, path: path.into(), cancelled })
    }
}

/// Why one `Unmount` call against one record did not go through.
enum UnmountFailure {
    /// The backend the record names is not on the bus at all, so the call
    /// never reached a GVfs. Distinguishing this from a refusal by a live
    /// backend is the whole point: one is worth retrying against a fresh
    /// record, the other is the server's answer and must reach the user.
    OwnerGone,
    Mount(MountError),
}

impl UnmountFailure {
    /// Cancellation wins over a vanished peer: the user answered a prompt,
    /// which means a backend was there to ask.
    fn from_vanished(error: MountError) -> Self {
        match error {
            MountError::Cancelled => Self::Mount(MountError::Cancelled),
            MountError::Failed(_) => Self::OwnerGone,
        }
    }
}

impl From<MountError> for UnmountFailure {
    fn from(error: MountError) -> Self {
        Self::Mount(error)
    }
}

impl From<UnmountFailure> for MountError {
    fn from(failure: UnmountFailure) -> Self {
        match failure {
            UnmountFailure::Mount(error) => error,
            // Reached only when a record that was fresh a moment ago has
            // gone in between, which is the share being disconnected.
            UnmountFailure::OwnerGone => MountError::Failed(
                "GVfs stopped serving that share while it was being disconnected".to_string(),
            ),
        }
    }
}

/// Whether the bus refused the call because nothing owns the name, rather
/// than a backend refusing the unmount.
fn owner_is_gone(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if names_a_missing_peer(name.as_str()))
}

/// The two D-Bus errors that mean the call never reached anyone.
///
/// `ServiceUnknown` is what a unique name that has gone gets, and it is the
/// one a user sees as "The name is not activatable"; `NameHasNoOwner` is the
/// same answer worded for a well-known one.
fn names_a_missing_peer(error_name: &str) -> bool {
    matches!(
        error_name,
        "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner"
    )
}

/// A `MountOperation` on the bus for the span of one call.
struct ExportedOperation<'a> {
    client: &'a GvfsClient,
    path: OwnedObjectPath,
    cancelled: Arc<AtomicBool>,
}

impl ExportedOperation<'_> {
    /// The `(so)` a backend calls back to.
    fn source(&self) -> (String, OwnedObjectPath) {
        let name =
            self.client.connection.unique_name().map(|name| name.to_string()).unwrap_or_default();
        (name, self.path.clone())
    }

    async fn finish(self, result: zbus::Result<()>) -> Result<(), MountError> {
        let _ =
            self.client.connection.object_server().remove::<MountOperation, _>(&self.path).await;
        match result {
            Ok(()) => Ok(()),
            Err(_) if self.cancelled.load(Ordering::Relaxed) => Err(MountError::Cancelled),
            Err(error) => Err(MountError::Failed(describe(&error))),
        }
    }
}

/// GVfs answers with GIO's error text, which is already a sentence for the
/// user ("Connection refused", "Permission denied"); the D-Bus error name
/// around it is not.
fn describe(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(text), _) if !text.is_empty() => {
            text.trim_end_matches('.').to_string()
        }
        zbus::Error::MethodError(name, _, _) => {
            name.as_str().rsplit('.').next().unwrap_or(name.as_str()).to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// A mount records the unique name of the backend serving it. Restart
    /// GVfs and every one of those names is dead, so `Unmount` against a
    /// record listed beforehand comes back "The name is not activatable" —
    /// which says nothing about whether the share is still mounted, and must
    /// not reach the user as though it did.
    #[test]
    fn a_call_that_reached_nobody_is_told_apart_from_a_backend_refusing() {
        assert!(names_a_missing_peer("org.freedesktop.DBus.Error.ServiceUnknown"));
        assert!(names_a_missing_peer("org.freedesktop.DBus.Error.NameHasNoOwner"));

        // A live backend saying no. These are the server's answer and belong
        // on screen exactly as they are.
        assert!(!names_a_missing_peer("org.freedesktop.DBus.Error.AccessDenied"));
        assert!(!names_a_missing_peer("org.gtk.vfs.Error.Busy"));
        assert!(!names_a_missing_peer("org.freedesktop.DBus.Error.NoReply"));
    }

    const PRIVATE_BUS_CHILD: &str = "MARCEL_GVFS_PRIVATE_BUS_TEST_CHILD";
    const PRIVATE_BUS_CONFIG: &str = "MARCEL_TEST_DBUS_SESSION_CONFIG";

    /// Watching for a daemon that is not there yet, on a bus where it can be
    /// made to appear on cue.
    ///
    /// The live test can only check the case where a daemon is already
    /// running, which `NameHasOwner` answers without the signal ever being
    /// read. This checks the other half, and the half that matters after a
    /// restart: the name appearing while Marcel is waiting. Both ways of
    /// getting it wrong — an argument filter that matches nothing, a
    /// subscription made after the check — look like a wait that never ends,
    /// so a timeout is the assertion.
    #[test]
    fn private_session_bus_daemon_wait() {
        if std::env::var_os(PRIVATE_BUS_CHILD).is_some() {
            return;
        }
        let module = module_path!()
            .strip_prefix(concat!(env!("CARGO_PKG_NAME"), "::"))
            .unwrap_or(module_path!());
        let mut command = std::process::Command::new("dbus-run-session");
        if let Some(config) = std::env::var_os(PRIVATE_BUS_CONFIG) {
            command.arg("--config-file").arg(config);
        }
        let output = command
            .arg("--")
            .arg(std::env::current_exe().expect("test executable must have a path"))
            .arg("--exact")
            .arg(format!("{module}::private_session_bus_daemon_wait_child"))
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

    #[test]
    fn private_session_bus_daemon_wait_child() {
        if std::env::var_os(PRIVATE_BUS_CHILD).is_none() {
            return;
        }
        smol::block_on(async {
            let connection =
                zbus::Connection::session().await.expect("the private bus must be reachable");
            let dbus = zbus::fdo::DBusProxy::new(&connection).await.unwrap();
            assert!(
                !dbus.name_has_owner(DAEMON.try_into().unwrap()).await.unwrap(),
                "a fresh private bus must not already have a GVfs on it"
            );

            let appear = async {
                // Long enough for the wait to have subscribed and found no
                // owner, so the name genuinely arrives while it is watching.
                smol::Timer::after(std::time::Duration::from_millis(250)).await;
                connection
                    .request_name(DAEMON)
                    .await
                    .expect("the private bus must hand over the name");
            };
            let wait = async {
                GvfsClient::wait_for_daemon().await.expect("watching the bus must not fail");
                true
            };
            let timeout = async {
                smol::Timer::after(std::time::Duration::from_secs(10)).await;
                false
            };
            let (saw_it, ()) = smol::future::zip(smol::future::or(wait, timeout), appear).await;
            assert!(saw_it, "a daemon taking the name must end the wait");
        });
    }

    /// Cancelling wins over a vanished peer: the user cannot have answered a
    /// prompt unless a backend was there to ask it, so the retry that a
    /// stale record earns would re-ask a question already answered.
    #[test]
    fn cancelling_an_unmount_is_never_read_as_a_stale_record() {
        assert!(matches!(
            UnmountFailure::from_vanished(MountError::Cancelled),
            UnmountFailure::Mount(MountError::Cancelled)
        ));
        assert!(matches!(
            UnmountFailure::from_vanished(MountError::Failed("gone".to_string())),
            UnmountFailure::OwnerGone
        ));
    }

    #[test]
    fn a_bare_host_means_sftp() {
        let location = Location::parse("wired").unwrap();
        assert_eq!(location.spec.kind, "sftp");
        assert_eq!(location.spec.items, items(&[("host", "wired")]));
        assert_eq!(location.path, "");
        assert_eq!(location.to_uri(), "sftp://wired/");

        let location = Location::parse("me@box.local:2222").unwrap();
        assert_eq!(
            location.spec.items,
            items(&[("host", "box.local"), ("user", "me"), ("port", "2222")])
        );
        assert_eq!(location.to_uri(), "sftp://me@box.local:2222/");

        assert!(Location::parse("not a host").is_err());
        assert!(Location::parse("").is_err());
    }

    #[test]
    fn sftp_uris_keep_their_path_and_drop_the_default_port() {
        let location = Location::parse("sftp://me@wired:22/home/me/Work Notes/").unwrap();
        assert_eq!(location.spec.items, items(&[("host", "wired"), ("user", "me")]));
        assert_eq!(location.path, "/home/me/Work Notes");
        assert_eq!(location.to_uri(), "sftp://me@wired/home/me/Work%20Notes");
        assert_eq!(Location::parse(&location.to_uri()).unwrap(), location);
        assert_eq!(Location::parse("ssh://wired").unwrap().spec.kind, "sftp");
    }

    #[test]
    fn smb_picks_the_backend_by_depth() {
        assert_eq!(Location::parse("smb://").unwrap().spec.kind, "smb-network");

        let server = Location::parse("smb://nas/").unwrap();
        assert_eq!(server.spec.kind, "smb-server");
        assert_eq!(server.spec.items, items(&[("server", "nas")]));
        assert_eq!(server.to_uri(), "smb://nas/");

        let share = Location::parse("smb://me@nas/media/films/2024").unwrap();
        assert_eq!(share.spec.kind, "smb-share");
        assert_eq!(
            share.spec.items,
            items(&[("server", "nas"), ("share", "media"), ("user", "me")])
        );
        assert_eq!(share.path, "/films/2024");
        assert_eq!(share.to_uri(), "smb://me@nas/media/films/2024");
        assert_eq!(share.label(), "media on nas");
        assert_eq!(Location::parse(&share.to_uri()).unwrap(), share);
    }

    #[test]
    fn dav_is_rooted_where_the_uri_points() {
        let location = Location::parse("davs://cloud.example/remote.php/dav/files/me/").unwrap();
        assert_eq!(location.spec.kind, "dav");
        assert_eq!(location.spec.items, items(&[("host", "cloud.example"), ("ssl", "true")]));
        assert_eq!(location.spec.prefix, "/remote.php/dav/files/me");
        assert_eq!(location.to_uri(), "davs://cloud.example/remote.php/dav/files/me");
        assert_eq!(Location::parse(&location.to_uri()).unwrap(), location);
        assert_eq!(Location::parse("dav://h/").unwrap().spec.prefix, "/");
    }

    #[test]
    fn unsupported_schemes_are_named() {
        let error = Location::parse("gopher://x/").unwrap_err();
        assert!(error.contains("gopher"), "{error}");
        assert!(Location::parse("file:///tmp").unwrap_err().contains("local"));
        assert!(Location::parse("sftp:///nohost").is_err());
    }

    #[test]
    fn a_saved_server_matches_the_mount_that_serves_it() {
        let wanted = Location::parse("sftp://wired/home/me").unwrap().spec;
        let mut mounted = wanted.clone();
        assert!(wanted.is_served_by(&mounted));
        mounted.items.insert("user".into(), "me".into());
        assert!(wanted.is_served_by(&mounted), "the backend may add what it learned");
        let other = Location::parse("sftp://other/").unwrap().spec;
        assert!(!wanted.is_served_by(&other));
        let mut ftp = wanted.clone();
        ftp.kind = "ftp".into();
        assert!(!wanted.is_served_by(&ftp));

        let deep = Location::parse("davs://h/remote.php/dav/files/me").unwrap().spec;
        let mut root = deep.clone();
        root.prefix = "/remote.php/dav".into();
        assert!(deep.is_served_by(&root), "a DAV backend may settle higher");
        let mut sibling = deep.clone();
        sibling.prefix = "/remote.php/davish".into();
        assert!(!deep.is_served_by(&sibling));
    }

    #[test]
    fn the_wire_form_is_nul_terminated_bytestrings() {
        let spec = Location::parse("sftp://me@wired/").unwrap().spec;
        let (prefix, items) = spec.to_dbus();
        assert_eq!(prefix, b"/\0");
        assert_eq!(items.len(), 3);
        assert_eq!(items["type"], Value::from(b"sftp\0".to_vec()));
        assert_eq!(items["host"], Value::from(b"wired\0".to_vec()));

        let owned: HashMap<String, OwnedValue> = items
            .into_iter()
            .map(|(key, value)| (key, OwnedValue::try_from(value).unwrap()))
            .collect();
        assert_eq!(MountSpec::from_dbus(prefix, owned), spec);
    }

    #[test]
    fn a_mount_resolves_paths_through_fuse() {
        let mount = Mount {
            owner: ":1.5".into(),
            object_path: OwnedObjectPath::try_from("/org/gtk/vfs/mount/1").unwrap(),
            name: "wired".into(),
            spec: Location::parse("sftp://wired/").unwrap().spec,
            fuse_root: Some(PathBuf::from("/run/user/1000/gvfs/sftp:host=wired")),
            default_location: "/home/me".into(),
        };
        assert_eq!(
            mount.directory_for(""),
            Some(PathBuf::from("/run/user/1000/gvfs/sftp:host=wired/home/me")),
            "no path means the server's default"
        );
        assert_eq!(
            mount.directory_for("/etc"),
            Some(PathBuf::from("/run/user/1000/gvfs/sftp:host=wired/etc"))
        );
        assert!(mount.contains(Path::new("/run/user/1000/gvfs/sftp:host=wired/etc")));
        assert!(!mount.contains(Path::new("/run/user/1000/gvfs/sftp:host=wired2")));

        let mut dav = mount.clone();
        dav.spec = Location::parse("davs://h/remote.php/dav").unwrap().spec;
        dav.default_location = String::new();
        dav.fuse_root = Some(PathBuf::from("/run/user/1000/gvfs/dav:host=h,ssl=true"));
        assert_eq!(
            dav.directory_for("/remote.php/dav/files/me"),
            Some(PathBuf::from("/run/user/1000/gvfs/dav:host=h,ssl=true/files/me")),
            "the prefix is the FUSE root, not a directory under it"
        );
        assert_eq!(dav.directory_for(""), dav.fuse_root);

        let mut without_fuse = mount;
        without_fuse.fuse_root = None;
        assert_eq!(without_fuse.directory_for(""), None);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// The signal-argument filter and `NameHasOwner` are the two things here
    /// that only the real bus can check: an argument index off by one makes
    /// the wait miss every daemon that ever appears, and both failures look
    /// like nothing happening.
    #[test]
    #[ignore = "needs GVfs on the session bus"]
    fn live_wait_for_daemon_returns_at_once_while_one_is_running() {
        smol::block_on(async {
            // Proves a daemon is up, so the wait must not block.
            GvfsClient::connect().await.expect("GVfs has to be running for this test");
            let waited = smol::future::or(
                async { GvfsClient::wait_for_daemon().await.map(|()| true) },
                async {
                    smol::Timer::after(std::time::Duration::from_secs(5)).await;
                    Ok(false)
                },
            )
            .await
            .unwrap();
            assert!(waited, "a running daemon must be seen without waiting for a signal");
        });
    }

    /// Talks to the real GVfs, so it is opt-in: `cargo test -- --ignored
    /// live_mounts --nocapture` prints what the Network section would list.
    #[test]
    #[ignore = "needs GVfs on the session bus"]
    fn live_mounts() {
        let mounts = smol::block_on(async {
            let client = GvfsClient::connect().await?;
            client.mounts().await
        })
        .unwrap();
        for mount in &mounts {
            eprintln!("{mount:#?}");
        }
    }

    /// Mounts and unmounts `MARCEL_TEST_SERVER` (an address `Location::parse`
    /// takes) through GVfs, answering no prompts: the host has to be one the
    /// agent logs into on its own.
    #[test]
    #[ignore = "needs GVfs and a reachable server in MARCEL_TEST_SERVER"]
    fn live_mount_round_trip() {
        let Ok(address) = std::env::var("MARCEL_TEST_SERVER") else {
            eprintln!("MARCEL_TEST_SERVER is not set; skipping");
            return;
        };
        let location = Location::parse(&address).unwrap();
        smol::block_on(async {
            let client = GvfsClient::connect().await.unwrap();
            let (prompts, incoming) = async_channel::unbounded();
            drop(incoming);
            // "Location is already mounted" from an earlier run that stopped
            // short of unmounting is the one failure the round trip absorbs.
            if let Err(error) = client.mount(&location.spec, prompts.clone()).await {
                eprintln!("mount: {error}");
            }
            let mount = client
                .mounts()
                .await
                .unwrap()
                .into_iter()
                .find(|mount| location.spec.is_served_by(&mount.spec))
                .expect("the mount is listed once MountLocation returns");
            eprintln!("{mount:#?}");
            let directory = mount.directory_for(&location.path).expect("gvfsd-fuse is running");
            assert!(directory.is_dir(), "{} is not a directory", directory.display());
            client.unmount(&mount, prompts).await.unwrap();
            assert!(
                !client.mounts().await.unwrap().iter().any(|m| m.spec == mount.spec),
                "unmounted"
            );
        });
    }
}
