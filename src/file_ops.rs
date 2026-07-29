use std::{
    collections::VecDeque,
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};

pub const OPERATION_HISTORY_LIMIT: usize = 100;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationRecord {
    CreateDirectory {
        path: PathBuf,
        identity: FileIdentity,
    },
    Copy {
        sources: Vec<PathSnapshot>,
        destination: PathBuf,
        created: Vec<PathSnapshot>,
    },
    Move {
        transfers: Vec<MoveRecord>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSnapshot {
    path: PathBuf,
    identity: FileIdentity,
    kind: SnapshotKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    source: PathBuf,
    destination: PathBuf,
    expected_state: Vec<PathSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMode {
    Copy,
    Move,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct TransferOutcome {
    pub operation: Option<OperationRecord>,
    pub completed: Vec<PathBuf>,
    pub failures: Vec<TransferFailure>,
}

#[derive(Debug, Default)]
pub struct TransferProgress {
    preparing: AtomicBool,
    total_items: AtomicU64,
    completed_items: AtomicU64,
    total_bytes: AtomicU64,
    completed_bytes: AtomicU64,
    current_path: Mutex<Option<PathBuf>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferProgressSnapshot {
    pub preparing: bool,
    pub total_items: u64,
    pub completed_items: u64,
    pub total_bytes: u64,
    pub completed_bytes: u64,
    pub current_path: Option<PathBuf>,
}

impl TransferProgress {
    pub fn snapshot(&self) -> TransferProgressSnapshot {
        TransferProgressSnapshot {
            preparing: self.preparing.load(Ordering::Relaxed),
            total_items: self.total_items.load(Ordering::Relaxed),
            completed_items: self.completed_items.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            completed_bytes: self.completed_bytes.load(Ordering::Relaxed),
            current_path: self
                .current_path
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    fn set_preparing(&self, preparing: bool) {
        self.preparing.store(preparing, Ordering::Relaxed);
    }

    fn add_total(&self, items: u64, bytes: u64) {
        self.total_items.fetch_add(items, Ordering::Relaxed);
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn set_current_path(&self, path: Option<PathBuf>) {
        *self
            .current_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }

    fn complete_item(&self) {
        self.completed_items.fetch_add(1, Ordering::Relaxed);
    }

    fn complete_bytes(&self, bytes: u64) {
        self.completed_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl OperationRecord {
    pub fn path(&self) -> &Path {
        match self {
            Self::CreateDirectory { path, .. } => path,
            Self::Copy {
                destination,
                created,
                ..
            } => created
                .first()
                .map(|snapshot| snapshot.path.as_path())
                .unwrap_or(destination),
            Self::Move { transfers } => transfers
                .first()
                .map(|transfer| transfer.destination.as_path())
                .unwrap_or_else(|| Path::new("")),
        }
    }
}

#[derive(Debug)]
pub struct OperationJournal {
    undo: VecDeque<OperationRecord>,
    redo: VecDeque<OperationRecord>,
    limit: usize,
}

impl Default for OperationJournal {
    fn default() -> Self {
        Self::new(OPERATION_HISTORY_LIMIT)
    }
}

impl OperationJournal {
    pub fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            limit,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn record(&mut self, operation: OperationRecord) {
        self.redo.clear();
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn begin_undo(&mut self) -> Option<OperationRecord> {
        self.undo.pop_back()
    }

    pub fn finish_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
    }

    pub fn cancel_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn begin_redo(&mut self) -> Option<OperationRecord> {
        self.redo.pop_back()
    }

    pub fn finish_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn cancel_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
    }
}

fn push_bounded(stack: &mut VecDeque<OperationRecord>, operation: OperationRecord, limit: usize) {
    if limit == 0 {
        return;
    }
    while stack.len() >= limit {
        stack.pop_front();
    }
    stack.push_back(operation);
}

pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim().is_empty() {
        bail!("Enter a folder name");
    }
    if name == "." || name == ".." {
        bail!("“{name}” is reserved and cannot be used as a folder name");
    }
    if name.contains('/') || name.contains('\0') {
        bail!("Folder names cannot contain “/” or a null character");
    }
    Ok(())
}

pub fn create_directory(parent: &Path, name: &str) -> Result<OperationRecord> {
    validate_entry_name(name)?;
    create_directory_at(parent.join(name))
}

pub fn undo_operation(operation: &OperationRecord) -> Result<OperationRecord> {
    match operation {
        OperationRecord::CreateDirectory { path, identity } => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("Cannot undo: “{}” no longer exists", path.display()))?;
            if !metadata.file_type().is_dir() {
                bail!("Cannot undo: “{}” is no longer a directory", path.display());
            }
            if fs::read_dir(path)
                .with_context(|| format!("Cannot inspect “{}”", path.display()))?
                .next()
                .is_some()
            {
                bail!("Cannot undo: “{}” is no longer empty", path.display());
            }
            if file_identity(&metadata) != *identity {
                bail!("Cannot undo: “{}” changed or was replaced", path.display());
            }
            fs::remove_dir(path)
                .with_context(|| format!("Could not remove “{}”", path.display()))?;
            Ok(operation.clone())
        }
        OperationRecord::Copy { created, .. } => {
            remove_snapshotted_tree(created)?;
            Ok(operation.clone())
        }
        OperationRecord::Move { transfers } => {
            for transfer in transfers {
                validate_snapshot_tree(&transfer.expected_state)?;
                ensure_unoccupied(&transfer.source)?;
            }
            let mut undone = Vec::with_capacity(transfers.len());
            for transfer in transfers.iter().rev() {
                if let Err(error) = rename_no_replace(&transfer.destination, &transfer.source) {
                    let rollback_error = rollback_undone_moves(&undone).err();
                    let message = format!(
                        "Could not move “{}” back to “{}”: {error}",
                        transfer.destination.display(),
                        transfer.source.display()
                    );
                    return match rollback_error {
                        Some(rollback_error) => Err(anyhow::anyhow!(
                            "{message}; rollback also failed: {rollback_error}"
                        )),
                        None => Err(anyhow::anyhow!("{message}; earlier moves were rolled back")),
                    };
                }
                undone.push(MoveRecord {
                    source: transfer.source.clone(),
                    destination: transfer.destination.clone(),
                    expected_state: snapshot_tree(&transfer.source)?,
                });
            }
            undone.reverse();
            Ok(OperationRecord::Move { transfers: undone })
        }
    }
}

pub fn redo_operation(operation: &OperationRecord) -> Result<OperationRecord> {
    match operation {
        OperationRecord::CreateDirectory { path, .. } => create_directory_at(path.clone()),
        OperationRecord::Copy {
            sources,
            destination,
            ..
        } => {
            validate_snapshot_tree(sources)?;
            let source_paths = top_level_paths(sources);
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Copy,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome);
            }
            outcome
                .operation
                .context("Redo did not produce a copy operation")
        }
        OperationRecord::Move { transfers } => {
            for transfer in transfers {
                validate_snapshot_tree(&transfer.expected_state)?;
            }
            let source_paths = transfers
                .iter()
                .map(|transfer| transfer.source.clone())
                .collect::<Vec<_>>();
            let destination = transfers
                .first()
                .and_then(|transfer| transfer.destination.parent())
                .context("Move record has no destination directory")?;
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Move,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome);
            }
            outcome
                .operation
                .context("Redo did not produce a move operation")
        }
    }
}

fn rollback_failed_redo(outcome: TransferOutcome) -> Result<OperationRecord> {
    let failure = summarize_failures(&outcome.failures);
    if let Some(partial) = outcome.operation {
        match undo_operation(&partial) {
            Ok(_) => bail!("{failure}; completed items were rolled back"),
            Err(rollback_error) => {
                bail!("{failure}; rollback also failed: {rollback_error}");
            }
        }
    }
    bail!("{failure}");
}

fn rollback_undone_moves(undone: &[MoveRecord]) -> Result<()> {
    for transfer in undone.iter().rev() {
        rename_no_replace(&transfer.source, &transfer.destination).with_context(|| {
            format!(
                "Could not restore “{}” to “{}”",
                transfer.source.display(),
                transfer.destination.display()
            )
        })?;
    }
    Ok(())
}

pub fn transfer_paths(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
) -> TransferOutcome {
    transfer_paths_impl(sources, destination, mode, cancelled, None)
}

pub fn transfer_paths_with_progress(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Arc<TransferProgress>,
) -> TransferOutcome {
    transfer_paths_impl(sources, destination, mode, cancelled, Some(progress))
}

fn transfer_paths_impl(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<TransferProgress>>,
) -> TransferOutcome {
    // Conceptually follows Yazi's per-item scheduled transfer outcomes,
    // cooperative cancellation, partial-success accounting, and rename-first
    // move path. No Yazi code is copied:
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/worker.rs
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
    let mut completed = Vec::new();
    let mut failures = Vec::new();
    let mut copied_sources = Vec::new();
    let mut copied_created = Vec::new();
    let mut moved = Vec::new();

    if let Some(progress) = &progress {
        progress.set_preparing(true);
        match mode {
            TransferMode::Copy => {
                for source in sources {
                    measure_entry(source, &cancelled, progress);
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
            TransferMode::Move => progress.add_total(sources.len() as u64, 0),
        }
        progress.set_preparing(false);
    }

    for source in sources {
        if cancelled.load(Ordering::Acquire) {
            failures.push(TransferFailure {
                path: source.clone(),
                message: "Operation cancelled".to_string(),
            });
            break;
        }

        let Some(name) = source.file_name() else {
            failures.push(TransferFailure {
                path: source.clone(),
                message: "Source has no file name".to_string(),
            });
            continue;
        };
        let target = destination.join(name);
        if let Some(progress) = &progress {
            progress.set_current_path(Some(source.clone()));
        }
        let result = match mode {
            TransferMode::Copy => copy_one(source, &target, &cancelled, progress.as_deref()).map(
                |(source, created)| {
                    copied_sources.extend(source);
                    copied_created.extend(created);
                },
            ),
            TransferMode::Move => move_one(source, &target).map(|record| {
                moved.push(record);
                if let Some(progress) = &progress {
                    progress.complete_item();
                }
            }),
        };

        match result {
            Ok(()) => completed.push(target),
            Err(error) => failures.push(TransferFailure {
                path: source.clone(),
                message: error.to_string(),
            }),
        }
    }

    let operation = match mode {
        TransferMode::Copy if !copied_created.is_empty() => Some(OperationRecord::Copy {
            sources: copied_sources,
            destination: destination.to_path_buf(),
            created: copied_created,
        }),
        TransferMode::Move if !moved.is_empty() => Some(OperationRecord::Move { transfers: moved }),
        _ => None,
    };

    if let Some(progress) = &progress {
        progress.set_current_path(None);
    }
    TransferOutcome {
        operation,
        completed,
        failures,
    }
}

fn measure_entry(path: &Path, cancelled: &AtomicBool, progress: &TransferProgress) {
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    let kind = metadata.file_type();
    progress.add_total(1, if kind.is_file() { metadata.len() } else { 0 });
    if !kind.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Ok(entry) = entry {
            measure_entry(&entry.path(), cancelled, progress);
        }
    }
}

pub fn summarize_failures(failures: &[TransferFailure]) -> String {
    match failures {
        [] => String::new(),
        [failure] => failure.message.clone(),
        failures => format!(
            "{} items failed; first error: {}",
            failures.len(),
            failures[0].message
        ),
    }
}

fn copy_one(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<(Vec<PathSnapshot>, Vec<PathSnapshot>)> {
    ensure_unoccupied(destination)?;
    if source.is_dir() && destination.starts_with(source) {
        bail!(
            "Cannot copy “{}” into itself",
            source.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    let source_state = snapshot_tree(source)?;
    let staging = staging_path(destination)?;
    if let Err(error) = copy_entry(source, &staging, cancelled, progress) {
        cleanup_staging(&staging);
        return Err(error);
    }
    if let Err(error) = rename_no_replace(&staging, destination) {
        cleanup_staging(&staging);
        return Err(error).with_context(|| {
            format!(
                "Could not publish copy at “{}”; nothing was overwritten",
                destination.display()
            )
        });
    }
    let created_state = snapshot_tree(destination)?;
    Ok((source_state, created_state))
}

fn copy_entry(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Operation cancelled");
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Could not inspect “{}”", source.display()))?;
    let kind = metadata.file_type();
    if let Some(progress) = progress {
        progress.set_current_path(Some(source.to_path_buf()));
    }

    if kind.is_dir() {
        fs::create_dir(destination)
            .with_context(|| format!("Could not create “{}”", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("Could not read “{}”", source.display()))?
        {
            let entry = entry
                .with_context(|| format!("Could not read an entry in “{}”", source.display()))?;
            copy_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                cancelled,
                progress,
            )?;
        }
        fs::set_permissions(destination, metadata.permissions()).with_context(|| {
            format!(
                "Could not preserve permissions on “{}”",
                destination.display()
            )
        })?;
        if let Some(progress) = progress {
            progress.complete_item();
        }
    } else if kind.is_file() {
        copy_file_cancellable(source, destination, cancelled, progress)?;
        fs::set_permissions(destination, metadata.permissions()).with_context(|| {
            format!(
                "Could not preserve permissions on “{}”",
                destination.display()
            )
        })?;
        if let Some(progress) = progress {
            progress.complete_item();
        }
    } else if kind.is_symlink() {
        let target = fs::read_link(source)
            .with_context(|| format!("Could not read link “{}”", source.display()))?;
        std::os::unix::fs::symlink(target, destination)
            .with_context(|| format!("Could not copy link “{}”", source.display()))?;
        if let Some(progress) = progress {
            progress.complete_item();
        }
    } else {
        bail!(
            "Special files are not supported yet: “{}”",
            source.display()
        );
    }
    Ok(())
}

fn copy_file_cancellable(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    let mut input =
        fs::File::open(source).with_context(|| format!("Could not open “{}”", source.display()))?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("Could not create “{}”", destination.display()))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let read = input
            .read(&mut buffer)
            .with_context(|| format!("Could not read “{}”", source.display()))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .with_context(|| format!("Could not write “{}”", destination.display()))?;
        if let Some(progress) = progress {
            progress.complete_bytes(read as u64);
        }
    }
    output
        .sync_all()
        .with_context(|| format!("Could not finish “{}”", destination.display()))
}

fn move_one(source: &Path, destination: &Path) -> Result<MoveRecord> {
    ensure_unoccupied(destination)?;
    if source.is_dir() && destination.starts_with(source) {
        bail!(
            "Cannot move “{}” into itself",
            source.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    rename_no_replace(source, destination).with_context(|| {
        format!(
            "Could not move “{}” to “{}”; cross-filesystem moves are not supported yet",
            source.display(),
            destination.display()
        )
    })?;
    Ok(MoveRecord {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        expected_state: snapshot_tree(destination)?,
    })
}

fn staging_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("Copy destination has no parent directory")?;
    for _ in 0..100 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".marcel-copy-{}-{sequence}", std::process::id()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("Could not reserve a temporary copy path")
}

fn cleanup_staging(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
}

fn snapshot_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    let mut snapshots = Vec::new();
    snapshot_entry(root, &mut snapshots)?;
    Ok(snapshots)
}

fn snapshot_entry(path: &Path, snapshots: &mut Vec<PathSnapshot>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect “{}”", path.display()))?;
    let kind = if metadata.file_type().is_dir() {
        SnapshotKind::Directory
    } else if metadata.file_type().is_file() {
        SnapshotKind::File
    } else if metadata.file_type().is_symlink() {
        SnapshotKind::Symlink
    } else {
        bail!("Special files are not supported yet: “{}”", path.display());
    };
    snapshots.push(PathSnapshot {
        path: path.to_path_buf(),
        identity: file_identity(&metadata),
        kind,
    });
    if kind == SnapshotKind::Directory {
        let mut children = fs::read_dir(path)
            .with_context(|| format!("Could not read “{}”", path.display()))?
            .collect::<io::Result<Vec<_>>>()
            .with_context(|| format!("Could not read an entry in “{}”", path.display()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            snapshot_entry(&child.path(), snapshots)?;
        }
    }
    Ok(())
}

fn validate_snapshot_tree(snapshots: &[PathSnapshot]) -> Result<()> {
    for snapshot in snapshots {
        let metadata = fs::symlink_metadata(&snapshot.path).with_context(|| {
            format!(
                "Cannot continue: “{}” no longer exists",
                snapshot.path.display()
            )
        })?;
        if file_identity(&metadata) != snapshot.identity {
            bail!(
                "Cannot continue: “{}” changed or was replaced",
                snapshot.path.display()
            );
        }
    }
    Ok(())
}

fn remove_snapshotted_tree(snapshots: &[PathSnapshot]) -> Result<()> {
    validate_snapshot_tree(snapshots)?;
    for snapshot in snapshots.iter().rev() {
        match snapshot.kind {
            SnapshotKind::Directory => fs::remove_dir(&snapshot.path),
            SnapshotKind::File | SnapshotKind::Symlink => fs::remove_file(&snapshot.path),
        }
        .with_context(|| format!("Could not remove “{}”", snapshot.path.display()))?;
    }
    Ok(())
}

fn ensure_unoccupied(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "“{}” already exists; nothing was overwritten",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Could not inspect destination “{}”", path.display())),
    }
}

fn top_level_paths(snapshots: &[PathSnapshot]) -> Vec<PathBuf> {
    snapshots
        .iter()
        .filter(|candidate| {
            !snapshots.iter().any(|other| {
                other.kind == SnapshotKind::Directory
                    && other.path != candidate.path
                    && candidate.path.starts_with(&other.path)
            })
        })
        .map(|snapshot| snapshot.path.clone())
        .collect()
}

fn create_directory_at(path: PathBuf) -> Result<OperationRecord> {
    fs::create_dir(&path).with_context(|| format!("Could not create “{}”", path.display()))?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Created “{}” but could not inspect it", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "Created path “{}” is not a directory; refusing to record it",
            path.display()
        );
    }

    Ok(OperationRecord::CreateDirectory {
        path,
        identity: file_identity(&metadata),
    })
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
compile_error!("Marcel's safe file-operation identity checks currently require Unix metadata");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_that_escape_the_parent_or_have_no_name() {
        for name in ["", " ", ".", "..", "nested/folder", "bad\0name"] {
            assert!(validate_entry_name(name).is_err(), "{name:?} was accepted");
        }
        assert!(validate_entry_name(".config").is_ok());
        assert!(validate_entry_name("New Folder").is_ok());
    }

    #[test]
    fn create_never_overwrites_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("occupied"), b"keep me").unwrap();

        assert!(create_directory(root.path(), "occupied").is_err());
        assert_eq!(fs::read(root.path().join("occupied")).unwrap(), b"keep me");
    }

    #[test]
    fn create_undo_and_redo_validate_the_path() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "photos").unwrap();

        undo_operation(&created).unwrap();
        assert!(!created.path().exists());

        let recreated = redo_operation(&created).unwrap();
        assert!(recreated.path().is_dir());
    }

    #[test]
    fn undo_refuses_a_non_empty_created_directory() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "work").unwrap();
        fs::write(created.path().join("important.txt"), b"data").unwrap();

        assert!(undo_operation(&created).is_err());
        assert_eq!(
            fs::read(created.path().join("important.txt")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn undo_refuses_a_replacement_at_the_same_path() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "replace-me").unwrap();
        fs::remove_dir(created.path()).unwrap();
        fs::create_dir(created.path()).unwrap();

        assert!(undo_operation(&created).is_err());
        assert!(created.path().is_dir());
    }

    #[test]
    fn history_is_bounded_and_new_work_clears_redo() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = OperationJournal::new(2);
        let first = create_directory(root.path(), "first").unwrap();
        let second = create_directory(root.path(), "second").unwrap();
        let third = create_directory(root.path(), "third").unwrap();
        journal.record(first);
        journal.record(second.clone());
        journal.record(third.clone());

        assert_eq!(journal.begin_undo(), Some(third.clone()));
        assert_eq!(journal.begin_undo(), Some(second.clone()));
        assert_eq!(journal.begin_undo(), None);

        journal.finish_undo(second.clone());
        assert!(journal.can_redo());
        journal.cancel_undo(third);
        journal.record(second);
        assert!(!journal.can_redo());
    }

    #[test]
    fn recursive_copy_preserves_sources_and_supports_undo_redo() {
        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_parent).unwrap();
        fs::create_dir(&destination).unwrap();
        let album = source_parent.join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("notes.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("notes.txt", album.join("notes-link")).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&album),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(
            fs::read(destination.join("album/notes.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read_link(destination.join("album/notes-link")).unwrap(),
            PathBuf::from("notes.txt")
        );
        assert_eq!(fs::read(album.join("notes.txt")).unwrap(), b"hello");

        let operation = outcome.operation.unwrap();
        let redo_record = undo_operation(&operation).unwrap();
        assert!(!destination.join("album").exists());
        let redone = redo_operation(&redo_record).unwrap();
        assert_eq!(fs::read(redone.path().join("notes.txt")).unwrap(), b"hello");
    }

    #[test]
    fn copy_never_overwrites_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"new").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("source.txt"), b"keep").unwrap();

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.operation.is_none());
        assert_eq!(fs::read(destination.join("source.txt")).unwrap(), b"keep");
    }

    #[test]
    fn copy_undo_refuses_a_modified_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"original").unwrap();
        fs::create_dir(&destination).unwrap();
        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.unwrap();
        fs::write(destination.join("source.txt"), b"changed").unwrap();

        assert!(undo_operation(&operation).is_err());
        assert_eq!(
            fs::read(destination.join("source.txt")).unwrap(),
            b"changed"
        );
    }

    #[test]
    fn move_supports_identity_checked_undo_and_redo() {
        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_parent).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = source_parent.join("move-me.txt");
        fs::write(&source, b"contents").unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        let operation = outcome.operation.unwrap();
        assert!(!source.exists());

        let redo_record = undo_operation(&operation).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        let redone = redo_operation(&redo_record).unwrap();
        assert_eq!(fs::read(redone.path()).unwrap(), b"contents");
        assert!(!source.exists());
    }

    #[test]
    fn move_refuses_to_put_a_directory_inside_itself() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let descendant = source.join("descendant");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&descendant).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &descendant,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.failures.len(), 1);
        assert!(source.is_dir());
    }

    #[test]
    fn cancelled_transfer_does_not_publish_an_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"contents").unwrap();
        fs::create_dir(&destination).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));

        let outcome = transfer_paths(&[source], &destination, TransferMode::Copy, cancelled);
        assert!(outcome.operation.is_none());
        assert!(!destination.join("source.txt").exists());
    }

    #[test]
    fn copy_reports_item_and_byte_progress() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"marcel").unwrap();
        fs::create_dir(&destination).unwrap();
        let progress = Arc::new(TransferProgress::default());

        let outcome = transfer_paths_with_progress(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        );

        assert!(outcome.failures.is_empty());
        assert_eq!(
            progress.snapshot(),
            TransferProgressSnapshot {
                preparing: false,
                total_items: 1,
                completed_items: 1,
                total_bytes: 6,
                completed_bytes: 6,
                current_path: None,
            }
        );
    }
}
