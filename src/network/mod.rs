//! The application's view of network shares: the servers the user saved,
//! the shares GVfs has connected, and which of the former the latter serve.
//!
//! Two lists, one store. The saved servers live in the `servers` file beside
//! `bookmarks` and follow the same single-writer rules; the connected shares
//! are GVfs's, re-read every time its tracker announces a change. A saved
//! server is shown whether or not it is connected, and connecting it means
//! asking GVfs to mount its spec and then navigating into the FUSE directory
//! the mount appears at. Without GVfs the store has no client, `available`
//! is false, and the sidebar has no Network section.

mod prompts;

use std::{
    collections::HashSet,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Task};

use crate::{
    config,
    desktop::gvfs::{GvfsChange, GvfsClient, Location, Mount, MountError, MountSpec},
    surface::{self, Report},
};

// ---------------------------------------------------------------------------
// The servers file.

/// One line of the servers file: where, and what the user calls it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub location: Location,
    /// The user's name for it; without one, the host or share names it.
    pub name: Option<String>,
}

impl Server {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.location.label())
    }
}

/// What one read of the servers file produced. `rejected` counts lines that
/// are not servers Marcel understands; as with bookmarks, a nonzero count
/// makes the store refuse to save over them.
pub struct LoadedServers {
    pub servers: Vec<Server>,
    pub rejected: usize,
}

/// `<uri>`, or `<uri> <name>`: the GTK bookmarks shape, so a line can be
/// written by hand and a name can hold spaces.
pub fn load(path: &Path) -> Result<LoadedServers> {
    let Some(contents) = config::read_own_file(path).context("Could not read servers")? else {
        return Ok(LoadedServers { servers: Vec::new(), rejected: 0 });
    };
    let mut seen = HashSet::new();
    let mut rejected = 0;
    let servers = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (uri, name) = match line.split_once(char::is_whitespace) {
                Some((uri, name)) => (uri, Some(name.trim().to_string()).filter(|n| !n.is_empty())),
                None => (line, None),
            };
            // The file holds URIs, which is what Marcel writes; the bare-host
            // shorthand the Connect dialog takes would make any stray word
            // here a server.
            let Ok(location) = Location::parse(uri).ok().filter(|_| uri.contains("://")).ok_or(())
            else {
                rejected += 1;
                return None;
            };
            // A duplicate of a server already loaded is not user data at
            // risk; collapsing it loses nothing.
            seen.insert(location.clone()).then_some(Server { location, name })
        })
        .collect();
    Ok(LoadedServers { servers, rejected })
}

pub fn save(path: &Path, servers: &[Server]) -> Result<()> {
    config::write_atomically(path, |file| {
        for server in servers {
            let uri = server.location.to_uri();
            match &server.name {
                Some(name) => writeln!(
                    file,
                    "{uri} {}",
                    name.split_whitespace().collect::<Vec<_>>().join(" ")
                )?,
                None => writeln!(file, "{uri}")?,
            }
        }
        Ok(())
    })
    .context("Could not write servers")
}

// ---------------------------------------------------------------------------
// The store.

/// How long to let a daemon that has just taken the name finish starting.
///
/// The bus reports the name as owned the moment the daemon takes it, which
/// is before it can answer a call on it. Long enough to let a
/// `systemctl --user restart gvfs-daemon` settle, short enough that the
/// Network section is back before the user has finished wondering.
const GVFS_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

struct GlobalNetwork(Entity<NetworkStore>);

impl Global for GlobalNetwork {}

pub fn global(home: &Path, cx: &mut App) -> Entity<NetworkStore> {
    if let Some(existing) = cx.try_global::<GlobalNetwork>() {
        return existing.0.clone();
    }
    let store = cx.new(|cx| NetworkStore::start(config::path(home, "servers"), cx));
    cx.set_global(GlobalNetwork(store.clone()));
    store
}

pub struct NetworkStore {
    path: PathBuf,
    client: Option<Arc<GvfsClient>>,
    mounts: Vec<Mount>,
    servers: Vec<Server>,
    /// The one icon every row wears.
    icon: Option<PathBuf>,
    /// Specs whose mount or unmount is in flight, so a row can show it and a
    /// second click does not start a second call.
    busy: Vec<MountSpec>,
    loading: bool,
    /// Why the saved list must not be modified, when it must not be; see
    /// `BookmarkStore::read_only`.
    read_only: Option<String>,
    _load_task: Option<Task<()>>,
    _watch_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
}

impl NetworkStore {
    fn start(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let load_path = path.clone();
        let loaded = cx.background_executor().spawn(smol::unblock(move || {
            let loaded = load(&load_path)?;
            let icon = crate::desktop::icons::IconProvider::discover().icon_for_network();
            anyhow::Ok((loaded, icon))
        }));
        let load_task = cx.spawn(async move |this, cx| {
            let result = loaded.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((loaded, icon)) => {
                        this.servers = loaded.servers;
                        this.icon = icon;
                        if loaded.rejected > 0 {
                            this.read_only = Some(format!(
                                "{} line(s) in “{}” are not servers Marcel understands; \
                                 fix or remove them to change the list, or they would be lost",
                                loaded.rejected,
                                this.path.display()
                            ));
                        }
                    }
                    Err(error) => {
                        this.read_only = Some(format!(
                            "Servers could not be loaded, so they cannot be changed: {error:#}"
                        ));
                    }
                }
                cx.notify();
            });
        });

        let watch_task = cx.spawn(async move |this, cx| {
            let client = match GvfsClient::connect().await {
                Ok(client) => Arc::new(client),
                // No session bus, or no GVfs on it: the sidebar has no Network
                // section and nothing else changes.
                Err(error) => {
                    eprintln!("Marcel will not list network shares: {error:#}");
                    return;
                }
            };
            let mut client = client;
            loop {
                let _ = this.update(cx, |this, _| this.client = Some(Arc::clone(&client)));
                let restart = loop {
                    match client.mounts().await {
                        Ok(mounts) => {
                            if this
                                .update(cx, |this, cx| {
                                    this.mounts = mounts;
                                    cx.notify();
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => eprintln!("Marcel could not list network shares: {error:#}"),
                    }
                    match client.changed().await {
                        Ok(GvfsChange::Mounts) => continue,
                        // Every mount record names a backend of the daemon
                        // that has just gone, and so does the subscription
                        // these signals arrive on. Nothing of this client is
                        // worth keeping.
                        Ok(GvfsChange::DaemonReplaced) => break None,
                        Err(error) => break Some(error),
                    }
                };
                if let Some(error) = restart {
                    eprintln!("Marcel stopped watching network shares: {error:#}");
                }
                // The share list belongs to a daemon that is gone: showing it
                // would offer rows that cannot be clicked. The section comes
                // back with the daemon.
                if this
                    .update(cx, |this, cx| {
                        this.client = None;
                        this.mounts.clear();
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                client = loop {
                    // Waited for rather than started: see `wait_for_daemon`.
                    if let Err(error) = GvfsClient::wait_for_daemon().await {
                        eprintln!("Marcel stopped waiting for GVfs: {error:#}");
                        return;
                    }
                    // The name is owned from the moment the daemon takes it,
                    // which is before it is ready to answer; a failed connect
                    // here simply waits for the next one.
                    cx.background_executor().timer(GVFS_RECONNECT_DELAY).await;
                    match GvfsClient::connect().await {
                        Ok(client) => break Arc::new(client),
                        Err(error) => eprintln!("Marcel could not reach GVfs: {error:#}"),
                    }
                };
            }
        });

        Self {
            path,
            client: None,
            mounts: Vec::new(),
            servers: Vec::new(),
            icon: None,
            busy: Vec::new(),
            loading: true,
            read_only: None,
            _load_task: Some(load_task),
            _watch_task: Some(watch_task),
            save_task: None,
        }
    }

    /// Whether there is a GVfs to connect through at all.
    pub fn available(&self) -> bool {
        self.client.is_some()
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn icon(&self) -> Option<&Path> {
        self.icon.as_deref()
    }

    /// The live mount serving `spec`, if any.
    pub fn mount_for(&self, spec: &MountSpec) -> Option<&Mount> {
        self.mounts.iter().find(|mount| spec.is_served_by(&mount.spec))
    }

    /// Mounts no saved server accounts for: connected by hand, or by
    /// another application.
    pub fn unsaved_mounts(&self) -> Vec<Mount> {
        self.mounts
            .iter()
            .filter(|mount| {
                !self.servers.iter().any(|server| server.location.spec.is_served_by(&mount.spec))
            })
            .cloned()
            .collect()
    }

    /// The mount whose FUSE directory holds `path`: what the sidebar
    /// highlights while a window is on a share.
    pub fn mount_containing(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|mount| mount.contains(path))
            .max_by_key(|mount| mount.fuse_root.as_ref().map(|root| root.as_os_str().len()))
    }

    pub fn is_busy(&self, spec: &MountSpec) -> bool {
        self.busy.iter().any(|busy| busy == spec || busy.is_served_by(spec))
    }

    /// Connections in flight that no row stands for yet: an address typed
    /// into the location bar or the Connect dialog, until it is listed.
    pub fn pending(&self) -> Vec<MountSpec> {
        self.busy
            .iter()
            .filter(|spec| {
                self.mount_for(spec).is_none()
                    && !self.servers.iter().any(|server| server.location.spec.is_served_by(spec))
            })
            .cloned()
            .collect()
    }

    /// Connect a location, reporting failure on the window that asked and
    /// handing the directory to `then` once it can be browsed. A location
    /// already mounted answers at once.
    pub fn connect(
        &mut self,
        location: Location,
        origin: AnyWindowHandle,
        then: impl FnOnce(PathBuf, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        if let Some(mount) = self.mount_for(&location.spec) {
            match mount.directory_for(&location.path) {
                // Deferred, not called: the window that asked is mid-update
                // when it asks, and `then` updates it again.
                Some(directory) => cx.defer(move |cx| then(directory, cx)),
                None => self.refuse(no_fuse(mount), origin, cx),
            }
            return;
        }
        let Some(client) = self.begin(&location.spec, origin, cx) else {
            return;
        };
        let (prompts, incoming) = async_channel::unbounded();
        prompts::serve(incoming, origin, cx);
        cx.spawn(async move |this, cx| {
            let result = match client.mount(&location.spec, prompts).await {
                Ok(()) => {
                    client.mounts().await.map_err(|error| MountError::Failed(error.to_string()))
                }
                Err(error) => Err(error),
            };
            let outcome = this.update(cx, |this, cx| {
                this.finish(&location.spec, cx);
                result.map(|mounts| {
                    this.mounts = mounts;
                    let mount = this.mount_for(&location.spec).cloned();
                    cx.notify();
                    mount
                })
            });
            let report = match outcome {
                Ok(Ok(Some(mount))) => match mount.directory_for(&location.path) {
                    Some(directory) => {
                        cx.update(|cx| then(directory, cx));
                        None
                    }
                    None => Some(Report::Error(no_fuse(&mount))),
                },
                Ok(Ok(None)) => Some(Report::Error(format!(
                    "GVfs connected “{}” but does not list it",
                    location.label()
                ))),
                Ok(Err(MountError::Cancelled)) | Err(_) => None,
                Ok(Err(MountError::Failed(reason))) => Some(Report::Error(format!(
                    "Could not connect to “{}”: {reason}",
                    location.label()
                ))),
            };
            surface::deliver(origin, report, cx);
        })
        .detach();
    }

    pub fn disconnect(&mut self, mount: Mount, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(client) = self.begin(&mount.spec, origin, cx) else {
            return;
        };
        let (prompts, incoming) = async_channel::unbounded();
        prompts::serve(incoming, origin, cx);
        cx.spawn(async move |this, cx| {
            let result = client.unmount(&mount, prompts).await;
            // The `Unmounted` signal refreshes the list on its own, but a
            // record that was stale enough to need `unmount`'s second attempt
            // came from a daemon whose signals never arrived. Re-reading here
            // costs one call and leaves the row matching the truth either way.
            let mounts = match &result {
                Ok(()) => client.mounts().await.ok(),
                Err(_) => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.finish(&mount.spec, cx);
                if let Some(mounts) = mounts {
                    this.mounts = mounts;
                }
            });
            let report = match result {
                Ok(()) => Some(Report::Success(format!("Disconnected from “{}”", mount.name))),
                Err(MountError::Cancelled) => None,
                Err(MountError::Failed(reason)) => {
                    Some(Report::Error(format!("Could not disconnect “{}”: {reason}", mount.name)))
                }
            };
            surface::deliver(origin, report, cx);
        })
        .detach();
    }

    /// Mark a spec busy and hand back the client, or say why not.
    fn begin(
        &mut self,
        spec: &MountSpec,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Arc<GvfsClient>> {
        let refusal = if self.is_busy(spec) {
            Some("That server is still being connected or disconnected; wait a moment".to_string())
        } else if self.client.is_none() {
            Some("Network shares are not available: GVfs is not on the session bus".to_string())
        } else {
            None
        };
        if let Some(reason) = refusal {
            self.refuse(reason, origin, cx);
            return None;
        }
        self.busy.push(spec.clone());
        cx.notify();
        self.client.clone()
    }

    fn finish(&mut self, spec: &MountSpec, cx: &mut Context<Self>) {
        self.busy.retain(|busy| busy != spec);
        cx.notify();
    }

    fn refuse(&self, reason: String, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        cx.spawn(async move |_, cx| {
            surface::deliver(origin, Some(Report::Error(reason)), cx);
        })
        .detach();
    }

    // -----------------------------------------------------------------------
    // The saved list.

    /// Refuse a mutation while the list is not the user's list yet, telling
    /// them why on the window that asked.
    fn writable(&self, origin: AnyWindowHandle, cx: &mut Context<Self>) -> bool {
        let reason = if self.loading {
            Some("Servers are still loading; try again in a moment".to_string())
        } else {
            self.read_only.clone()
        };
        let Some(reason) = reason else {
            return true;
        };
        self.refuse(reason, origin, cx);
        false
    }

    /// Save a server. `false` means it was there already, or the store
    /// refused and has said why.
    pub fn add(
        &mut self,
        location: Location,
        name: Option<String>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.writable(origin, cx) || self.servers.iter().any(|s| s.location == location) {
            return false;
        }
        self.servers.push(Server { location, name });
        self.changed(origin, cx);
        true
    }

    /// Remove the server at `index`, provided it is still `expected`: the
    /// menu opened on one window's rendering of a list another window can
    /// change.
    pub fn remove_at(
        &mut self,
        index: usize,
        expected: &Location,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Server> {
        if !self.writable(origin, cx) || !self.still_at(index, expected) {
            return None;
        }
        let server = self.servers.remove(index);
        self.changed(origin, cx);
        Some(server)
    }

    pub fn rename_at(
        &mut self,
        index: usize,
        expected: &Location,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.writable(origin, cx) || !self.still_at(index, expected) {
            return false;
        }
        let name = name.trim();
        self.servers[index].name = (!name.is_empty()).then(|| name.to_string());
        self.changed(origin, cx);
        true
    }

    fn still_at(&self, index: usize, expected: &Location) -> bool {
        self.servers.get(index).is_some_and(|server| &server.location == expected)
    }

    fn changed(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.start_save(origin, cx);
        cx.notify();
    }

    /// Write the current list, then again if it moved on while saving; the
    /// same coalescing writer as bookmarks.
    fn start_save(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        if self.loading || self.read_only.is_some() || self.save_task.is_some() {
            return;
        }
        let path = self.path.clone();
        let snapshot = self.servers.clone();
        let saved_snapshot = snapshot.clone();
        let saving = cx.background_executor().spawn(smol::unblock(move || save(&path, &snapshot)));
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = saving.await;
            let report = this.update(cx, |this, cx| {
                // Clearing this drops the running task's handle; everything
                // after it stays synchronous.
                this.save_task = None;
                cx.notify();
                if this.servers != saved_snapshot {
                    this.start_save(origin, cx);
                }
                match result {
                    Err(error) => Some(Report::Error(format!("Could not save servers: {error}"))),
                    Ok(()) => None,
                }
            });
            surface::deliver(origin, report.ok().flatten(), cx);
        }));
    }
}

fn no_fuse(mount: &Mount) -> String {
    format!(
        "“{}” is connected, but GVfs is not showing it as a folder: gvfsd-fuse is not running",
        mount.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    fn server(uri: &str, name: Option<&str>) -> Server {
        Server { location: Location::parse(uri).unwrap(), name: name.map(str::to_string) }
    }

    #[test]
    fn round_trips_names_and_escaped_paths() {
        let sandbox = Sandbox::new();
        let file = sandbox.path("config/servers");
        let servers = vec![
            server("sftp://me@wired/home/me/Work Notes", Some("Think Centre")),
            server("smb://nas/media", None),
            server("davs://cloud.example/remote.php/dav", Some("Cloud")),
        ];

        save(&file, &servers).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            "sftp://me@wired/home/me/Work%20Notes Think Centre\nsmb://nas/media\ndavs://cloud.example/remote.php/dav Cloud\n"
        );
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.servers, servers);
        assert_eq!(loaded.rejected, 0);
    }

    #[test]
    fn unreadable_lines_are_counted_and_duplicates_collapsed() {
        let sandbox = Sandbox::new();
        let file = sandbox.file(
            "servers",
            "sftp://wired/\n\nnot a server address\nsftp://wired/ Again\ngopher://x/\nwired\n",
        );
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.servers, vec![server("sftp://wired/", None)]);
        assert_eq!(loaded.rejected, 3, "a bare word is not a server in the file");
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let sandbox = Sandbox::new();
        let loaded = load(&sandbox.path("servers")).unwrap();
        assert!(loaded.servers.is_empty());
        assert_eq!(loaded.rejected, 0);
    }

    #[test]
    fn labels_prefer_the_users_name() {
        assert_eq!(server("sftp://wired/", Some("Think Centre")).label(), "Think Centre");
        assert_eq!(server("sftp://me@wired/", None).label(), "wired");
        assert_eq!(server("smb://nas/media", None).label(), "media on nas");
    }
}
