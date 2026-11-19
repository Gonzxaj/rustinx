//! Nginx command invocation. Every command uses explicit arguments; no shell is involved.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use thiserror::Error;
use tokio::process::{Child, Command};

#[derive(Debug, Error)]
pub enum NginxError {
    #[error("failed to execute {bin}: {source}")]
    Exec { bin: PathBuf, source: io::Error },
    #[error("nginx configuration test failed ({status}):\n{output}")]
    InvalidConfig { status: ExitStatus, output: String },
    #[error("nginx configuration test did not finish within {0:?}")]
    TestTimeout(Duration),
}

#[derive(Debug, Clone)]
pub struct Nginx {
    bin: PathBuf,
    conf: PathBuf,
}

impl Nginx {
    pub fn new(bin: PathBuf, conf: PathBuf) -> Self {
        Self { bin, conf }
    }

    /// `-e stderr` sends errors to stderr even before the configuration is parsed.
    fn common_args(&self) -> Vec<OsString> {
        vec![
            "-c".into(),
            self.conf.clone().into(),
            "-e".into(),
            "stderr".into(),
        ]
    }

    pub fn test_args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from("-t")];
        args.extend(self.common_args());
        args
    }

    /// Nginx must stay in the foreground so the supervisor owns the master process.
    pub fn run_args(&self) -> Vec<OsString> {
        let mut args = self.common_args();
        args.extend(["-g".into(), "daemon off;".into()]);
        args
    }

    /// Runs `nginx -t` and returns its combined output on success.
    pub async fn test_config(&self, timeout: Duration) -> Result<String, NginxError> {
        let mut command = Command::new(&self.bin);
        command
            .args(self.test_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| NginxError::TestTimeout(timeout))?
            .map_err(|source| NginxError::Exec {
                bin: self.bin.clone(),
                source,
            })?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let text = text.trim_end().to_owned();
        if output.status.success() {
            Ok(text)
        } else {
            Err(NginxError::InvalidConfig {
                status: output.status,
                output: text,
            })
        }
    }

    /// Starts the Nginx master in its own process group, so terminal signals reach only the
    /// supervisor, which decides how Nginx is stopped. Output is inherited for container logs.
    pub fn spawn(&self) -> Result<NginxProcess, NginxError> {
        let child = Command::new(&self.bin)
            .args(self.run_args())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| NginxError::Exec {
                bin: self.bin.clone(),
                source,
            })?;
        Ok(NginxProcess { child })
    }
}

#[derive(Debug)]
pub struct NginxProcess {
    child: Child,
}

impl NginxProcess {
    /// PID of the master process, or `None` once it has been reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Sends `signal` to the master process. Returns `Ok(false)` if it already exited.
    pub fn signal(&self, signal: Signal) -> io::Result<bool> {
        let Some(pid) = self.pid() else {
            return Ok(false);
        };
        let pid = i32::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
        match kill(Pid::from_raw(pid), signal) {
            Ok(()) => Ok(true),
            Err(nix::errno::Errno::ESRCH) => Ok(false),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    /// Sends SIGKILL and reaps the process.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nginx() -> Nginx {
        Nginx::new("/usr/sbin/nginx".into(), "/etc/rustinx/nginx.conf".into())
    }

    #[test]
    fn test_arguments_are_explicit() {
        let args = nginx().test_args();
        assert_eq!(
            args,
            ["-t", "-c", "/etc/rustinx/nginx.conf", "-e", "stderr"].map(OsString::from)
        );
    }

    #[test]
    fn run_arguments_keep_nginx_in_foreground() {
        let args = nginx().run_args();
        assert_eq!(
            args,
            [
                "-c",
                "/etc/rustinx/nginx.conf",
                "-e",
                "stderr",
                "-g",
                "daemon off;"
            ]
            .map(OsString::from)
        );
    }

    #[tokio::test]
    async fn missing_binary_is_reported() {
        let nginx = Nginx::new("/nonexistent/nginx".into(), "/tmp/nginx.conf".into());
        let err = nginx.test_config(Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(err, NginxError::Exec { .. }));
        assert!(matches!(nginx.spawn().unwrap_err(), NginxError::Exec { .. }));
    }
}
