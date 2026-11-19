//! Structured logging to stderr, so container runtimes collect it.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

pub const ENV_LOG: &str = "RUSTINX_LOG";
pub const ENV_LOG_FORMAT: &str = "RUSTINX_LOG_FORMAT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") | Some("text") => Ok(Self::Text),
            Some("json") => Ok(Self::Json),
            Some(other) => Err(format!(
                "{ENV_LOG_FORMAT} must be \"text\" or \"json\", got {other:?}"
            )),
        }
    }
}

/// Initializes the global subscriber. `RUSTINX_LOG` accepts `EnvFilter` directives (default `info`).
pub fn init() -> Result<(), String> {
    let format = LogFormat::parse(std::env::var(ENV_LOG_FORMAT).ok().as_deref())?;
    let filter = match std::env::var(ENV_LOG) {
        Ok(directives) => {
            EnvFilter::try_new(&directives).map_err(|err| format!("invalid {ENV_LOG}: {err}"))?
        }
        Err(_) => EnvFilter::new("info"),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal());
    let result = match format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().flatten_event(true).try_init(),
    };
    result.map_err(|err| format!("failed to initialize logging: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_formats() {
        assert_eq!(LogFormat::parse(None), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("text")), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("json")), Ok(LogFormat::Json));
        assert!(LogFormat::parse(Some("xml")).is_err());
    }
}
