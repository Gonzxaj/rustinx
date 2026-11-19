//! Integration tests for the `rustinx` binary against a fake nginx (`tests/fixtures/fake-nginx.sh`).
//!
//! The real Nginx is exercised by the Docker integration suite (`scripts/docker-integration.sh`).
//! Here the test process serves the health endpoint itself, so each scenario controls readiness.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    dir: TempDir,
    health_addr: SocketAddr,
}

impl Harness {
    /// A harness whose health endpoint answers 200 to every request.
    fn healthy() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let health_addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n");
            }
        });
        Self::with_health_addr(health_addr)
    }

    /// A harness whose health endpoint never answers (nothing listens on the port).
    fn unhealthy() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let health_addr = listener.local_addr().unwrap();
        drop(listener);
        Self::with_health_addr(health_addr)
    }

    fn with_health_addr(health_addr: SocketAddr) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("data")).unwrap();
        fs::create_dir(dir.path().join("fake")).unwrap();
        Self { dir, health_addr }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn fake(&self, name: &str) -> PathBuf {
        self.path("fake").join(name)
    }

    fn command(&self, mode: &str) -> Command {
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-nginx.sh");
        let mut command = Command::new(env!("CARGO_BIN_EXE_rustinx"));
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("RUSTINX_NGINX_BIN", fake)
            .env("RUSTINX_NGINX_CONF", self.path("nginx.conf"))
            .env("RUSTINX_DATA_DIR", self.path("data"))
            .env("RUSTINX_HEALTH_ADDR", self.health_addr.to_string())
            .env("RUSTINX_STARTUP_TIMEOUT_SECS", "2")
            .env("RUSTINX_SHUTDOWN_TIMEOUT_SECS", "2")
            .env("RUSTINX_LOG", "debug")
            .env("FAKE_NGINX_MODE", mode)
            .env("FAKE_NGINX_DIR", self.path("fake"))
            .stdin(Stdio::null());
        command
    }

    fn spawn(&self, mode: &str) -> Child {
        let log = File::create(self.path("rustinx.log")).unwrap();
        self.command(mode)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.path("rustinx.log")).unwrap_or_default()
    }

    fn read_fake(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.fake(name))
            .ok()
            .map(|s| s.trim().to_owned())
    }

    fn nginx_pid(&self) -> Pid {
        Pid::from_raw(
            self.read_fake("pid")
                .expect("fake nginx was not started")
                .parse()
                .unwrap(),
        )
    }

    fn wait_for_log(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; log:\n{}",
                self.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_exit(&self, child: &mut Child) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("rustinx did not exit in time; log:\n{}", self.log());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn signal(child: &Child, signal: Signal) {
    kill(Pid::from_raw(child.id() as i32), signal).unwrap();
}

fn is_alive(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

fn assert_graceful_stop(signal_to_send: Signal) {
    let harness = Harness::healthy();
    let mut child = harness.spawn("normal");
    harness.wait_for_log("nginx is healthy");
    let nginx = harness.nginx_pid();
    assert!(is_alive(nginx));

    signal(&child, signal_to_send);
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(0), "log:\n{}", harness.log());
    assert_eq!(
        harness.read_fake("signal").as_deref(),
        Some("quit"),
        "nginx must receive SIGQUIT"
    );
    assert!(!is_alive(nginx), "nginx process must not survive the supervisor");
    let log = harness.log();
    assert!(log.contains("shutdown requested"), "log:\n{log}");
    assert!(log.contains("nginx stopped"), "log:\n{log}");
}

#[test]
fn sigterm_stops_nginx_gracefully() {
    assert_graceful_stop(Signal::SIGTERM);
}

#[test]
fn sigint_stops_nginx_gracefully() {
    assert_graceful_stop(Signal::SIGINT);
}

#[test]
fn nginx_is_tested_then_started_with_explicit_arguments() {
    let harness = Harness::healthy();
    let mut child = harness.spawn("normal");
    harness.wait_for_log("nginx is healthy");
    signal(&child, Signal::SIGTERM);
    harness.wait_exit(&mut child);

    let conf = harness.path("nginx.conf");
    let expected = format!(
        "test -t -c {conf} -e stderr\nrun -c {conf} -e stderr -g daemon off;",
        conf = conf.display()
    );
    assert_eq!(harness.read_fake("invocations").unwrap(), expected);
}

#[test]
fn invalid_nginx_config_exits_with_code_2_without_starting_nginx() {
    let harness = Harness::healthy();
    let mut child = harness.spawn("bad-config");
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(2), "log:\n{}", harness.log());
    assert!(harness.read_fake("pid").is_none(), "nginx must not be started");
    assert!(
        harness.log().contains("fake configuration error"),
        "nginx -t output must be logged"
    );
}

#[test]
fn nginx_exit_during_startup_is_detected() {
    // Unhealthy endpoint: a real nginx that fails to start never answers the health probe.
    let harness = Harness::unhealthy();
    let mut child = harness.spawn("exit-on-start");
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(3), "log:\n{}", harness.log());
    assert!(harness.log().contains("nginx exited during startup"));
}

#[test]
fn unexpected_nginx_exit_is_detected() {
    let harness = Harness::healthy();
    let mut child = harness.spawn("crash-after-start");
    harness.wait_for_log("nginx is healthy");
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(3), "log:\n{}", harness.log());
    assert!(harness.log().contains("nginx exited unexpectedly"));

    let worker = Pid::from_raw(harness.read_fake("worker").unwrap().parse().unwrap());
    assert!(
        harness
            .log()
            .contains("killed processes left in the nginx process group")
    );
    // The orphan is reaped asynchronously by the host's init process.
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(worker) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!is_alive(worker), "orphaned worker {worker} survived");
}

#[test]
fn nginx_that_never_becomes_healthy_is_stopped() {
    let harness = Harness::unhealthy();
    let mut child = harness.spawn("normal");
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(3), "log:\n{}", harness.log());
    assert!(harness.log().contains("did not become healthy"));
    assert_eq!(harness.read_fake("signal").as_deref(), Some("quit"));
    assert!(!is_alive(harness.nginx_pid()));
}

#[test]
fn shutdown_timeout_escalates_to_sigkill() {
    let harness = Harness::healthy();
    let mut child = harness.spawn("ignore-quit");
    harness.wait_for_log("nginx is healthy");
    let nginx = harness.nginx_pid();

    let started = Instant::now();
    signal(&child, Signal::SIGTERM);
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(0), "log:\n{}", harness.log());
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "SIGKILL must wait for the shutdown timeout"
    );
    assert_eq!(harness.read_fake("signal").as_deref(), Some("quit-ignored"));
    assert!(harness.log().contains("sending SIGKILL"));
    assert!(!is_alive(nginx));
}

#[test]
fn runtime_state_persists_across_restarts() {
    let harness = Harness::healthy();
    for expected_count in 1..=2u64 {
        let mut child = harness.spawn("normal");
        harness.wait_for_log("nginx is healthy");
        signal(&child, Signal::SIGTERM);
        assert_eq!(harness.wait_exit(&mut child).code(), Some(0));

        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(harness.path("data/state/runtime.json")).unwrap())
                .unwrap();
        assert_eq!(state["start_count"], expected_count);
        assert_eq!(state["schema_version"], 1);
    }
    for name in ["accounts", "certificates", "challenges", "state"] {
        assert!(harness.path("data").join(name).is_dir(), "{name} missing");
    }
}

#[test]
fn missing_data_dir_exits_with_code_1_without_starting_nginx() {
    let harness = Harness::healthy();
    fs::remove_dir(harness.path("data")).unwrap();
    let mut child = harness.spawn("normal");
    let status = harness.wait_exit(&mut child);

    assert_eq!(status.code(), Some(1), "log:\n{}", harness.log());
    assert!(harness.log().contains("is the volume mounted?"));
    assert!(harness.read_fake("pid").is_none());
}

#[test]
fn invalid_environment_exits_with_code_2() {
    let harness = Harness::healthy();
    let status = harness
        .command("normal")
        .env("RUSTINX_SHUTDOWN_TIMEOUT_SECS", "forever")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(harness.read_fake("invocations").is_none());
}

#[test]
fn healthcheck_command_reports_state() {
    let healthy = Harness::healthy();
    let output = healthy.command("normal").arg("healthcheck").output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "healthy");

    let unhealthy = Harness::unhealthy();
    let output = unhealthy.command("normal").arg("healthcheck").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("unhealthy:"));
}
