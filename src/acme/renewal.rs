//! Background certificate renewal, with a retry deadline persisted across restarts.

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::certificates::{CertificatesConfig, CertificatesError, RenewalSettings, config_path};
use super::{IssueError, issue_with_clock};
use crate::config::Config;
use crate::nginx::https::{self, HttpsError};
use crate::storage::{StorageError, write_atomic};

#[derive(Debug, Error)]
enum RenewalError {
    #[error(transparent)]
    Issue(#[from] IssueError),
    #[error(transparent)]
    Https(#[from] HttpsError),
    #[error(transparent)]
    Config(#[from] CertificatesError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("RUSTINX_CERTIFICATES_CONF must be an absolute path")]
    RelativePath,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Retry {
    failures: u32,
    next_attempt_unix: u64,
}

impl Retry {
    fn failed(&mut self, now: u64, policy: RenewalSettings, rate_limited: bool) -> u64 {
        let delay = policy
            .retry_initial_secs
            .saturating_mul(2_u64.saturating_pow(self.failures))
            .min(policy.retry_max_secs);
        let delay = if rate_limited {
            delay.max(policy.rate_limit_delay_secs)
        } else {
            delay
        };
        self.failures = self.failures.saturating_add(1);
        self.next_attempt_unix = now.saturating_add(delay);
        delay
    }
}

fn read_retry(path: &Path) -> Result<Retry, RenewalError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Retry::default()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) async fn run(config: &Config, shutdown: CancellationToken) {
    let path = config_path();
    if !path.is_absolute() {
        warn!(target: "rustinx::renewal", error = %RenewalError::RelativePath, "automatic renewal unavailable");
        return;
    }
    let retry_path = config.data_dir.join("state/renewal.json");
    let mut retry = match read_retry(&retry_path) {
        Ok(retry) => retry,
        Err(err) => {
            warn!(target: "rustinx::renewal", error = %err, "could not recover renewal retry state");
            return;
        }
    };
    let mut enabled = false;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let delay = match CertificatesConfig::load(&path) {
            Ok(Some(requests)) if requests.renewal.is_some() => {
                let policy = requests.renewal.expect("renewal configured");
                if !enabled {
                    info!(target: "rustinx::renewal", check_interval_secs = policy.check_interval_secs,
                        "automatic certificate renewal enabled"
                    );
                    enabled = true;
                }
                let now = unix_seconds();
                if retry.next_attempt_unix > now {
                    retry.next_attempt_unix - now
                } else {
                    let result = renew(config, &requests, &shutdown, policy).await;
                    if shutdown.is_cancelled() {
                        return;
                    }
                    let delay = match result {
                        Ok(()) => {
                            debug!(target: "rustinx::renewal", "automatic certificate check completed");
                            retry = Retry::default();
                            policy.check_interval_secs
                        }
                        Err(err) => {
                            let rate_limited = matches!(err, RenewalError::Issue(IssueError::RateLimited(_)));
                            let delay = retry.failed(unix_seconds(), policy, rate_limited);
                            warn!(target: "rustinx::renewal", error = %err, rate_limited, retry_in_secs = delay, "automatic certificate renewal failed");
                            delay
                        }
                    };
                    if let Err(err) = save_retry(&retry_path, &retry) {
                        warn!(target: "rustinx::renewal", error = %err, "could not save renewal retry state");
                    }
                    delay
                }
            }
            Ok(_) => {
                debug!(target: "rustinx::renewal", "automatic certificate renewal disabled");
                return;
            }
            Err(err) => {
                warn!(target: "rustinx::renewal", error = %err, "could not load automatic renewal configuration");
                RenewalSettings::default().retry_initial_secs
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
    }
}

fn save_retry(path: &Path, retry: &Retry) -> Result<(), RenewalError> {
    write_atomic(path, &serde_json::to_vec(retry)?)?;
    Ok(())
}

async fn renew(
    config: &Config,
    requests: &CertificatesConfig,
    shutdown: &CancellationToken,
    policy: RenewalSettings,
) -> Result<(), RenewalError> {
    issue_with_clock(
        &config.data_dir,
        requests,
        shutdown.clone(),
        policy.renew_before_secs,
        SystemTime::now,
    )
    .await?;
    if !shutdown.is_cancelled() && https::requested(requests) {
        https::apply_if_changed(config, requests).await?;
    }
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_increase_to_the_cap_and_rate_limits_use_the_longer_delay() {
        let policy = RenewalSettings {
            retry_initial_secs: 2,
            retry_max_secs: 5,
            rate_limit_delay_secs: 90,
            ..RenewalSettings::default()
        };
        let mut retry = Retry::default();
        assert_eq!(retry.failed(10, policy, false), 2);
        assert_eq!(retry.failed(12, policy, false), 4);
        assert_eq!(retry.failed(16, policy, false), 5);
        assert_eq!(retry.failed(21, policy, true), 90);

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("renewal.json");
        save_retry(&path, &retry).unwrap();
        let restored = read_retry(&path).unwrap();
        assert_eq!(restored.next_attempt_unix, 111);
        assert_eq!(restored.failures, 4);
    }
}
