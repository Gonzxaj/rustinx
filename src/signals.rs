//! Translates termination signals into a cancellation token.

use std::io;

use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Installs SIGTERM, SIGINT and SIGHUP handlers and returns a token cancelled on SIGTERM or SIGINT.
///
/// SIGHUP is consumed and ignored: its default action would terminate the supervisor, and
/// validated reloads are not implemented yet.
pub fn shutdown_token() -> io::Result<CancellationToken> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                _ = sigterm.recv() => "SIGTERM",
                _ = sigint.recv() => "SIGINT",
                _ = sighup.recv() => {
                    warn!(signal = "SIGHUP", "signal ignored: configuration reload is not supported yet");
                    continue;
                }
            };
            if cancel.is_cancelled() {
                info!(signal = name, "shutdown already in progress");
            } else {
                info!(signal = name, "shutdown requested");
                cancel.cancel();
            }
        }
    });
    Ok(token)
}
