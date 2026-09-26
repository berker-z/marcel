//! Files on the desktop clipboard, so Ctrl+C in Marcel and Ctrl+V in
//! Nautilus, Dolphin, a terminal, or a chat client move the same files.
//!
//! GPUI's Wayland clipboard offers and accepts text only: it advertises three
//! text types and answers every request with the same string. A file manager
//! needs `x-special/gnome-copied-files` and `text/uri-list` instead, so this
//! module keeps its own Wayland connection on a thread and speaks the
//! data-control protocol (`ext-data-control-v1`, or wlroots' older
//! `zwlr-data-control-unstable-v1`). Unlike `wl_data_device`, data-control
//! can set and watch the selection without keyboard focus or an input serial.
//!
//! Where neither protocol is offered (GNOME, X11) the service reports itself
//! unavailable and the clipboard stays Marcel's own, as it always was.

use std::{
    io::{Read as _, Write as _},
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat::WlSeat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::fsops::TransferMode;

/// Nautilus, Nemo, Caja, and Thunar: `copy` or `cut`, then one URI per line.
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
/// RFC 2483, which everything else reads: Dolphin, browsers, Electron.
const URI_LIST: &str = "text/uri-list";
/// Dolphin's cut flag, alongside `text/uri-list`.
const KDE_CUT_SELECTION: &str = "application/x-kde-cutselection";
/// Paths as text, for a terminal or an editor.
const TEXT_TYPES: [&str; 5] =
    ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// How long another application gets to hand over its file list.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// A file list larger than this is not a file list.
const READ_LIMIT: u64 = 16 * 1024 * 1024;

/// Files staged by Copy or Cut, here or in another application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileClipboard {
    pub mode: TransferMode,
    pub paths: Vec<PathBuf>,
}

/// What the desktop selection holds, as far as Marcel is concerned.
#[derive(Clone, Debug, Default)]
enum Selection {
    /// Nothing, text, an image: nothing Paste can use.
    #[default]
    NoFiles,
    /// Another application offered files and they are still being read.
    Reading,
    Files(FileClipboard),
}

#[derive(Default)]
struct Shared {
    available: bool,
    selection: Selection,
    /// Bumped with every selection change, so a slow read of an old one
    /// cannot overwrite a newer one.
    generation: u64,
}

enum Command {
    Publish(Option<FileClipboard>),
}

struct Service {
    shared: Arc<Mutex<Shared>>,
    commands: Mutex<channel::Sender<Command>>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// Connect to the compositor's clipboard on a thread of its own.
///
/// Until it has connected, and forever if it cannot, the clipboard is
/// Marcel's own.
pub(crate) fn start() {
    if SERVICE.get().is_some() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (sender, receiver) = channel::channel();
    if SERVICE.set(Service { shared: shared.clone(), commands: Mutex::new(sender) }).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("marcel-clipboard".into()).spawn(move || {
        if let Err(error) = run(shared.clone(), receiver) {
            eprintln!("Marcel: files stay on Marcel's own clipboard: {error}");
        }
        lock(&shared).available = false;
    });
    if let Err(error) = spawned {
        eprintln!("Marcel: could not start the clipboard thread: {error}");
    }
}

/// The files on the desktop clipboard, or `None` when there is no desktop
/// clipboard to ask and Marcel's own is the only one.
pub(crate) fn system() -> Option<Option<FileClipboard>> {
    let service = SERVICE.get()?;
    let shared = lock(&service.shared);
    shared.available.then(|| match &shared.selection {
        Selection::Files(files) => Some(files.clone()),
        Selection::NoFiles | Selection::Reading => None,
    })
}

/// Put files on the desktop clipboard, or clear it.
pub(crate) fn publish(files: Option<FileClipboard>) {
    let Some(service) = SERVICE.get() else { return };
    {
        let mut shared = lock(&service.shared);
        if !shared.available {
            return;
        }
        // Answer from what was just staged at once rather than after the
        // compositor's echo, so Paste right after Copy never sees the old one.
        shared.generation += 1;
        shared.selection = files.clone().map_or(Selection::NoFiles, Selection::Files);
    }
    let _ = lock(&service.commands).send(Command::Publish(files));
}

/// Replace `pasted` on the desktop clipboard with `remaining`, unless
/// something else has been copied in the meantime.
///
/// A cut whose items moved is spent; one where some failed keeps the
/// failures so Paste can retry them. Either way, text copied since is left
/// alone.
pub(crate) fn replace_if_current(pasted: &FileClipboard, remaining: Option<FileClipboard>) {
    if system().flatten().as_ref() == Some(pasted) {
        publish(remaining);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// --- What goes on the clipboard -------------------------------------------

/// A marker only this process offers, so its own selection is recognised
/// when the compositor announces it back.
fn own_marker() -> String {
    format!("application/x-marcel-owner;pid={}", std::process::id())
}

fn offered_types(files: &FileClipboard) -> Vec<String> {
    let mut types = vec![GNOME_COPIED_FILES.to_string(), URI_LIST.to_string()];
    if files.mode == TransferMode::Move {
        types.push(KDE_CUT_SELECTION.to_string());
    }
    types.extend(TEXT_TYPES.iter().map(|text| text.to_string()));
    types.push(own_marker());
    types
}

fn file_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(String::from)
}

/// The bytes for one requested type, or `None` for a type never offered.
fn payload(files: &FileClipboard, mime_type: &str) -> Option<Vec<u8>> {
    let uris = || files.paths.iter().filter_map(|path| file_uri(path));
    let text = match mime_type {
        GNOME_COPIED_FILES => {
            let verb = match files.mode {
                TransferMode::Copy => "copy",
                TransferMode::Move => "cut",
            };
            std::iter::once(verb.to_string()).chain(uris()).collect::<Vec<_>>().join("\n")
        }
        URI_LIST => uris().map(|uri| uri + "\r\n").collect(),
        KDE_CUT_SELECTION => "1".to_string(),
        text if TEXT_TYPES.contains(&text) => {
            files.paths.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>().join("\n")
        }
        marker if marker == own_marker() => String::new(),
        _ => return None,
    };
    Some(text.into_bytes())
}

// --- What comes off it ------------------------------------------------------

/// Local paths from URIs, one per line; anything that is not a local file
/// (an `sftp://` a GVfs client copied, a web link) is left out.
fn local_paths<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    lines
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| url::Url::parse(line).ok()?.to_file_path().ok())
        .collect()
}

fn parse_gnome_copied_files(bytes: &[u8]) -> Option<FileClipboard> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    let mode = match lines.next()?.trim() {
        "copy" => TransferMode::Copy,
        "cut" => TransferMode::Move,
        _ => return None,
    };
    let paths = local_paths(lines);
    (!paths.is_empty()).then_some(FileClipboard { mode, paths })
}

fn parse_uri_list(bytes: &[u8], cut: bool) -> Option<FileClipboard> {
    let paths = local_paths(std::str::from_utf8(bytes).ok()?.lines());
    let mode = if cut { TransferMode::Move } else { TransferMode::Copy };
    (!paths.is_empty()).then_some(FileClipboard { mode, paths })
}

// --- The two protocols, behind one face -------------------------------------

enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

/// The types an offer announced, collected before the selection event.
#[derive(Default)]
struct OfferTypes(Mutex<Vec<String>>);

impl Manager {
    fn create_source(&self, qh: &QueueHandle<State>) -> Source {
        match self {
            Self::Ext(manager) => Source::Ext(manager.create_data_source(qh, ())),
            Self::Wlr(manager) => Source::Wlr(manager.create_data_source(qh, ())),
        }
    }
}

impl Device {
    fn set_selection(&self, source: Option<&Source>) {
        match (self, source) {
            (Self::Ext(device), Some(Source::Ext(source))) => device.set_selection(Some(source)),
            (Self::Wlr(device), Some(Source::Wlr(source))) => device.set_selection(Some(source)),
            (Self::Ext(device), _) => device.set_selection(None),
            (Self::Wlr(device), _) => device.set_selection(None),
        }
    }
}

impl Source {
    fn offer(&self, mime_type: String) {
        match self {
            Self::Ext(source) => source.offer(mime_type),
            Self::Wlr(source) => source.offer(mime_type),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }

    fn id(&self) -> wayland_client::backend::ObjectId {
        match self {
            Self::Ext(source) => source.id(),
            Self::Wlr(source) => source.id(),
        }
    }
}

impl Offer {
    fn types(&self) -> Vec<String> {
        let data = match self {
            Self::Ext(offer) => offer.data::<OfferTypes>(),
            Self::Wlr(offer) => offer.data::<OfferTypes>(),
        };
        data.map(|types| lock(&types.0).clone()).unwrap_or_default()
    }

    fn receive(&self, mime_type: &str, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type.to_string(), fd),
            Self::Wlr(offer) => offer.receive(mime_type.to_string(), fd),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

// --- The thread --------------------------------------------------------------

struct State {
    connection: Connection,
    qh: QueueHandle<State>,
    manager: Manager,
    device: Device,
    shared: Arc<Mutex<Shared>>,
    /// The source this process currently offers, and what it holds.
    source: Option<(Source, FileClipboard)>,
    selection_offer: Option<Offer>,
    /// The compositor withdrew the device; the thread has nothing left to do.
    finished: bool,
}

fn run(shared: Arc<Mutex<Shared>>, commands: channel::Channel<Command>) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let (manager, device) =
        if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
            let device = manager.get_data_device(&seat, &qh, ());
            (Manager::Ext(manager), Device::Ext(device))
        } else {
            let manager = globals
                .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
                .map_err(|_| anyhow::anyhow!("the compositor offers no data-control protocol"))?;
            let device = manager.get_data_device(&seat, &qh, ());
            (Manager::Wlr(manager), Device::Wlr(device))
        };

    let mut event_loop = EventLoop::<State>::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(connection.clone(), queue)
        .insert(handle.clone())
        .map_err(|error| anyhow::anyhow!("watching the Wayland connection: {}", error.error))?;
    handle
        .insert_source(commands, |event, _, state| match event {
            channel::Event::Msg(Command::Publish(files)) => state.publish(files),
            channel::Event::Closed => state.finished = true,
        })
        .map_err(|error| anyhow::anyhow!("watching clipboard commands: {}", error.error))?;

    let mut state = State {
        connection,
        qh,
        manager,
        device,
        shared: shared.clone(),
        source: None,
        selection_offer: None,
        finished: false,
    };
    lock(&shared).available = true;
    while !state.finished {
        event_loop.dispatch(None, &mut state)?;
    }
    Err(anyhow::anyhow!("the compositor withdrew the clipboard"))
}

impl State {
    fn publish(&mut self, files: Option<FileClipboard>) {
        if let Some((source, _)) = self.source.take() {
            source.destroy();
        }
        match files {
            Some(files) => {
                let source = self.manager.create_source(&self.qh);
                for mime_type in offered_types(&files) {
                    source.offer(mime_type);
                }
                self.device.set_selection(Some(&source));
                self.source = Some((source, files.clone()));
                self.set_selection(Selection::Files(files));
            }
            None => {
                self.device.set_selection(None);
                self.set_selection(Selection::NoFiles);
            }
        }
        let _ = self.connection.flush();
    }

    fn set_selection(&self, selection: Selection) -> u64 {
        let mut shared = lock(&self.shared);
        shared.generation += 1;
        shared.selection = selection;
        shared.generation
    }

    fn send(&self, source: &Source, mime_type: &str, fd: OwnedFd) {
        let Some(bytes) = self
            .source
            .as_ref()
            .filter(|(current, _)| current.id() == source.id())
            .and_then(|(_, files)| payload(files, mime_type))
        else {
            return;
        };
        // A reader that stops reading must not stall the clipboard, so the
        // write happens beside it; closing the pipe ends the transfer.
        let _ = std::thread::Builder::new().name("marcel-clipboard-send".into()).spawn(move || {
            let _ = std::fs::File::from(fd).write_all(&bytes);
        });
    }

    fn cancelled(&mut self, source: &Source) {
        source.destroy();
        if self.source.as_ref().is_some_and(|(current, _)| current.id() == source.id()) {
            self.source = None;
        }
    }

    fn selection(&mut self, offer: Option<Offer>) {
        if let Some(previous) = self.selection_offer.take() {
            previous.destroy();
        }
        let types = offer.as_ref().map(Offer::types).unwrap_or_default();
        let has = |mime_type: &str| types.iter().any(|offered| offered == mime_type);
        if has(&own_marker()) {
            if let Some((_, files)) = &self.source {
                self.set_selection(Selection::Files(files.clone()));
            }
        } else if let Some(offer) =
            offer.as_ref().filter(|_| has(GNOME_COPIED_FILES) || has(URI_LIST))
        {
            let generation = self.set_selection(Selection::Reading);
            self.read_files(offer, has(GNOME_COPIED_FILES), has(KDE_CUT_SELECTION), generation);
        } else {
            self.set_selection(Selection::NoFiles);
        }
        self.selection_offer = offer;
    }

    /// Ask the offering application for its file list and read it beside
    /// the event loop, which has to keep answering the compositor meanwhile.
    fn read_files(&self, offer: &Offer, gnome: bool, kde_cut: bool, generation: u64) {
        let Some(list) = request(offer, if gnome { GNOME_COPIED_FILES } else { URI_LIST }) else {
            self.set_selection(Selection::NoFiles);
            return;
        };
        let cut = (!gnome && kde_cut).then(|| request(offer, KDE_CUT_SELECTION)).flatten();
        let _ = self.connection.flush();
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("marcel-clipboard-read".into()).spawn(move || {
            let files = read_pipe(list).and_then(|bytes| {
                if gnome {
                    parse_gnome_copied_files(&bytes)
                } else {
                    let cut = cut.and_then(read_pipe).is_some_and(|flag| flag.trim_ascii() == b"1");
                    parse_uri_list(&bytes, cut)
                }
            });
            let mut shared = lock(&shared);
            if shared.generation == generation {
                shared.selection = files.map_or(Selection::NoFiles, Selection::Files);
            }
        });
    }
}

/// Ask for one type; the returned end yields it once the offerer writes.
fn request(offer: &Offer, mime_type: &str) -> Option<std::io::PipeReader> {
    let (reader, writer) = std::io::pipe().ok()?;
    offer.receive(mime_type, writer.as_fd());
    Some(reader)
}

/// Everything the offerer writes, or `None` when it takes too long.
fn read_pipe(mut reader: std::io::PipeReader) -> Option<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("marcel-clipboard-pipe".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let read = (&mut reader).take(READ_LIMIT).read_to_end(&mut bytes);
            let _ = sender.send(read.map(|_| bytes));
        })
        .ok()?;
    receiver.recv_timeout(READ_TIMEOUT).ok()?.ok()
}

// --- Dispatch ------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The two protocols' events are the same events under different names.
macro_rules! data_control_dispatch {
    ($variant:ident, $manager:ty, $device:ty, $device_mod:ident, $source:ty, $source_mod:ident, $offer:ty, $offer_mod:ident) => {
        impl Dispatch<$manager, ()> for State {
            fn event(_: &mut Self, _: &$manager, _: <$manager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }

        impl Dispatch<$device, ()> for State {
            fn event(state: &mut Self, _: &$device, event: $device_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    $device_mod::Event::Selection { id } => state.selection(id.map(Offer::$variant)),
                    $device_mod::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                    $device_mod::Event::Finished => state.finished = true,
                    _ => {}
                }
            }

            event_created_child!(State, $device, [
                $device_mod::EVT_DATA_OFFER_OPCODE => ($offer, OfferTypes::default()),
            ]);
        }

        impl Dispatch<$offer, OfferTypes> for State {
            fn event(_: &mut Self, _: &$offer, event: $offer_mod::Event, types: &OfferTypes, _: &Connection, _: &QueueHandle<Self>) {
                if let $offer_mod::Event::Offer { mime_type } = event {
                    lock(&types.0).push(mime_type);
                }
            }
        }

        impl Dispatch<$source, ()> for State {
            fn event(state: &mut Self, source: &$source, event: $source_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                let source = Source::$variant(source.clone());
                match event {
                    $source_mod::Event::Send { mime_type, fd } => state.send(&source, &mime_type, fd),
                    $source_mod::Event::Cancelled => state.cancelled(&source),
                    _ => {}
                }
            }
        }
    };
}

data_control_dispatch!(
    Ext,
    ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ext_data_control_device_v1,
    ExtDataControlSourceV1,
    ext_data_control_source_v1,
    ExtDataControlOfferV1,
    ext_data_control_offer_v1
);
data_control_dispatch!(
    Wlr,
    ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    zwlr_data_control_device_v1,
    ZwlrDataControlSourceV1,
    zwlr_data_control_source_v1,
    ZwlrDataControlOfferV1,
    zwlr_data_control_offer_v1
);

#[cfg(test)]
mod tests {
    use super::*;

    fn files(mode: TransferMode, paths: &[&str]) -> FileClipboard {
        FileClipboard { mode, paths: paths.iter().map(PathBuf::from).collect() }
    }

    fn text(files: &FileClipboard, mime_type: &str) -> String {
        String::from_utf8(payload(files, mime_type).unwrap()).unwrap()
    }

    #[test]
    fn a_cut_reads_as_a_cut_to_nautilus_and_dolphin() {
        let cut = files(TransferMode::Move, &["/tmp/My Photo.png", "/tmp/b"]);
        assert_eq!(
            text(&cut, GNOME_COPIED_FILES),
            "cut\nfile:///tmp/My%20Photo.png\nfile:///tmp/b"
        );
        assert_eq!(text(&cut, URI_LIST), "file:///tmp/My%20Photo.png\r\nfile:///tmp/b\r\n");
        assert_eq!(text(&cut, KDE_CUT_SELECTION), "1");
        assert!(offered_types(&cut).contains(&KDE_CUT_SELECTION.to_string()));
        assert!(
            !offered_types(&files(TransferMode::Copy, &["/a"]))
                .contains(&KDE_CUT_SELECTION.to_string())
        );
    }

    #[test]
    fn a_text_paste_gets_plain_paths() {
        let copy = files(TransferMode::Copy, &["/tmp/My Photo.png", "/tmp/b"]);
        assert_eq!(text(&copy, "text/plain;charset=utf-8"), "/tmp/My Photo.png\n/tmp/b");
        assert_eq!(payload(&copy, "image/png"), None);
    }

    #[test]
    fn what_marcel_writes_it_reads_back() {
        for mode in [TransferMode::Copy, TransferMode::Move] {
            let staged = files(mode, &["/tmp/My Photo.png", "/tmp/ünï"]);
            let gnome = payload(&staged, GNOME_COPIED_FILES).unwrap();
            assert_eq!(parse_gnome_copied_files(&gnome), Some(staged.clone()));
            let list = payload(&staged, URI_LIST).unwrap();
            assert_eq!(parse_uri_list(&list, mode == TransferMode::Move), Some(staged));
        }
    }

    #[test]
    fn non_utf8_names_survive_the_round_trip() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9");
        let staged = FileClipboard { mode: TransferMode::Copy, paths: vec![PathBuf::from(name)] };
        let gnome = payload(&staged, GNOME_COPIED_FILES).unwrap();
        assert_eq!(parse_gnome_copied_files(&gnome), Some(staged));
    }

    #[test]
    fn only_local_files_are_pasted() {
        let list = b"# comment\r\nhttps://example.com/\r\nsftp://host/home/a\r\nfile:///tmp/a\r\nfile://localhost/tmp/b\r\n";
        assert_eq!(
            parse_uri_list(list, false),
            Some(files(TransferMode::Copy, &["/tmp/a", "/tmp/b"]))
        );
        assert_eq!(parse_uri_list(b"https://example.com/\r\n", false), None);
        assert_eq!(parse_gnome_copied_files(b"cut\nsftp://host/a"), None);
        assert_eq!(parse_gnome_copied_files(b"move\nfile:///tmp/a"), None);
        // Nautilus writes a trailing newline some versions and not others.
        assert_eq!(
            parse_gnome_copied_files(b"copy\nfile:///tmp/a\n"),
            Some(files(TransferMode::Copy, &["/tmp/a"]))
        );
    }
}
