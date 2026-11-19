//! Nginx lifecycle: validate, start, wait for readiness, monitor and stop.

use std::io;
use std::process::ExitStatus;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::sys::signal::Signal;
use thiserror::Error;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::health;
use crate::nginx::{Nginx, NginxError, NginxProcess};
use crate::storage::{Storage, StorageError};

pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_CONFIG: u8 = 2;
pub const EXIT_NGINX: u8 = 3;

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Nginx(#[from] NginxError),
    #[error("nginx exited during startup ({0})")]
    StartupExit(ExitStatus),
    #[error("nginx did not become healthy within {0:?}")]
    StartupTimeout(Duration),
    #[error("nginx exited unexpectedly ({0})")]
    UnexpectedExit(ExitStatus),
    #[error("failed to manage the nginx process: {0}")]
    Process(#[from] io::Error),
    #[error("internal task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl SupervisorError {
    /// Process exit code for this error; documented in README.md.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Nginx(NginxError::InvalidConfig { .. }) => EXIT_CONFIG,
            Self::StartupExit(_) | Self::StartupTimeout(_) | Self::UnexpectedExit(_) => EXIT_NGINX,
            _ => EXIT_FAILURE,
        }
    }
}

/// Runs until `shutdown` is cancelled (returns `Ok`) or Nginx fails (returns `Err`).
/// Nginx is never left running when this function returns.
pub async fn run(config: &Config, shutdown: CancellationToken) -> Result<(), SupervisorError> {
    let data_dir = config.data_dir.clone();
    let state =
        tokio::task::spawn_blocking(move || Storage::open(&data_dir)?.record_start(unix_now())).await??;
    info!(
        data_dir = %config.data_dir.display(),
        start_count = state.start_count,
        "persistent storage ready"
    );

    let nginx = Nginx::new(config.nginx_bin.clone(), config.nginx_conf.clone());
    let output = tokio::select! {
        result = nginx.test_config(config.startup_timeout) => result?,
        _ = shutdown.cancelled() => return Ok(()),
    };
    info!(conf = %config.nginx_conf.display(), "nginx configuration is valid");
    if !output.is_empty() {
        debug!(output = %output, "nginx -t output");
    }

    let mut process = nginx.spawn()?;
    info!(pid = process.pid(), "nginx started");

    let started = Instant::now();
    let ready = tokio::select! {
        status = process.wait() => return Err(SupervisorError::StartupExit(status?)),
        ready = wait_until_healthy(config) => ready,
        _ = shutdown.cancelled() => {
            stop(&mut process, config.shutdown_timeout).await?;
            return Ok(());
        }
    };
    if !ready {
        error!(timeout = ?config.startup_timeout, "nginx did not become healthy; stopping it");
        stop(&mut process, config.shutdown_timeout).await?;
        return Err(SupervisorError::StartupTimeout(config.startup_timeout));
    }
    info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "nginx is healthy"
    );

    tokio::select! {
        status = process.wait() => Err(SupervisorError::UnexpectedExit(status?)),
        _ = shutdown.cancelled() => {
            stop(&mut process, config.shutdown_timeout).await?;
            Ok(())
        }
    }
}

async fn wait_until_healthy(config: &Config) -> bool {
    let deadline = Instant::now() + config.startup_timeout;
    loop {
        match health::probe(config.health_addr, PROBE_TIMEOUT).await {
            Ok(()) => return true,
            Err(err) => debug!(error = %err, "nginx not ready yet"),
        }
        if Instant::now() + READINESS_POLL_INTERVAL >= deadline {
            return false;
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

/// Graceful stop: SIGQUIT lets workers finish in-flight requests; SIGKILL after `timeout`.
async fn stop(process: &mut NginxProcess, timeout: Duration) -> Result<(), SupervisorError> {
    let pid = process.pid();
    if process.signal(Signal::SIGQUIT)? {
        info!(pid, timeout = ?timeout, "graceful nginx shutdown requested (SIGQUIT)");
    }
    match tokio::time::timeout(timeout, process.wait()).await {
        Ok(status) => info!(pid, status = %status?, "nginx stopped"),
        Err(_) => {
            warn!(pid, timeout = ?timeout, "nginx did not stop in time; sending SIGKILL");
            process.kill().await?;
            info!(pid, "nginx killed");
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn exit_codes() {
        let status = ExitStatus::from_raw(1 << 8);
        let invalid = SupervisorError::Nginx(NginxError::InvalidConfig {
            status,
            output: String::new(),
        });
        assert_eq!(invalid.exit_code(), EXIT_CONFIG);
        assert_eq!(SupervisorError::StartupExit(status).exit_code(), EXIT_NGINX);
        assert_eq!(
            SupervisorError::StartupTimeout(Duration::from_secs(1)).exit_code(),
            EXIT_NGINX
        );
        assert_eq!(SupervisorError::UnexpectedExit(status).exit_code(), EXIT_NGINX);
        let io = SupervisorError::Process(io::Error::other("x"));
        assert_eq!(io.exit_code(), EXIT_FAILURE);
    }
}
