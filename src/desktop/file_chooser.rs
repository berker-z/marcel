//! The `org.freedesktop.impl.portal.FileChooser` backend.
//!
//! Applications no longer open file dialogs themselves; they ask
//! `xdg-desktop-portal`, which forwards the call to whichever backend
//! `portals.conf` names for the interface. This is that backend: a D-Bus
//! service that turns each `OpenFile`, `SaveFile`, and `SaveFiles` call into a
//! [`PickerRequest`], hands it to the application, and blocks the method until
//! a window answers.
//!
//! The wire format is documented in
//! [`docs/file-chooser-portal.md`](../docs/file-chooser-portal.md) and the
//! portal backend reference. Nothing in here touches a window; the application
//! side is `window::open_picker`.

use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_channel::{Receiver, Sender, TrySendError};
use url::Url;
use zbus::{
    object_server::ResponseDispatchNotifier,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::desktop::picker::{
    FileFilter, FilterPattern, PickerMode, PickerRequest, PickerResponse, strip_mnemonic,
};

pub const FILE_CHOOSER_BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.marcel";
pub const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
pub const CLAIM_FILE_CHOOSER_ENV: &str = "MARCEL_CLAIM_FILE_CHOOSER";

/// The interface revision this backend implements. Version 4 is the one that
/// added the `directory` option, which is why it is the floor.
const INTERFACE_VERSION: u32 = 4;

/// How many picker requests may wait for a window at once.
///
/// The frontend serialises requests per application, but every application on
/// the session bus may ask. Past this many, a request is refused rather than
/// queued: a dialog that appears a minute after the click is worse than one
/// that fails.
const REQUEST_QUEUE_CAPACITY: usize = 8;

/// Bounds on caller-supplied lists. A misbehaving application should get an
/// error, not a picker that allocates until the session swaps.
const MAX_FILTERS: usize = 64;
const MAX_PATTERNS_PER_FILTER: usize = 64;
const MAX_SAVE_FILES: usize = 1024;
const MAX_STRING_BYTES: usize = 4096;

pub const RESPONSE_SUCCESS: u32 = 0;
pub const RESPONSE_CANCELLED: u32 = 1;
pub const RESPONSE_OTHER: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    OpenFile,
    SaveFile,
    SaveFiles,
}

/// What a method replies with: the response code and the results dictionary.
type Reply = (u32, HashMap<String, OwnedValue>);

/// Requests that have been asked and not yet answered on the bus.
///
/// GPUI quits when the last window closes, and for a Marcel started by a
/// dialog request the picker is the last window. Its answer is written to
/// the bus from another thread, so without this the process could exit with
/// the reply still in hand and the application see its backend vanish. The
/// count covers a request from the moment it is decoded until the reply has
/// left the process, and the quit hook waits for it to reach zero.
#[derive(Clone, Default)]
pub struct ReplyTracker(Arc<AtomicUsize>);

impl ReplyTracker {
    pub fn pending(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    fn begin(&self) -> PendingReply {
        self.0.fetch_add(1, Ordering::AcqRel);
        PendingReply(self.0.clone())
    }
}

struct PendingReply(Arc<AtomicUsize>);

impl Drop for PendingReply {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(crate) struct FileChooserService {
    requests: Sender<PickerRequest>,
    tracker: ReplyTracker,
}

impl FileChooserService {
    pub(crate) fn new(requests: Sender<PickerRequest>, tracker: ReplyTracker) -> Self {
        Self { requests, tracker }
    }

    async fn run(
        &self,
        connection: &zbus::Connection,
        handle: OwnedObjectPath,
        method: Method,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<Reply>> {
        let pending = self.tracker.begin();
        let server = connection.object_server();
        let (reply, responses) = async_channel::bounded(1);
        let (close, closed) = async_channel::bounded(1);
        let request = decode_request(method, title, &options, reply, closed)
            .map_err(zbus::fdo::Error::InvalidArgs)?;
        let filters = request.filters.clone();

        // The frontend cancels a dialog whose caller went away by calling
        // `Close` on the handle it gave us, so the object has to exist before
        // the window does.
        server
            .at(handle.clone(), RequestObject { close })
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let outcome = match enqueue(&self.requests, request) {
            Ok(()) => responses.recv().await.unwrap_or(PickerResponse::Closed),
            Err(error) => {
                let _ = server.remove::<RequestObject, _>(&handle).await;
                return Err(error);
            }
        };
        let _ = server.remove::<RequestObject, _>(&handle).await;

        // The request stays pending until the reply is on the wire, not
        // merely returned from here.
        let (notifier, sent) = ResponseDispatchNotifier::new(encode_response(outcome, &filters));
        connection
            .executor()
            .spawn(
                async move {
                    sent.await;
                    drop(pending);
                },
                "marcel file-chooser reply",
            )
            .detach();
        Ok(notifier)
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserService {
    // The portal interfaces spell this one in lowercase, unlike every other
    // D-Bus property; zbus would otherwise export `Version`.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        INTERFACE_VERSION
    }

    async fn open_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<Reply>> {
        self.run(connection, handle, Method::OpenFile, title, options)
            .await
    }

    async fn save_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<Reply>> {
        self.run(connection, handle, Method::SaveFile, title, options)
            .await
    }

    async fn save_files(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<Reply>> {
        self.run(connection, handle, Method::SaveFiles, title, options)
            .await
    }
}

/// The per-request object the frontend uses to withdraw a dialog.
struct RequestObject {
    close: Sender<()>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    async fn close(&self) {
        // A second Close, or one after the window answered, has nothing left
        // to do; the buffered slot is enough.
        let _ = self.close.try_send(());
    }
}

fn enqueue(sender: &Sender<PickerRequest>, request: PickerRequest) -> zbus::fdo::Result<()> {
    sender.try_send(request).map_err(|error| match error {
        TrySendError::Full(_) => zbus::fdo::Error::LimitsExceeded(
            "Marcel already has too many file choosers waiting".to_string(),
        ),
        TrySendError::Closed(_) => {
            zbus::fdo::Error::Failed("Marcel's file-chooser receiver has stopped".to_string())
        }
    })
}

/// Serve the interface on `connection` and try to own the backend name.
///
/// Like the generic file-manager name, this is an opt-in extra: refusal is
/// logged and survived, never a startup condition.
pub(crate) async fn serve(
    connection: &zbus::Connection,
    requests: Sender<PickerRequest>,
    tracker: ReplyTracker,
) -> zbus::Result<()> {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};

    connection
        .object_server()
        .at(
            PORTAL_OBJECT_PATH,
            FileChooserService::new(requests, tracker),
        )
        .await?;
    match connection
        .request_name_with_flags(FILE_CHOOSER_BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await?
    {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
        _ => eprintln!(
            "another portal backend owns {FILE_CHOOSER_BUS_NAME}; Marcel keeps running without it"
        ),
    }
    Ok(())
}

pub(crate) fn request_channel() -> (Sender<PickerRequest>, Receiver<PickerRequest>) {
    async_channel::bounded(REQUEST_QUEUE_CAPACITY)
}

fn decode_request(
    method: Method,
    title: String,
    options: &HashMap<String, OwnedValue>,
    reply: Sender<PickerResponse>,
    closed: Receiver<()>,
) -> Result<PickerRequest, String> {
    let directory = bool_option(options, "directory")?.unwrap_or(false);
    let multiple = bool_option(options, "multiple")?.unwrap_or(false);
    let mode = match method {
        Method::OpenFile if directory => PickerMode::OpenDirectories,
        Method::OpenFile => PickerMode::OpenFiles,
        Method::SaveFile => PickerMode::SaveFile,
        Method::SaveFiles => PickerMode::SaveFiles {
            names: save_file_names(options)?,
        },
    };

    let current_file = bytes_option(options, "current_file")?.map(PathBuf::from);
    let start_directory = bytes_option(options, "current_folder")?
        .map(PathBuf::from)
        .or_else(|| {
            current_file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        });
    let current_name = match method {
        Method::SaveFile => string_option(options, "current_name")?
            .filter(|name| !name.is_empty())
            .or_else(|| {
                current_file
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
            }),
        Method::OpenFile | Method::SaveFiles => None,
    };

    let filters = filters_option(options)?;
    let current_filter = current_filter_option(options, &filters)?;
    let accept_label = string_option(options, "accept_label")?
        .map(|label| strip_mnemonic(&label))
        .filter(|label| !label.trim().is_empty());

    Ok(PickerRequest {
        title,
        mode,
        // A save dialog names one file; the option is meaningless there.
        multiple: multiple && method == Method::OpenFile,
        accept_label,
        start_directory,
        current_name,
        filters,
        current_filter,
        reply,
        closed,
    })
}

fn bool_option(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<bool>, String> {
    options
        .get(key)
        .map(|value| bool::try_from(value).map_err(|_| format!("option {key} must be a boolean")))
        .transpose()
}

fn string_option(
    options: &HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(value) = options.get(key) else {
        return Ok(None);
    };
    let value = <&str>::try_from(value).map_err(|_| format!("option {key} must be a string"))?;
    bounded_string(value, key).map(Some)
}

fn bounded_string(value: &str, key: &str) -> Result<String, String> {
    if value.len() > MAX_STRING_BYTES {
        return Err(format!(
            "option {key} is longer than {MAX_STRING_BYTES} bytes"
        ));
    }
    Ok(value.to_string())
}

/// A NUL-terminated byte string, the portal's encoding for file paths.
fn bytes_option(
    options: &HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<OsString>, String> {
    let Some(value) = options.get(key) else {
        return Ok(None);
    };
    let bytes = Vec::<u8>::try_from(value.clone())
        .map_err(|_| format!("option {key} must be a byte string"))?;
    if bytes.len() > MAX_STRING_BYTES {
        return Err(format!(
            "option {key} is longer than {MAX_STRING_BYTES} bytes"
        ));
    }
    Ok(bytestring_to_os(bytes).filter(|value| !value.is_empty()))
}

fn bytestring_to_os(mut bytes: Vec<u8>) -> Option<OsString> {
    if let Some(end) = bytes.iter().position(|byte| *byte == 0) {
        bytes.truncate(end);
    }
    Some(OsString::from_vec(bytes))
}

fn save_file_names(options: &HashMap<String, OwnedValue>) -> Result<Vec<OsString>, String> {
    let Some(value) = options.get("files") else {
        return Err("SaveFiles needs a list of file names".to_string());
    };
    let files = Vec::<Vec<u8>>::try_from(value.clone())
        .map_err(|_| "option files must be a list of byte strings".to_string())?;
    if files.is_empty() {
        return Err("SaveFiles needs at least one file name".to_string());
    }
    if files.len() > MAX_SAVE_FILES {
        return Err(format!(
            "SaveFiles accepts at most {MAX_SAVE_FILES} file names"
        ));
    }
    files
        .into_iter()
        .map(|bytes| {
            if bytes.len() > MAX_STRING_BYTES {
                return Err(format!(
                    "a file name is longer than {MAX_STRING_BYTES} bytes"
                ));
            }
            // The caller asks for a folder and hands over names to put in
            // it. A name carrying a directory is either a mistake or an
            // attempt to write outside the folder the user chose.
            let name = bytestring_to_os(bytes).unwrap_or_default();
            crate::fsops::validate_entry_os_name(&name).map_err(|error| error.to_string())?;
            Ok(name)
        })
        .collect()
}

fn filters_option(options: &HashMap<String, OwnedValue>) -> Result<Vec<FileFilter>, String> {
    let Some(value) = options.get("filters") else {
        return Ok(Vec::new());
    };
    let filters = Vec::<(String, Vec<(u32, String)>)>::try_from(value.clone())
        .map_err(|_| "option filters must be a list of (name, patterns)".to_string())?;
    if filters.len() > MAX_FILTERS {
        return Err(format!("at most {MAX_FILTERS} filters are supported"));
    }
    filters.into_iter().map(decode_filter).collect()
}

fn decode_filter((name, patterns): (String, Vec<(u32, String)>)) -> Result<FileFilter, String> {
    if patterns.len() > MAX_PATTERNS_PER_FILTER {
        return Err(format!(
            "a filter has more than {MAX_PATTERNS_PER_FILTER} patterns"
        ));
    }
    let name = bounded_string(&name, "filters")?;
    let patterns = patterns
        .into_iter()
        .map(|(kind, pattern)| {
            let pattern = bounded_string(&pattern, "filters")?;
            match kind {
                0 => Ok(FilterPattern::Glob(pattern)),
                1 => Ok(FilterPattern::Mime(pattern)),
                other => Err(format!("unknown filter pattern type {other}")),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileFilter::new(name, patterns))
}

fn current_filter_option(
    options: &HashMap<String, OwnedValue>,
    filters: &[FileFilter],
) -> Result<Option<usize>, String> {
    let Some(value) = options.get("current_filter") else {
        return Ok(None);
    };
    let (name, patterns) = <(String, Vec<(u32, String)>)>::try_from(value.clone())
        .map_err(|_| "option current_filter must be a (name, patterns) pair".to_string())?;
    let wanted = decode_filter((name, patterns))?;
    // The spec says the current filter must be one of `filters`; GTK adds it
    // to the list when it is not. Adding it is the friendlier reading.
    Ok(filters.iter().position(|filter| *filter == wanted))
}

fn encode_response(
    response: PickerResponse,
    filters: &[FileFilter],
) -> (u32, HashMap<String, OwnedValue>) {
    let mut results = HashMap::new();
    match response {
        PickerResponse::Chosen { paths, filter } => {
            let uris = paths
                .iter()
                .filter_map(|path| Url::from_file_path(path).ok())
                .map(String::from)
                .collect::<Vec<_>>();
            if let Ok(uris) = OwnedValue::try_from(Value::from(uris)) {
                results.insert("uris".to_string(), uris);
            }
            if let Some(filter) = filter.and_then(|index| filters.get(index))
                && let Ok(filter) = OwnedValue::try_from(encode_filter(filter))
            {
                results.insert("current_filter".to_string(), filter);
            }
            (RESPONSE_SUCCESS, results)
        }
        PickerResponse::Cancelled => (RESPONSE_CANCELLED, results),
        PickerResponse::Closed => (RESPONSE_OTHER, results),
    }
}

fn encode_filter(filter: &FileFilter) -> Value<'static> {
    let patterns = filter
        .patterns
        .iter()
        .map(|pattern| match pattern {
            FilterPattern::Glob(glob) => (0u32, glob.clone()),
            FilterPattern::Mime(mime) => (1u32, mime.clone()),
        })
        .collect::<Vec<_>>();
    Value::from((filter.name.clone(), patterns))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(entries: Vec<(&str, Value<'static>)>) -> HashMap<String, OwnedValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), OwnedValue::try_from(value).unwrap()))
            .collect()
    }

    fn decode(
        method: Method,
        options: &HashMap<String, OwnedValue>,
    ) -> Result<PickerRequest, String> {
        let (reply, _responses) = async_channel::bounded(1);
        let (_close, closed) = async_channel::bounded(1);
        decode_request(method, "Pick".to_string(), options, reply, closed)
    }

    fn bytestring(value: &str) -> Value<'static> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        Value::from(bytes)
    }

    #[test]
    fn an_empty_open_request_picks_one_file_from_home() {
        let request = decode(Method::OpenFile, &HashMap::new()).unwrap();
        assert_eq!(request.mode, PickerMode::OpenFiles);
        assert!(!request.multiple);
        assert_eq!(request.start_directory, None);
        assert!(request.filters.is_empty());
        assert_eq!(request.accept_label, None);
    }

    #[test]
    fn open_options_select_directories_multiples_filters_and_labels() {
        let filters = vec![
            (
                "Images".to_string(),
                vec![
                    (0u32, "*.png".to_string()),
                    (1u32, "image/jpeg".to_string()),
                ],
            ),
            ("All".to_string(), vec![(0u32, "*".to_string())]),
        ];
        let options = self::options(vec![
            ("directory", Value::from(true)),
            ("multiple", Value::from(true)),
            ("accept_label", Value::from("_Choose")),
            ("filters", Value::from(filters.clone())),
            ("current_filter", Value::from(filters[1].clone())),
            ("current_folder", bytestring("/tmp")),
        ]);
        let request = decode(Method::OpenFile, &options).unwrap();
        assert_eq!(request.mode, PickerMode::OpenDirectories);
        assert!(request.multiple);
        assert_eq!(request.accept_label.as_deref(), Some("Choose"));
        assert_eq!(request.start_directory, Some(PathBuf::from("/tmp")));
        assert_eq!(request.filters.len(), 2);
        assert_eq!(
            request.filters[0].patterns,
            vec![
                FilterPattern::Glob("*.png".to_string()),
                FilterPattern::Mime("image/jpeg".to_string()),
            ]
        );
        assert_eq!(request.current_filter, Some(1));
    }

    #[test]
    fn save_options_seed_the_name_from_current_name_or_current_file() {
        let options = self::options(vec![("current_file", bytestring("/tmp/notes/draft.md"))]);
        let request = decode(Method::SaveFile, &options).unwrap();
        assert_eq!(request.mode, PickerMode::SaveFile);
        assert_eq!(request.start_directory, Some(PathBuf::from("/tmp/notes")));
        assert_eq!(request.current_name.as_deref(), Some("draft.md"));

        let options = self::options(vec![
            ("current_name", Value::from("report.pdf")),
            ("current_folder", bytestring("/tmp")),
            ("multiple", Value::from(true)),
        ]);
        let request = decode(Method::SaveFile, &options).unwrap();
        assert_eq!(request.current_name.as_deref(), Some("report.pdf"));
        assert_eq!(request.start_directory, Some(PathBuf::from("/tmp")));
        assert!(!request.multiple, "a save dialog names exactly one file");
    }

    #[test]
    fn save_files_needs_plain_names() {
        let options = self::options(vec![(
            "files",
            Value::from(vec![b"a.txt\0".to_vec(), b"b.txt".to_vec()]),
        )]);
        let request = decode(Method::SaveFiles, &options).unwrap();
        assert_eq!(
            request.mode,
            PickerMode::SaveFiles {
                names: vec![OsString::from("a.txt"), OsString::from("b.txt")]
            }
        );

        assert!(decode(Method::SaveFiles, &HashMap::new()).is_err());
        let traversal = self::options(vec![(
            "files",
            Value::from(vec![b"../escape.txt\0".to_vec()]),
        )]);
        assert!(decode(Method::SaveFiles, &traversal).is_err());
        let empty = self::options(vec![("files", Value::from(Vec::<Vec<u8>>::new()))]);
        assert!(decode(Method::SaveFiles, &empty).is_err());
    }

    #[test]
    fn wrongly_typed_options_are_rejected_rather_than_ignored() {
        let options = self::options(vec![("multiple", Value::from("yes"))]);
        assert!(decode(Method::OpenFile, &options).is_err());
        let options = self::options(vec![(
            "filters",
            Value::from(vec![(2u32, "x".to_string())]),
        )]);
        assert!(decode(Method::OpenFile, &options).is_err());
        let options = self::options(vec![(
            "filters",
            Value::from(vec![("Odd".to_string(), vec![(7u32, "*".to_string())])]),
        )]);
        assert!(decode(Method::OpenFile, &options).is_err());
    }

    #[test]
    fn responses_carry_uris_and_the_active_filter() {
        let filters = vec![FileFilter::new(
            "Images".to_string(),
            vec![FilterPattern::Glob("*.png".to_string())],
        )];
        let (code, results) = encode_response(
            PickerResponse::Chosen {
                paths: vec![PathBuf::from("/tmp/My Photo.png")],
                filter: Some(0),
            },
            &filters,
        );
        assert_eq!(code, RESPONSE_SUCCESS);
        assert_eq!(
            Vec::<String>::try_from(results["uris"].clone()).unwrap(),
            vec!["file:///tmp/My%20Photo.png".to_string()]
        );
        assert_eq!(
            <(String, Vec<(u32, String)>)>::try_from(results["current_filter"].clone()).unwrap(),
            ("Images".to_string(), vec![(0u32, "*.png".to_string())])
        );

        let (code, results) = encode_response(PickerResponse::Cancelled, &filters);
        assert_eq!((code, results.len()), (RESPONSE_CANCELLED, 0));
        let (code, _) = encode_response(PickerResponse::Closed, &filters);
        assert_eq!(code, RESPONSE_OTHER);
    }
}
