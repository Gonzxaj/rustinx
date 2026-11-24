//! Nginx lifecycle: validate, start, wait for readiness, monitor and stop.

use std::io;
use std::process::ExitStatus;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::sys::signal::Signal;
use thiserror::Error;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::{Nginx, NginxError, NginxProcess};
use super::{health, https};
use crate::acme::renewal;
use crate::config::Config;
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
    #[error(transparent)]
    Https(#[from] https::HttpsError),
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
    let startup_config = config.clone();
    let (state, managed_https) = tokio::task::spawn_blocking(move || {
        let state = Storage::open(&startup_config.data_dir)?.record_start(unix_now())?;
        let managed_https = https::prepare_startup(&startup_config)?;
        Ok::<_, SupervisorError>((state, managed_https))
    })
    .await??;
    info!(target: "rustinx::supervisor", data_dir = %config.data_dir.display(),
        start_count = state.start_count,
        "persistent storage ready"
    );

    let nginx = Nginx::new(config.nginx_bin.clone(), config.nginx_conf.clone());
    let output = tokio::select! {
        result = validate_config(&nginx, config.startup_timeout, managed_https) => result?,
        _ = shutdown.cancelled() => return Ok(()),
    };
    info!(target: "rustinx::supervisor", conf = %config.nginx_conf.display(), "nginx configuration is valid");
    if !output.is_empty() {
        debug!(target: "rustinx::supervisor", output = %output, "nginx -t output");
    }

    let mut process = nginx.spawn()?;
    info!(target: "rustinx::supervisor", pid = process.pid(), "nginx started");

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
        error!(target: "rustinx::supervisor", timeout = ?config.startup_timeout, "nginx did not become healthy; stopping it");
        stop(&mut process, config.shutdown_timeout).await?;
        return Err(SupervisorError::StartupTimeout(config.startup_timeout));
    }
    info!(target: "rustinx::supervisor", elapsed_ms = started.elapsed().as_millis() as u64,
        "nginx is healthy"
    );

    let worker_shutdown = shutdown.child_token();
    let mut worker = tokio::spawn({
        let config = config.clone();
        let shutdown = worker_shutdown.clone();
        async move { renewal::run(&config, shutdown).await }
    });
    let result = tokio::select! {
        status = process.wait() => {
            kill_leftovers(&process);
            Err(status.map_or_else(SupervisorError::Process, SupervisorError::UnexpectedExit))
        }
        _ = shutdown.cancelled() => stop(&mut process, config.shutdown_timeout).await,

    };
    worker_shutdown.cancel();
    if tokio::time::timeout(Duration::from_secs(1), &mut worker)
        .await
        .is_err()
    {
        worker.abort();
        let _ = worker.await;
    }
    result
}

async fn validate_config(
    nginx: &Nginx,
    timeout: Duration,
    managed_https: bool,
) -> Result<String, SupervisorError> {
    match nginx.test_config(timeout).await {
        Err(NginxError::InvalidConfig { .. }) if managed_https => {
            warn!(target: "rustinx::supervisor", "saved HTTPS configuration failed validation; attempting HTTP startup");
            https::prepare_http_only()?;
            Ok(nginx.test_config(timeout).await?)
        }
        result => Ok(result?),
    }
}

async fn wait_until_healthy(config: &Config) -> bool {
    let deadline = Instant::now() + config.startup_timeout;
    loop {
        match health::probe(config.health_addr, PROBE_TIMEOUT).await {
            Ok(()) => return true,
            Err(err) => debug!(target: "rustinx::supervisor", error = %err, "nginx not ready yet"),
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
        info!(target: "rustinx::supervisor", pid, timeout = ?timeout, "graceful nginx shutdown requested (SIGQUIT)");
    }
    match tokio::time::timeout(timeout, process.wait()).await {
        Ok(status) => info!(target: "rustinx::supervisor", pid, status = %status?, "nginx stopped"),
        Err(_) => {
            warn!(target: "rustinx::supervisor", pid, timeout = ?timeout, "nginx did not stop in time; sending SIGKILL");
            process.kill().await?;
            info!(target: "rustinx::supervisor", pid, "nginx killed");
        }
    }
    kill_leftovers(process);
    Ok(())
}

/// Ensures no Nginx process outlives its master.
fn kill_leftovers(process: &NginxProcess) {
    match process.kill_group() {
        Ok(true) => warn!(target: "rustinx::supervisor", "killed processes left in the nginx process group"),
        Ok(false) => {}
        Err(err) => {
            warn!(target: "rustinx::supervisor", error = %err, "failed to kill the nginx process group")
        }
    }
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
