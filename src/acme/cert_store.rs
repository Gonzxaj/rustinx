//! Persistent ACME account credentials and certificates under the data directory.
//! All functions are blocking; call them from `spawn_blocking`.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use x509_parser::pem::parse_x509_pem;

use crate::storage::{read_owned_file, write_atomic};

pub const METADATA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum CertStoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
    #[error("{path}: invalid JSON: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid certificate storage ID")]
    InvalidId,
    #[error("{0} must be a real certificate directory")]
    UnsafeDirectory(PathBuf),
    #[error("invalid certificate chain: {0}")]
    InvalidChain(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CertStoreError + '_ {
    move |source| CertStoreError::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateMetadata {
    pub schema_version: u32,
    pub domains: Vec<String>,
    pub directory: String,
    pub issued_at_unix: u64,
    pub not_before_unix: i64,
    pub not_after_unix: i64,
}

#[derive(Debug, Clone)]
pub struct CertStore {
    accounts: PathBuf,
    certificates: PathBuf,
}

impl CertStore {
    /// `data_dir` must already be initialized by `storage::Storage::open`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            accounts: data_dir.join("accounts"),
            certificates: data_dir.join("certificates"),
        }
    }

    /// One account per ACME directory, so staging and production never share credentials.
    pub fn account_path(&self, directory: &str) -> PathBuf {
        let name: String = directory
            .trim_start_matches("https://")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.accounts.join(format!("{name}.json"))
    }

    pub fn load_account<T: for<'de> Deserialize<'de>>(
        &self,
        directory: &str,
    ) -> Result<Option<T>, CertStoreError> {
        let path = self.account_path(directory);
        match read_owned_file(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| CertStoreError::Json { path, source }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CertStoreError::Io { path, source }),
        }
    }

    pub fn save_account<T: Serialize>(&self, directory: &str, credentials: &T) -> Result<(), CertStoreError> {
        let path = self.account_path(directory);
        let bytes = serde_json::to_vec_pretty(credentials).map_err(|source| CertStoreError::Json {
            path: path.clone(),
            source,
        })?;
        write_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn certificate_dir(&self, id: &str) -> PathBuf {
        self.certificates.join(id)
    }

    pub fn load_metadata(&self, id: &str) -> Result<Option<CertificateMetadata>, CertStoreError> {
        let dir = self.checked_certificate_dir(id)?;
        let path = dir.join("metadata.json");
        match read_owned_file(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| CertStoreError::Json { path, source }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CertStoreError::Io { path, source }),
        }
    }

    fn checked_certificate_dir(&self, id: &str) -> Result<PathBuf, CertStoreError> {
        if !super::certificates::valid_id(id) {
            return Err(CertStoreError::InvalidId);
        }
        let dir = self.certificate_dir(id);
        actual_directory(&dir)?;
        Ok(dir)
    }

    /// Stores a new certificate. The previous one stays in place until the new directory is complete,
    /// so a failure never leaves a partial or missing certificate.
    pub fn install(
        &self,
        id: &str,
        chain_pem: &str,
        key_pem: &str,
        domains: &[String],
        directory: &str,
        now_unix: u64,
    ) -> Result<CertificateMetadata, CertStoreError> {
        let (cert_pem, rest_pem, not_before, not_after) = split_chain(chain_pem)?;
        let metadata = CertificateMetadata {
            schema_version: METADATA_SCHEMA_VERSION,
            domains: domains.to_vec(),
            directory: directory.to_owned(),
            issued_at_unix: now_unix,
            not_before_unix: not_before,
            not_after_unix: not_after,
        };
        self.recover(id)?;
        let final_dir = self.checked_certificate_dir(id)?;
        let staging = self.certificates.join(format!(".{id}.new"));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&staging)
            .map_err(io_err(&staging))?;

        let metadata_json = serde_json::to_vec_pretty(&metadata).map_err(|source| CertStoreError::Json {
            path: staging.join("metadata.json"),
            source,
        })?;
        let files: [(&str, &[u8]); 5] = [
            ("privkey.pem", key_pem.as_bytes()),
            ("cert.pem", cert_pem.as_bytes()),
            ("chain.pem", rest_pem.as_bytes()),
            ("fullchain.pem", chain_pem.as_bytes()),
            ("metadata.json", &metadata_json),
        ];
        let written = files
            .iter()
            .try_for_each(|(name, bytes)| write_atomic(&staging.join(name), bytes));
        if let Err(err) = written {
            let _ = fs::remove_dir_all(&staging);
            return Err(err.into());
        }

        if actual_directory(&final_dir)? {
            // Publish the complete set in one operation, keeping the live path present.
            let dir = fs::File::open(&self.certificates).map_err(io_err(&self.certificates))?;
            rustix::fs::renameat_with(
                &dir,
                &staging,
                &dir,
                &final_dir,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .map_err(|err| io_err(&final_dir)(err.into()))?;
        } else {
            fs::rename(&staging, &final_dir).map_err(io_err(&final_dir))?;
        }
        sync_dir(&self.certificates)?;
        remove_dir_if_exists(&staging)?;
        Ok(metadata)
    }

    /// Restores a certificate left aside by an interrupted `install` and removes incomplete ones.
    pub fn recover(&self, id: &str) -> Result<(), CertStoreError> {
        let final_dir = self.checked_certificate_dir(id)?;
        let previous = self.certificates.join(format!(".{id}.old"));
        if !actual_directory(&final_dir)? && actual_directory(&previous)? {
            fs::rename(&previous, &final_dir).map_err(io_err(&previous))?;
            sync_dir(&self.certificates)?;
        }
        remove_dir_if_exists(&previous)?;
        remove_dir_if_exists(&self.certificates.join(format!(".{id}.new")))
    }
}

/// Returns (leaf PEM, intermediates PEM, not_before, not_after) after checking every block parses.
/// Blocks are copied verbatim instead of being re-encoded.
fn split_chain(chain_pem: &str) -> Result<(String, String, i64, i64), CertStoreError> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut blocks = Vec::new();
    let mut rest = chain_pem;
    while let Some(start) = rest.find(BEGIN) {
        let end = rest[start..]
            .find(END)
            .ok_or_else(|| CertStoreError::InvalidChain("unterminated PEM block".to_owned()))?
            + start
            + END.len();
        blocks.push(format!("{}\n", &rest[start..end]));
        rest = &rest[end..];
    }
    if !rest.trim().is_empty() && blocks.is_empty() {
        return Err(CertStoreError::InvalidChain("no certificate found".to_owned()));
    }
    let leaf = blocks
        .first()
        .ok_or_else(|| CertStoreError::InvalidChain("no certificate found".to_owned()))?;
    let mut validity = None;
    for block in &blocks {
        let (_, pem) =
            parse_x509_pem(block.as_bytes()).map_err(|err| CertStoreError::InvalidChain(err.to_string()))?;
        let x509 = pem
            .parse_x509()
            .map_err(|err| CertStoreError::InvalidChain(err.to_string()))?;
        if validity.is_none() {
            let v = x509.validity();
            validity = Some((v.not_before.timestamp(), v.not_after.timestamp()));
        }
    }
    let (not_before, not_after) = validity.expect("at least one block");
    Ok((leaf.clone(), blocks[1..].concat(), not_before, not_after))
}

fn actual_directory(path: &Path) -> Result<bool, CertStoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(CertStoreError::UnsafeDirectory(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io_err(path)(err)),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<(), CertStoreError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CertStoreError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn sync_dir(path: &Path) -> Result<(), CertStoreError> {
    fs::File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(io_err(path))
}

#[cfg(test)]
pub(crate) fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    /// Self-signed test certificate generated with rcgen (not a secret).
    fn test_chain(names: &[&str]) -> (String, String) {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let cert = rcgen::generate_simple_self_signed(names).unwrap();
        let pem = cert.cert.pem();
        (format!("{pem}{pem}"), cert.signing_key.serialize_pem())
    }

    fn store() -> (tempfile::TempDir, CertStore) {
        let dir = tempfile::tempdir().unwrap();
        Storage::open(dir.path()).unwrap();
        let store = CertStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn recovery_rejects_path_traversal_and_symlinked_certificate() {
        let data = tempfile::tempdir().unwrap();
        crate::storage::Storage::open(data.path()).unwrap();
        let store = CertStore::new(data.path());
        assert!(matches!(
            store.recover("../state"),
            Err(CertStoreError::InvalidId)
        ));
        std::os::unix::fs::symlink(data.path().join("state"), store.certificate_dir("example")).unwrap();
        assert!(matches!(
            store.recover("example"),
            Err(CertStoreError::UnsafeDirectory(_))
        ));
    }

    #[test]
    fn account_round_trip_is_private() {
        let (_dir, store) = store();
        let url = "https://acme.example/dir";
        assert!(store.load_account::<serde_json::Value>(url).unwrap().is_none());
        store
            .save_account(url, &serde_json::json!({"id": "acct"}))
            .unwrap();
        let path = store.account_path(url);
        assert_eq!(path.file_name().unwrap(), "acme.example_dir.json");
        assert_eq!(mode(&path), 0o600);
        let loaded: serde_json::Value = store.load_account(url).unwrap().unwrap();
        assert_eq!(loaded["id"], "acct");
    }

    #[test]
    fn install_writes_all_files_with_private_key_protected() {
        let (_dir, store) = store();
        let (chain, key) = test_chain(&["example.com"]);
        let domains = vec!["example.com".to_owned()];
        let meta = store
            .install("example", &chain, &key, &domains, "https://ca/dir", 100)
            .unwrap();
        let dir = store.certificate_dir("example");
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("privkey.pem")), 0o600);
        assert_eq!(fs::read_to_string(dir.join("fullchain.pem")).unwrap(), chain);
        let cert = fs::read_to_string(dir.join("cert.pem")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("chain.pem")).unwrap(), cert);
        assert_eq!(format!("{cert}{cert}"), chain);
        assert!(meta.not_after_unix > meta.not_before_unix);
        assert_eq!(store.load_metadata("example").unwrap(), Some(meta));
    }

    #[test]
    fn invalid_chain_keeps_previous_certificate() {
        let (_dir, store) = store();
        let (chain, key) = test_chain(&["example.com"]);
        let domains = vec!["example.com".to_owned()];
        store
            .install("example", &chain, &key, &domains, "https://ca/dir", 1)
            .unwrap();
        let err = store.install("example", "garbage", &key, &domains, "https://ca/dir", 2);
        assert!(matches!(err, Err(CertStoreError::InvalidChain(_))));
        let dir = store.certificate_dir("example");
        assert_eq!(fs::read_to_string(dir.join("fullchain.pem")).unwrap(), chain);
        assert_eq!(store.load_metadata("example").unwrap().unwrap().issued_at_unix, 1);
    }

    #[test]
    fn replacement_swaps_and_cleans_up() {
        let (dir, store) = store();
        let domains = vec!["example.com".to_owned()];
        let (chain1, key1) = test_chain(&["example.com"]);
        let (chain2, key2) = test_chain(&["example.com"]);
        store
            .install("example", &chain1, &key1, &domains, "https://ca/dir", 1)
            .unwrap();
        store
            .install("example", &chain2, &key2, &domains, "https://ca/dir", 2)
            .unwrap();
        let cert_dir = store.certificate_dir("example");
        assert_eq!(fs::read_to_string(cert_dir.join("privkey.pem")).unwrap(), key2);
        let entries: Vec<_> = fs::read_dir(dir.path().join("certificates"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["example"]);
    }

    #[test]
    fn recover_restores_interrupted_install() {
        let (dir, store) = store();
        let (chain, key) = test_chain(&["example.com"]);
        store
            .install(
                "example",
                &chain,
                &key,
                &["example.com".to_owned()],
                "https://ca/dir",
                1,
            )
            .unwrap();
        let certs = dir.path().join("certificates");
        fs::rename(certs.join("example"), certs.join(".example.old")).unwrap();
        fs::create_dir(certs.join(".example.new")).unwrap();
        store.recover("example").unwrap();
        assert!(certs.join("example/fullchain.pem").exists());
        assert!(!certs.join(".example.old").exists());
        assert!(!certs.join(".example.new").exists());
    }
}
