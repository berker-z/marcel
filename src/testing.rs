//! Fixtures the test modules share.

use std::{
    fs,
    os::unix::{fs::PermissionsExt as _, net::UnixListener},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

/// A scratch tree for one test: paths are relative to it, parents are made
/// on demand, and everything goes when it drops.
pub struct Sandbox(tempfile::TempDir);

impl Sandbox {
    pub fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    pub fn root(&self) -> &Path {
        self.0.path()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.0.path().join(relative)
    }

    pub fn dir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    pub fn file(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// A shell script that runs `body`.
    pub fn script(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.file(relative, format!("#!/bin/sh\n{body}\n"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A socket: something Marcel can move but never copy or delete.
    pub fn socket(&self, relative: &str) -> UnixListener {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        UnixListener::bind(path).unwrap()
    }

    /// The names directly under `relative`, sorted.
    pub fn names(&self, relative: &str) -> Vec<String> {
        let mut names = fs::read_dir(self.path(relative))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

pub fn read(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

pub fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Tests that need a permission failure cannot run as root, whom permission
/// bits do not constrain.
pub fn skip_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Make `path` read-only, or writable again.
pub fn seal(path: &Path, sealed: bool) {
    let mode = if sealed { 0o555 } else { 0o755 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
