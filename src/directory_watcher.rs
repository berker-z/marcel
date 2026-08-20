use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use notify::{
    Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher as _,
};

use crate::{directory_session::DirectoryEvent, fs::FileEntry, icons::IconProvider};

const WATCH_COALESCE_IDLE: Duration = Duration::from_millis(250);
const WATCH_COALESCE_MAX: Duration = Duration::from_secs(1);
const WATCH_CANCEL_POLL: Duration = Duration::from_millis(100);
const POLL_FALLBACK_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PATHS_PER_BATCH: usize = 4096;
/// How many raw notify events one coalescing window will hold.
///
/// The raw channel is unbounded, and draining it into a `Vec` for up to a
/// second let a churning directory (a build tree, a log spray) grow that
/// vector without limit. Past this the window closes early; the batch then
/// exceeds `MAX_PATHS_PER_BATCH` or not on its own merits.
const MAX_RAW_EVENTS_PER_BATCH: usize = 65_536;

#[derive(Debug)]
pub enum DirectoryWatcherUpdate {
    Events(Vec<DirectoryEvent>),
    RescanRequired,
}

enum WatcherBackend {
    Recommended(RecommendedWatcher),
    Poll(PollWatcher),
}

impl WatcherBackend {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        match self {
            Self::Recommended(watcher) => watcher.watch(path, RecursiveMode::NonRecursive),
            Self::Poll(watcher) => watcher.watch(path, RecursiveMode::NonRecursive),
        }
    }
}

/// Watch one local directory and publish coalesced, metadata-revalidated
/// changes. This conceptually adapts Yazi's non-recursive primary watcher,
/// polling fallback, 250 ms deduplication window, and upsert/delete
/// revalidation flow:
///
/// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-watcher/src/local/local.rs
///
/// Marcel keeps the backend behind its own typed `DirectoryEvent` interface
/// and does not use Yazi's VFS or event system. No Yazi code is copied.
pub fn watch_directory(
    directory: &Path,
    updates: async_channel::Sender<DirectoryWatcherUpdate>,
    cancelled: Arc<AtomicBool>,
) {
    let (raw_sender, raw_receiver) = mpsc::channel();
    let mut backend = match recommended_watcher(raw_sender.clone()) {
        Ok(mut watcher) => match watcher.watch(directory, RecursiveMode::NonRecursive) {
            Ok(()) => WatcherBackend::Recommended(watcher),
            Err(_) => match polling_watcher(raw_sender.clone()) {
                Ok(watcher) => WatcherBackend::Poll(watcher),
                Err(_) => {
                    let _ = updates.send_blocking(DirectoryWatcherUpdate::RescanRequired);
                    return;
                }
            },
        },
        Err(_) => match polling_watcher(raw_sender.clone()) {
            Ok(watcher) => WatcherBackend::Poll(watcher),
            Err(_) => {
                let _ = updates.send_blocking(DirectoryWatcherUpdate::RescanRequired);
                return;
            }
        },
    };

    if matches!(backend, WatcherBackend::Poll(_)) && backend.watch(directory).is_err() {
        let _ = updates.send_blocking(DirectoryWatcherUpdate::RescanRequired);
        return;
    }

    let mut icons = IconProvider::discover();
    while !cancelled.load(Ordering::Acquire) {
        let first = match raw_receiver.recv_timeout(WATCH_CANCEL_POLL) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let started = Instant::now();
        let mut raw = vec![first];

        while started.elapsed() < WATCH_COALESCE_MAX && raw.len() < MAX_RAW_EVENTS_PER_BATCH {
            let remaining = WATCH_COALESCE_MAX.saturating_sub(started.elapsed());
            let timeout = WATCH_COALESCE_IDLE.min(remaining);
            match raw_receiver.recv_timeout(timeout) {
                Ok(result) => raw.push(result),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if cancelled.load(Ordering::Acquire) {
                return;
            }
        }

        let Some(paths) = collect_changed_paths(directory, raw) else {
            if updates
                .send_blocking(DirectoryWatcherUpdate::RescanRequired)
                .is_err()
            {
                return;
            }
            continue;
        };
        if paths.is_empty() {
            continue;
        }

        let events = revalidate_paths_with_icons(paths, &mut icons);
        if !events.is_empty()
            && updates
                .send_blocking(DirectoryWatcherUpdate::Events(events))
                .is_err()
        {
            return;
        }
    }
}

fn recommended_watcher(
    sender: mpsc::Sender<notify::Result<Event>>,
) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(
        move |result| {
            let _ = sender.send(result);
        },
        Config::default(),
    )
}

fn polling_watcher(sender: mpsc::Sender<notify::Result<Event>>) -> notify::Result<PollWatcher> {
    PollWatcher::new(
        move |result| {
            let _ = sender.send(result);
        },
        Config::default().with_poll_interval(POLL_FALLBACK_INTERVAL),
    )
}

fn collect_changed_paths(
    directory: &Path,
    events: Vec<notify::Result<Event>>,
) -> Option<Vec<PathBuf>> {
    let mut paths = HashSet::new();
    for event in events {
        let event = event.ok()?;
        if event.kind.is_access() {
            continue;
        }
        if matches!(event.kind, EventKind::Any | EventKind::Other) {
            return None;
        }
        for path in event.paths {
            if path == directory {
                return None;
            }
            if path.parent() == Some(directory) {
                paths.insert(path);
                if paths.len() > MAX_PATHS_PER_BATCH {
                    return None;
                }
            }
        }
    }
    Some(paths.into_iter().collect())
}

pub(crate) fn revalidate_paths(paths: Vec<PathBuf>) -> Vec<DirectoryEvent> {
    let mut icons = IconProvider::discover();
    revalidate_paths_with_icons(paths, &mut icons)
}

fn revalidate_paths_with_icons(
    paths: Vec<PathBuf>,
    icons: &mut IconProvider,
) -> Vec<DirectoryEvent> {
    let mut events = Vec::with_capacity(paths.len());
    for path in paths {
        let event = match FileEntry::from_path(&path, icons) {
            Ok(entry) => DirectoryEvent::Changed(entry),
            Err(error) if error.kind() == io::ErrorKind::NotFound => DirectoryEvent::Removed(path),
            Err(_) => return vec![DirectoryEvent::RescanRequired],
        };
        events.push(event);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind};

    #[test]
    fn coalescing_deduplicates_immediate_children_and_ignores_access() {
        let directory = PathBuf::from("/folder");
        let child = directory.join("file.txt");
        let paths = collect_changed_paths(
            &directory,
            vec![
                Ok(Event::new(EventKind::Create(CreateKind::File)).add_path(child.clone())),
                Ok(Event::new(EventKind::Create(CreateKind::Any)).add_path(child.clone())),
                Ok(Event::new(EventKind::Access(AccessKind::Any)).add_path(child.clone())),
                Ok(Event::new(EventKind::Create(CreateKind::File))
                    .add_path(directory.join("nested/ignored.txt"))),
            ],
        )
        .unwrap();

        assert_eq!(paths, vec![child]);
    }

    #[test]
    fn watched_directory_and_backend_errors_require_a_rescan() {
        let directory = PathBuf::from("/folder");
        assert!(
            collect_changed_paths(
                &directory,
                vec![Ok(
                    Event::new(EventKind::Create(CreateKind::Any)).add_path(directory.clone())
                )],
            )
            .is_none()
        );
        assert!(
            collect_changed_paths(&directory, vec![Err(notify::Error::generic("boom"))]).is_none()
        );
    }

    #[test]
    fn revalidation_publishes_upserts_and_deletes() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing.txt");
        let missing = root.path().join("missing.txt");
        std::fs::write(&existing, b"hello").unwrap();
        let mut icons = IconProvider::discover();

        let events =
            revalidate_paths_with_icons(vec![existing.clone(), missing.clone()], &mut icons);

        assert!(events.iter().any(
            |event| matches!(event, DirectoryEvent::Changed(entry) if entry.path == existing)
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DirectoryEvent::Removed(path) if path == &missing))
        );
    }

    #[test]
    fn operation_revalidation_requests_rescan_for_ambiguous_errors() {
        assert_eq!(
            revalidate_paths(vec![PathBuf::from("/")]),
            vec![DirectoryEvent::RescanRequired]
        );
    }

    #[test]
    fn native_revalidation_requests_one_rescan_for_an_ambiguous_batch() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing.txt");
        std::fs::write(&existing, b"hello").unwrap();
        let mut icons = IconProvider::discover();

        assert_eq!(
            revalidate_paths_with_icons(vec![existing, PathBuf::from("/")], &mut icons),
            vec![DirectoryEvent::RescanRequired]
        );
    }
}
