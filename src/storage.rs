//! Persistent data directory layout and runtime state.
//!
//! All operations here are blocking filesystem calls; async callers must run them through
//! `tokio::task::spawn_blocking`.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

/// Subdirectories owned by rustinx inside the data directory.
pub const SUBDIRS: [&str; 4] = ["accounts", "certificates", "challenges", "state"];
pub const STATE_FILE: &str = "runtime.json";
pub const SCHEMA_VERSION: u32 = 1;
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("data directory {path} is not accessible (is the volume mounted?): {source}")]
    DataDir { path: PathBuf, source: io::Error },
    #[error("data directory {0} is not a directory")]
    NotDirectory(PathBuf),
    #[error("{0} must be a real directory, not a symbolic link or another file type")]
    UnsafeEntry(PathBuf),
    #[error("I/O error on {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("state file {path} uses schema version {found}, newer than supported version {SCHEMA_VERSION}")]
    UnsupportedSchema { path: PathBuf, found: u32 },
    #[error("failed to encode runtime state: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub schema_version: u32,
    pub first_start_unix: u64,
    pub last_start_unix: u64,
    pub start_count: u64,
}

#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    /// Validates the data directory and creates the private subdirectory layout.
    pub fn open(root: &Path) -> Result<Self, StorageError> {
        let metadata = fs::metadata(root).map_err(|source| StorageError::DataDir {
            path: root.to_owned(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(StorageError::NotDirectory(root.to_owned()));
        }
        for name in SUBDIRS {
            ensure_private_dir(&root.join(name))?;
        }
        Ok(Self {
            root: root.to_owned(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("state").join(STATE_FILE)
    }

    /// Records a supervisor start and persists it atomically. This also proves the volume is writable.
    pub fn record_start(&self, now_unix: u64) -> Result<RuntimeState, StorageError> {
        let path = self.state_path();
        let state = match load_state(&path, now_unix)? {
            Some(previous) => RuntimeState {
                schema_version: SCHEMA_VERSION,
                first_start_unix: previous.first_start_unix,
                last_start_unix: now_unix,
                start_count: previous.start_count.saturating_add(1),
            },
            None => RuntimeState {
                schema_version: SCHEMA_VERSION,
                first_start_unix: now_unix,
                last_start_unix: now_unix,
                start_count: 1,
            },
        };
        let mut bytes = serde_json::to_vec_pretty(&state)?;
        bytes.push(b'\n');
        write_atomic(&path, &bytes)?;
        Ok(state)
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> StorageError + '_ {
    move |source| StorageError::Io {
        path: path.to_owned(),
        source,
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if metadata.permissions().mode() & 0o777 != DIR_MODE {
                fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE)).map_err(io_err(path))?;
            }
            Ok(())
        }
        Ok(_) => Err(StorageError::UnsafeEntry(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .mode(DIR_MODE)
            .create(path)
            .map_err(io_err(path)),
        Err(err) => Err(io_err(path)(err)),
    }
}

/// Loads the state file. A corrupt file is moved aside instead of being overwritten.
fn load_state(path: &Path, now_unix: u64) -> Result<Option<RuntimeState>, StorageError> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_err(path)(err)),
    };
    match serde_json::from_slice::<RuntimeState>(&contents) {
        Ok(state) if state.schema_version > SCHEMA_VERSION => Err(StorageError::UnsupportedSchema {
            path: path.to_owned(),
            found: state.schema_version,
        }),
        Ok(state) => Ok(Some(state)),
        Err(err) => {
            let quarantine = path.with_file_name(format!("{STATE_FILE}.corrupt-{now_unix}"));
            fs::rename(path, &quarantine).map_err(io_err(path))?;
            warn!(
                path = %path.display(),
                moved_to = %quarantine.display(),
                error = %err,
                "runtime state file was corrupt and has been preserved; starting with fresh state"
            );
            Ok(None)
        }
    }
}

/// Writes `bytes` to `path` via a temporary file in the same directory, fsync and rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let dir = path
        .parent()
        .ok_or_else(|| StorageError::UnsafeEntry(path.to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| StorageError::UnsafeEntry(path.to_owned()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        file.write_all(bytes).map_err(io_err(&tmp))?;
        file.sync_all().map_err(io_err(&tmp))?;
        fs::rename(&tmp, path).map_err(io_err(path))?;
        File::open(dir).and_then(|d| d.sync_all()).map_err(io_err(dir))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_private_layout() {
        let dir = tempfile::tempdir().unwrap();
        Storage::open(dir.path()).unwrap();
        for name in SUBDIRS {
            let path = dir.path().join(name);
            assert!(path.is_dir(), "{name} missing");
            assert_eq!(mode(&path), 0o700, "{name} mode");
        }
    }

    #[test]
    fn tightens_existing_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = dir.path().join("accounts");
        fs::create_dir(&accounts).unwrap();
        fs::set_permissions(&accounts, fs::Permissions::from_mode(0o755)).unwrap();
        Storage::open(dir.path()).unwrap();
        assert_eq!(mode(&accounts), 0o700);
    }

    #[test]
    fn rejects_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let err = Storage::open(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(err, StorageError::DataDir { .. }));
    }

    #[test]
    fn rejects_file_as_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            Storage::open(&file).unwrap_err(),
            StorageError::NotDirectory(_)
        ));
    }

    #[test]
    fn rejects_symlinked_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("certificates")).unwrap();
        let err = Storage::open(dir.path()).unwrap_err();
        assert!(matches!(err, StorageError::UnsafeEntry(_)));
    }

    #[test]
    fn records_starts_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = Storage::open(dir.path()).unwrap().record_start(100).unwrap();
        assert_eq!(first.start_count, 1);
        assert_eq!(first.first_start_unix, 100);

        let second = Storage::open(dir.path()).unwrap().record_start(200).unwrap();
        assert_eq!(second.start_count, 2);
        assert_eq!(second.first_start_unix, 100);
        assert_eq!(second.last_start_unix, 200);

        let storage = Storage::open(dir.path()).unwrap();
        assert_eq!(mode(&storage.state_path()), 0o600);
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("state"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    #[test]
    fn corrupt_state_is_preserved_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        fs::write(storage.state_path(), b"{not json").unwrap();
        let state = storage.record_start(300).unwrap();
        assert_eq!(state.start_count, 1);
        let quarantined = dir.path().join("state").join("runtime.json.corrupt-300");
        assert_eq!(fs::read(quarantined).unwrap(), b"{not json");
    }

    #[test]
    fn newer_schema_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let newer = br#"{"schema_version":99,"first_start_unix":1,"last_start_unix":1,"start_count":1}"#;
        fs::write(storage.state_path(), newer).unwrap();
        let err = storage.record_start(400).unwrap_err();
        assert!(matches!(err, StorageError::UnsupportedSchema { found: 99, .. }));
        assert_eq!(fs::read(storage.state_path()).unwrap(), newer);
    }
}
