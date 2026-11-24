//! One-shot ACME issuance over HTTP-01.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cert_store::{CertStore, CertStoreError, METADATA_SCHEMA_VERSION};
use crate::certificates::{AcmeSettings, CertificateRequest, CertificatesConfig};
use crate::storage::{Storage, StorageError, write_atomic};

#[derive(Debug, Error)]
pub enum IssueError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Certificate(#[from] CertStoreError),
    #[error("{step}: {source}")]
    Io { step: &'static str, source: io::Error },
    #[error("another issuance is using this data directory")]
    Busy,
    #[error("stored ACME credentials belong to a different directory")]
    AccountDirectoryMismatch,
    #[error("ACME request failed while {0}")]
    Acme(&'static str),
    #[error("ACME authorization is {0:?}")]
    Authorization(AuthorizationStatus),
    #[error("ACME server did not offer HTTP-01")]
    MissingChallenge,
    #[error("ACME server returned an invalid HTTP-01 token")]
    InvalidToken,
    #[error("ACME order is {0:?}")]
    Order(OrderStatus),
    #[error("ACME operation timed out")]
    Timeout,
    #[error("issuance cancelled")]
    Cancelled,
    #[error("system time precedes the Unix epoch")]
    Clock,
}

#[derive(Serialize, Deserialize)]
struct StoredAccount {
    directory: String,
    credentials: AccountCredentials,
}

pub async fn issue(
    data_dir: &Path,
    config: &CertificatesConfig,
    shutdown: CancellationToken,
) -> Result<(), IssueError> {
    if config.certificates.is_empty() {
        return Ok(());
    }
    let storage = Storage::open(data_dir)?;
    let _lock = lock_issuance(storage.root())?;
    let challenges = storage.root().join("challenges/http-01");
    // A previous process may have been killed before it could remove its responses.
    clear_challenges(&challenges)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&challenges)
        .map_err(|source| IssueError::Io {
            step: "create challenge directory",
            source,
        })?;

    let store = CertStore::new(storage.root());
    let result = tokio::select! {
        biased;
        () = shutdown.cancelled() => Err(IssueError::Cancelled),
        result = issue_all(config, &store, &challenges) => result,
    };
    let cleanup = clear_challenges(&challenges);
    if let Err(err) = &cleanup {
        tracing::error!(error = %err, "could not clean HTTP-01 responses");
    }
    result.and(cleanup)
}

pub(crate) fn lock_issuance(root: &Path) -> Result<File, IssueError> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("accounts/.issuance.lock"))
        .map_err(|source| IssueError::Io {
            step: "open issuance lock",
            source,
        })?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(IssueError::Busy),
        Err(std::fs::TryLockError::Error(source)) => Err(IssueError::Io {
            step: "acquire issuance lock",
            source,
        }),
    }
}

fn clear_challenges(path: &Path) -> Result<(), IssueError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(IssueError::Io {
            step: "remove challenge directory",
            source,
        }),
    }
}

async fn issue_all(
    config: &CertificatesConfig,
    store: &CertStore,
    challenges: &Path,
) -> Result<(), IssueError> {
    let settings = &config.acme;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| IssueError::Clock)?;
    let timestamp = i64::try_from(now.as_secs()).map_err(|_| IssueError::Clock)?;
    let mut account = None;

    for request in &config.certificates {
        store.recover(&request.id)?;
        let reusable = store.load_metadata(&request.id)?.is_some_and(|metadata| {
            metadata.schema_version == METADATA_SCHEMA_VERSION
                && metadata.domains == request.domains
                && metadata.directory == settings.directory
                && metadata.not_before_unix <= timestamp
                && metadata.not_after_unix > timestamp
                && ["privkey.pem", "cert.pem", "chain.pem", "fullchain.pem"]
                    .iter()
                    .all(|name| store.certificate_dir(&request.id).join(name).is_file())
        });
        if reusable {
            info!(certificate = %request.id, "existing certificate is current");
            continue;
        }
        let account = match &account {
            Some(account) => account,
            None => account.insert(
                tokio::time::timeout(settings.challenge_timeout, open_account(settings, store))
                    .await
                    .map_err(|_| IssueError::Timeout)??,
            ),
        };
        let (chain, key) = tokio::time::timeout(
            settings.challenge_timeout,
            obtain(account, request, challenges, settings.challenge_timeout),
        )
        .await
        .map_err(|_| IssueError::Timeout)??;
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| IssueError::Clock)?;
        store.install(
            &request.id,
            &chain,
            &key,
            &request.domains,
            &settings.directory,
            issued_at.as_secs(),
        )?;
        info!(certificate = %request.id, "certificate issued");
    }
    Ok(())
}

async fn open_account(settings: &AcmeSettings, store: &CertStore) -> Result<Account, IssueError> {
    let builder = acme(
        match &settings.ca_root {
            Some(root) => Account::builder_with_root(root),
            None => Account::builder(),
        },
        "initializing TLS",
    )?;
    if let Some(saved) = store.load_account::<StoredAccount>(&settings.directory)? {
        if saved.directory != settings.directory {
            return Err(IssueError::AccountDirectoryMismatch);
        }
        let account = acme(
            builder.from_credentials(saved.credentials).await,
            "restoring account",
        )?;
        info!("ACME account restored");
        return Ok(account);
    }

    let contact: Vec<&str> = settings.contact.iter().map(String::as_str).collect();
    let (account, credentials) = acme(
        builder
            .create(
                &NewAccount {
                    contact: &contact,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                settings.directory.clone(),
                None,
            )
            .await,
        "registering account",
    )?;
    store.save_account(
        &settings.directory,
        &StoredAccount {
            directory: settings.directory.clone(),
            credentials,
        },
    )?;
    info!("ACME account registered");
    Ok(account)
}

async fn obtain(
    account: &Account,
    request: &CertificateRequest,
    challenges: &Path,
    timeout: Duration,
) -> Result<(String, String), IssueError> {
    let identifiers: Vec<_> = request.domains.iter().cloned().map(Identifier::Dns).collect();
    let mut order = acme(
        account.new_order(&NewOrder::new(&identifiers)).await,
        "creating order",
    )?;
    let mut responses = ChallengeResponses::default();
    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authorization = acme(result, "fetching authorization")?;
        match authorization.status {
            AuthorizationStatus::Valid => continue,
            AuthorizationStatus::Pending => {}
            status => return Err(IssueError::Authorization(status)),
        }
        let mut challenge = authorization
            .challenge(ChallengeType::Http01)
            .ok_or(IssueError::MissingChallenge)?;
        responses.publish(
            challenges,
            &challenge.token,
            challenge.key_authorization().as_str(),
        )?;
        acme(challenge.set_ready().await, "submitting HTTP-01 response")?;
    }

    let retries = RetryPolicy::new().timeout(timeout);
    let status = acme(order.poll_ready(&retries).await, "polling HTTP-01 authorization")?;
    if status != OrderStatus::Ready {
        return Err(IssueError::Order(status));
    }
    let key = acme(order.finalize().await, "finalizing order")?;
    let chain = acme(order.poll_certificate(&retries).await, "downloading certificate")?;
    Ok((chain, key))
}

// The remote server may echo request data in error details. Report the failing stage.
fn acme<T>(result: Result<T, instant_acme::Error>, step: &'static str) -> Result<T, IssueError> {
    result.map_err(|err| match err {
        instant_acme::Error::Timeout(_) => IssueError::Timeout,
        _ => IssueError::Acme(step),
    })
}

#[derive(Default)]
struct ChallengeResponses(Vec<PathBuf>);

impl ChallengeResponses {
    fn publish(&mut self, dir: &Path, token: &str, response: &str) -> Result<(), IssueError> {
        if token.is_empty()
            || token.len() > 255
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(IssueError::InvalidToken);
        }
        let path = dir.join(token);
        self.0.push(path.clone());
        write_atomic(&path, response.as_bytes())?;
        Ok(())
    }
}

impl Drop for ChallengeResponses {
    fn drop(&mut self) {
        for path in &self.0 {
            if let Err(err) = fs::remove_file(path)
                && err.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(error = %err, "could not remove HTTP-01 response");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unresponsive_ca_times_out_and_releases_issuance_lock() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ca = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let data = tempfile::tempdir().unwrap();
        let config = CertificatesConfig::parse(&format!(
            r#"
[acme]
directory = "https://{addr}/dir"
terms_of_service_agreed = true
challenge_timeout_secs = 1

[[certificate]]
id = "site"
domains = ["example.test"]
"#
        ))
        .unwrap();
        let result = issue(data.path(), &config, CancellationToken::new()).await;
        ca.abort();

        assert!(matches!(result, Err(IssueError::Timeout)), "{result:?}");
        assert!(!data.path().join("challenges/http-01").exists());
        assert!(lock_issuance(data.path()).is_ok());
    }
}
