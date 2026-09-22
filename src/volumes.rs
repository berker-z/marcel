//! The application's view of the drives UDisks2 reports, shared by every
//! window the way bookmarks are.
//!
//! One monitor, one list. The store connects once, re-reads the list every
//! time UDisks2 announces a change, and notifies observers; windows draw
//! whatever it holds and ask it to mount, unmount, or eject. Without UDisks2
//! the store stays empty and `available` is false, which the sidebar reads
//! as "no Devices section" rather than "no devices".

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Task};

use crate::{
    desktop::{
        icons::IconProvider,
        volumes::{Volume, VolumeMonitor},
    },
    surface::{self, Report},
};

struct GlobalVolumes(Entity<VolumeStore>);

impl Global for GlobalVolumes {}

pub fn global(home: &Path, cx: &mut App) -> Entity<VolumeStore> {
    if let Some(existing) = cx.try_global::<GlobalVolumes>() {
        return existing.0.clone();
    }
    let store = cx.new(|cx| VolumeStore::start(home.to_path_buf(), cx));
    cx.set_global(GlobalVolumes(store.clone()));
    store
}

/// What a request to change a volume's state came to; a mount reports its
/// mount point through its own path instead.
#[derive(Debug)]
pub enum VolumeChange {
    Unmounted,
    Ejected,
}

pub struct VolumeStore {
    monitor: Option<Arc<VolumeMonitor>>,
    volumes: Vec<Volume>,
    icons: HashMap<PathBuf, PathBuf>,
    /// Devices whose mount, unmount, or eject is in flight, so a row can
    /// show it and a second click does not start a second call.
    busy: Vec<PathBuf>,
    _watch: Option<Task<()>>,
}

impl VolumeStore {
    fn start(home: PathBuf, cx: &mut Context<Self>) -> Self {
        let user = std::env::var("USER").unwrap_or_default();
        let watch = cx.spawn(async move |this, cx| {
            let monitor = match VolumeMonitor::connect(user, home).await {
                Ok(monitor) => Arc::new(monitor),
                // No system bus, or nothing on it: a container, a BSD one day.
                // The sidebar has no Devices section and nothing else changes.
                Err(error) => {
                    eprintln!("Marcel will not list drives: {error:#}");
                    return;
                }
            };
            let _ = this.update(cx, |this, _| this.monitor = Some(Arc::clone(&monitor)));
            loop {
                match monitor.snapshot().await {
                    Ok(volumes) => {
                        let icons = {
                            let volumes = volumes.clone();
                            cx.background_executor()
                                .spawn(smol::unblock(move || icons_for(&volumes)))
                                .await
                        };
                        if this
                            .update(cx, |this, cx| {
                                this.volumes = volumes;
                                this.icons = icons;
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => eprintln!("Marcel could not list drives: {error:#}"),
                }
                if let Err(error) = monitor.changed().await {
                    eprintln!("Marcel stopped watching drives: {error:#}");
                    let _ = this.update(cx, |this, cx| {
                        this.monitor = None;
                        this.volumes.clear();
                        cx.notify();
                    });
                    return;
                }
            }
        });
        Self {
            monitor: None,
            volumes: Vec::new(),
            icons: HashMap::new(),
            busy: Vec::new(),
            _watch: Some(watch),
        }
    }

    /// Whether there is a UDisks2 to list drives from at all.
    pub fn available(&self) -> bool {
        self.monitor.is_some()
    }

    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }

    pub fn icon(&self, volume: &Volume) -> Option<&Path> {
        self.icons.get(&volume.device).map(PathBuf::as_path)
    }

    pub fn is_busy(&self, volume: &Volume) -> bool {
        self.busy.contains(&volume.device)
    }

    /// The volume mounted at or above `path`, if any: what the sidebar
    /// highlights while a window is somewhere on a drive.
    pub fn volume_containing(&self, path: &Path) -> Option<&Volume> {
        self.volumes
            .iter()
            .filter(|volume| {
                volume.mount_point.as_deref().is_some_and(|point| path.starts_with(point))
            })
            .max_by_key(|volume| volume.mount_point.as_ref().map(|point| point.as_os_str().len()))
    }

    /// Mount a volume, reporting failure on the window that asked and handing
    /// the mount point to `then` on success. A volume already mounted answers
    /// at once.
    pub fn mount(
        &mut self,
        volume: Volume,
        origin: AnyWindowHandle,
        then: impl FnOnce(PathBuf, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        if let Some(point) = &volume.mount_point {
            // Deferred, not called: the window that asked is mid-update when
            // it asks, and `then` updates it again.
            let point = point.clone();
            cx.defer(move |cx| then(point, cx));
            return;
        }
        let Some(monitor) = self.begin(&volume, origin, cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = monitor.mount(&volume).await;
            let _ = this.update(cx, |this, cx| this.finish(&volume, cx));
            match result {
                Ok(mounted) => {
                    if mounted.read_only {
                        let report = Report::Warning(read_only_explanation(&volume));
                        surface::deliver(origin, Some(report), cx);
                    }
                    cx.update(|cx| then(mounted.mount_point, cx));
                }
                Err(error) => surface::deliver(origin, Some(Report::Error(error.to_string())), cx),
            }
        })
        .detach();
    }

    pub fn unmount(&mut self, volume: Volume, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.change(volume, origin, cx, |monitor, volume| async move {
            monitor.unmount(&volume).await.map(|()| VolumeChange::Unmounted)
        });
    }

    pub fn eject(&mut self, volume: Volume, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.change(volume, origin, cx, |monitor, volume| async move {
            monitor.eject(&volume).await.map(|()| VolumeChange::Ejected)
        });
    }

    fn change<F, Fut>(
        &mut self,
        volume: Volume,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
        call: F,
    ) where
        F: FnOnce(Arc<VolumeMonitor>, Volume) -> Fut + 'static,
        Fut: Future<Output = anyhow::Result<VolumeChange>> + 'static,
    {
        let Some(monitor) = self.begin(&volume, origin, cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = call(monitor, volume.clone()).await;
            let _ = this.update(cx, |this, cx| this.finish(&volume, cx));
            let report = match result {
                Ok(VolumeChange::Unmounted) => {
                    Report::Success(format!("Unmounted “{}”", volume.name))
                }
                Ok(VolumeChange::Ejected) => {
                    Report::Success(format!("“{}” can be removed", volume.name))
                }
                Err(error) => Report::Error(error.to_string()),
            };
            surface::deliver(origin, Some(report), cx);
        })
        .detach();
    }

    /// Mark a volume busy and hand back the monitor, or say why not.
    fn begin(
        &mut self,
        volume: &Volume,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Arc<VolumeMonitor>> {
        let refusal = if self.is_busy(volume) {
            Some(format!("“{}” is still being changed; wait a moment", volume.name))
        } else if self.monitor.is_none() {
            Some("Drives are not available: UDisks2 is not on the system bus".to_string())
        } else {
            None
        };
        if let Some(reason) = refusal {
            cx.spawn(async move |_, cx| {
                surface::deliver(origin, Some(Report::Error(reason)), cx);
            })
            .detach();
            return None;
        }
        self.busy.push(volume.device.clone());
        cx.notify();
        self.monitor.clone()
    }

    fn finish(&mut self, volume: &Volume, cx: &mut Context<Self>) {
        self.busy.retain(|device| device != &volume.device);
        cx.notify();
    }
}

/// Why a volume came up read-only, as far as Marcel can tell from outside
/// the kernel. NTFS has one overwhelmingly common cause and a fix the user
/// can apply; anything else gets the honest generic sentence.
fn read_only_explanation(volume: &Volume) -> String {
    if volume.filesystem == "ntfs" {
        format!(
            "“{}” is mounted read-only: Windows left it in a dirty state, which usually means Fast Startup or hibernation. To write to it, turn off Fast Startup in Windows and shut it down fully",
            volume.name
        )
    } else {
        format!(
            "“{}” is mounted read-only: the filesystem refused to be written to, so it may need a check",
            volume.name
        )
    }
}

fn icons_for(volumes: &[Volume]) -> HashMap<PathBuf, PathBuf> {
    let mut provider = IconProvider::discover();
    volumes
        .iter()
        .filter_map(|volume| {
            provider.icon_for_volume(volume.removable).map(|icon| (volume.device.clone(), icon))
        })
        .collect()
}
