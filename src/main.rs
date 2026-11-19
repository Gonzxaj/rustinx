use std::process::ExitCode;
use std::time::Duration;

use rustinx::supervisor::{self, EXIT_CONFIG, EXIT_FAILURE, EXIT_OK};
use rustinx::{config, health, logging, signals};
use tracing::{error, info};

const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

const USAGE: &str = "\
Usage: rustinx [COMMAND]

Commands:
  run          Supervise Nginx (default)
  healthcheck  Exit 0 if the internal Nginx health endpoint answers 200
  version      Print the version

Configuration is read from RUSTINX_* environment variables (see README.md).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["run"] => run(),
        ["healthcheck"] => healthcheck(),
        ["version" | "--version" | "-V"] => {
            println!("rustinx {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("rustinx: invalid arguments {args:?}\n\n{USAGE}");
            ExitCode::from(EXIT_CONFIG)
        }
    }
}

fn run() -> ExitCode {
    if let Err(err) = logging::init() {
        eprintln!("rustinx: {err}");
        return ExitCode::from(EXIT_CONFIG);
    }
    let config = match config::Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            error!(error = %err, "invalid configuration");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(error = %err, "failed to start async runtime");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    info!(version = env!("CARGO_PKG_VERSION"), "rustinx starting");
    let result = runtime.block_on(async {
        let shutdown = signals::shutdown_token()?;
        supervisor::run(&config, shutdown).await
    });
    match result {
        Ok(()) => {
            info!("rustinx stopped");
            ExitCode::from(EXIT_OK)
        }
        Err(err) => {
            let code = err.exit_code();
            error!(error = %err, exit_code = code, "rustinx stopped with an error");
            ExitCode::from(code)
        }
    }
}

fn healthcheck() -> ExitCode {
    let addr = match config::health_addr_from_env() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("unhealthy: {err}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("unhealthy: failed to start runtime: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    match runtime.block_on(health::probe(addr, HEALTHCHECK_TIMEOUT)) {
        Ok(()) => {
            println!("healthy");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("unhealthy: {err}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}
