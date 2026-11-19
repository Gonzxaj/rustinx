//! Runtime configuration loaded from `RUSTINX_*` environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

pub const ENV_NGINX_BIN: &str = "RUSTINX_NGINX_BIN";
pub const ENV_NGINX_CONF: &str = "RUSTINX_NGINX_CONF";
pub const ENV_DATA_DIR: &str = "RUSTINX_DATA_DIR";
pub const ENV_HEALTH_ADDR: &str = "RUSTINX_HEALTH_ADDR";
pub const ENV_STARTUP_TIMEOUT: &str = "RUSTINX_STARTUP_TIMEOUT_SECS";
pub const ENV_SHUTDOWN_TIMEOUT: &str = "RUSTINX_SHUTDOWN_TIMEOUT_SECS";

const DEFAULT_NGINX_BIN: &str = "/usr/sbin/nginx";
const DEFAULT_NGINX_CONF: &str = "/etc/rustinx/nginx.conf";
const DEFAULT_DATA_DIR: &str = "/data";
const DEFAULT_HEALTH_ADDR: &str = "127.0.0.1:8081";
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 8;
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{var} must be an absolute path, got {value:?}")]
    NotAbsolute { var: &'static str, value: String },
    #[error("{var} must be a socket address such as 127.0.0.1:8081, got {value:?}")]
    InvalidAddr { var: &'static str, value: String },
    #[error("{var} must be a loopback address so the health endpoint is never exposed, got {value}")]
    NotLoopback { var: &'static str, value: SocketAddr },
    #[error("{var} must be an integer between 1 and {MAX_TIMEOUT_SECS}, got {value:?}")]
    InvalidTimeout { var: &'static str, value: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub nginx_bin: PathBuf,
    pub nginx_conf: PathBuf,
    pub data_dir: PathBuf,
    /// Internal Nginx health endpoint. Must match the health `server` block of the Nginx configuration.
    pub health_addr: SocketAddr,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &str, default: &str| lookup(key).unwrap_or_else(|| default.to_owned());
        Ok(Self {
            nginx_bin: absolute_path(ENV_NGINX_BIN, get(ENV_NGINX_BIN, DEFAULT_NGINX_BIN))?,
            nginx_conf: absolute_path(ENV_NGINX_CONF, get(ENV_NGINX_CONF, DEFAULT_NGINX_CONF))?,
            data_dir: absolute_path(ENV_DATA_DIR, get(ENV_DATA_DIR, DEFAULT_DATA_DIR))?,
            health_addr: health_addr(&get(ENV_HEALTH_ADDR, DEFAULT_HEALTH_ADDR))?,
            startup_timeout: timeout(
                ENV_STARTUP_TIMEOUT,
                lookup(ENV_STARTUP_TIMEOUT),
                DEFAULT_STARTUP_TIMEOUT_SECS,
            )?,
            shutdown_timeout: timeout(
                ENV_SHUTDOWN_TIMEOUT,
                lookup(ENV_SHUTDOWN_TIMEOUT),
                DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            )?,
        })
    }
}

/// Reads only the health address, so `rustinx healthcheck` does not depend on unrelated settings.
pub fn health_addr_from_env() -> Result<SocketAddr, ConfigError> {
    health_addr(&std::env::var(ENV_HEALTH_ADDR).unwrap_or_else(|_| DEFAULT_HEALTH_ADDR.to_owned()))
}

fn absolute_path(var: &'static str, value: String) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(&value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ConfigError::NotAbsolute { var, value })
    }
}

fn health_addr(value: &str) -> Result<SocketAddr, ConfigError> {
    let addr: SocketAddr = value.parse().map_err(|_| ConfigError::InvalidAddr {
        var: ENV_HEALTH_ADDR,
        value: value.to_owned(),
    })?;
    if !addr.ip().is_loopback() {
        return Err(ConfigError::NotLoopback {
            var: ENV_HEALTH_ADDR,
            value: addr,
        });
    }
    Ok(addr)
}

fn timeout(var: &'static str, value: Option<String>, default_secs: u64) -> Result<Duration, ConfigError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(default_secs));
    };
    match value.trim().parse::<u64>() {
        Ok(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => Ok(Duration::from_secs(secs)),
        _ => Err(ConfigError::InvalidTimeout { var, value }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_are_applied() {
        let config = load(&[]).unwrap();
        assert_eq!(config.nginx_bin, PathBuf::from("/usr/sbin/nginx"));
        assert_eq!(config.nginx_conf, PathBuf::from("/etc/rustinx/nginx.conf"));
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.health_addr, "127.0.0.1:8081".parse().unwrap());
        assert_eq!(config.startup_timeout, Duration::from_secs(10));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(8));
    }

    #[test]
    fn overrides_are_applied() {
        let config = load(&[
            (ENV_NGINX_BIN, "/opt/nginx"),
            (ENV_DATA_DIR, "/srv/data"),
            (ENV_HEALTH_ADDR, "[::1]:9000"),
            (ENV_STARTUP_TIMEOUT, "30"),
            (ENV_SHUTDOWN_TIMEOUT, " 5 "),
        ])
        .unwrap();
        assert_eq!(config.nginx_bin, PathBuf::from("/opt/nginx"));
        assert_eq!(config.data_dir, PathBuf::from("/srv/data"));
        assert_eq!(config.health_addr, "[::1]:9000".parse().unwrap());
        assert_eq!(config.startup_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(5));
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = load(&[(ENV_DATA_DIR, "data")]).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NotAbsolute {
                var: ENV_DATA_DIR,
                ..
            }
        ));
    }

    #[test]
    fn non_loopback_health_addr_is_rejected() {
        let err = load(&[(ENV_HEALTH_ADDR, "0.0.0.0:8081")]).unwrap_err();
        assert!(matches!(err, ConfigError::NotLoopback { .. }));
    }

    #[test]
    fn invalid_health_addr_is_rejected() {
        let err = load(&[(ENV_HEALTH_ADDR, "localhost")]).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAddr { .. }));
    }

    #[test]
    fn invalid_timeouts_are_rejected() {
        for value in ["0", "-1", "abc", "301", ""] {
            let err = load(&[(ENV_SHUTDOWN_TIMEOUT, value)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidTimeout { .. }),
                "value {value:?}"
            );
        }
    }
}
