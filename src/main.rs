use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use rustinx::certificates::{CertificatesConfig, DEFAULT_CERTIFICATES_CONF, ENV_CERTIFICATES_CONF};
use rustinx::nginx::Nginx;
use rustinx::supervisor::{self, EXIT_CONFIG, EXIT_FAILURE, EXIT_OK};
use rustinx::{config, health, https, logging, signals};
use tracing::{error, info};

const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

const USAGE: &str = "\
Usage: rustinx [COMMAND]

Commands:
  run          Supervise Nginx (default)
  healthcheck  Exit 0 if the internal Nginx health endpoint answers 200
  issue        Issue configured certificates via HTTP-01
  https        Apply managed HTTPS configuration using stored certificates
  nginx-config Print the effective Nginx configuration
  version      Print the version

Configuration is read from RUSTINX_* environment variables (see README.md).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["run"] => run(),
        ["issue"] => manage_certificates(true),
        ["https"] => manage_certificates(false),
        ["nginx-config"] => nginx_config(),
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

fn manage_certificates(issue: bool) -> ExitCode {
    if let Err(err) = logging::init() {
        eprintln!("rustinx: {err}");
        return ExitCode::from(EXIT_CONFIG);
    }
    let path = std::env::var_os(ENV_CERTIFICATES_CONF)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CERTIFICATES_CONF));
    if !path.is_absolute() {
        error!("{ENV_CERTIFICATES_CONF} must be an absolute path");
        return ExitCode::from(EXIT_CONFIG);
    }
    let requests = match CertificatesConfig::load(&path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            error!(path = %path.display(), "certificate configuration is missing");
            return ExitCode::from(EXIT_CONFIG);
        }
        Err(err) => {
            error!(error = %err, "invalid certificate configuration");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let config = match config::Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            error!(error = %err, "invalid configuration");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    run_command(async {
        if issue {
            let shutdown = signals::shutdown_token()?;
            rustinx::acme::issue(&config.data_dir, &requests, shutdown.clone()).await?;
            if shutdown.is_cancelled() {
                return Err(rustinx::acme::IssueError::Cancelled.into());
            }
        }
        if !issue || https::requested(&requests) {
            https::apply(&config, &requests).await?;
        }
        Ok(())
    })
}

fn nginx_config() -> ExitCode {
    let config = match config::Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("rustinx: {err}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    run_command(async {
        let nginx = Nginx::new(config.nginx_bin, config.nginx_conf);
        let output = nginx.dump_config(config.startup_timeout).await?;
        print!("{output}");
        Ok(())
    })
}

fn run_command(command: impl Future<Output = Result<(), Box<dyn std::error::Error>>>) -> ExitCode {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| -> Box<dyn std::error::Error> { Box::new(err) })
        .and_then(|runtime| runtime.block_on(command));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rustinx: {err}");
            ExitCode::from(EXIT_FAILURE)
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
