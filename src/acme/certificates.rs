//! Certificate requests declared by the user in a TOML file (default `/etc/rustinx/certificates.toml`).
//! The file is optional: without it rustinx only supervises Nginx.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

pub const ENV_CERTIFICATES_CONF: &str = "RUSTINX_CERTIFICATES_CONF";
pub const DEFAULT_CERTIFICATES_CONF: &str = "/etc/rustinx/certificates.toml";

const DEFAULT_CHALLENGE_TIMEOUT_SECS: u64 = 90;
const MAX_CHALLENGE_TIMEOUT_SECS: u64 = 900;
const MAX_ID_LEN: usize = 64;
/// Let's Encrypt limit of names per certificate.
const MAX_DOMAINS: usize = 100;

#[derive(Debug, Error)]
pub enum CertificatesError {
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("invalid {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    acme: RawAcme,
    cloudflare: Option<CloudflareSettings>,
    renewal: Option<RenewalSettings>,
    #[serde(default, rename = "certificate")]
    certificates: Vec<CertificateRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcme {
    directory: String,
    #[serde(default)]
    contact: Vec<String>,
    #[serde(default)]
    terms_of_service_agreed: bool,
    ca_root: Option<PathBuf>,
    challenge_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeSettings {
    /// ACME directory URL (HTTPS).
    pub directory: String,
    /// Account contact URLs, e.g. `mailto:admin@example.com`.
    pub contact: Vec<String>,
    /// Extra root CA for the ACME server's TLS certificate (test CAs such as Pebble only).
    pub ca_root: Option<PathBuf>,
    /// Maximum time to wait for authorizations and for the certificate.
    pub challenge_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateRequest {
    /// Directory name under `/data/certificates/`.
    pub id: String,
    /// DNS names (SAN). The first one is the primary name.
    pub domains: Vec<String>,
    #[serde(default)]
    pub challenge: Challenge,
    /// Opt in to generated HTTPS for the declared domains.
    pub https: Option<HttpsSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum Challenge {
    #[default]
    #[serde(rename = "http-01")]
    Http01,
    #[serde(rename = "dns-01")]
    Dns01,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareSettings {
    pub zone_id: String,
    #[serde(default = "default_dns_token_file")]
    pub token_file: PathBuf,
}

fn default_dns_token_file() -> PathBuf {
    "/run/secrets/cloudflare_dns_token".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpsSettings {
    /// Shared Nginx directives to include inside the generated TLS server.
    pub include: PathBuf,
    #[serde(default)]
    pub redirect_http: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificatesConfig {
    pub acme: AcmeSettings,
    pub cloudflare: Option<CloudflareSettings>,
    pub renewal: Option<RenewalSettings>,
    pub certificates: Vec<CertificateRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenewalSettings {
    pub renew_before_secs: u64,
    pub check_interval_secs: u64,
    pub retry_initial_secs: u64,
    pub retry_max_secs: u64,
    pub rate_limit_delay_secs: u64,
}

impl Default for RenewalSettings {
    fn default() -> Self {
        Self {
            renew_before_secs: 30 * 86_400,
            check_interval_secs: 12 * 3600,
            retry_initial_secs: 60,
            retry_max_secs: 3600,
            rate_limit_delay_secs: 48 * 3600,
        }
    }
}

pub(crate) fn config_path() -> PathBuf {
    std::env::var_os(ENV_CERTIFICATES_CONF)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CERTIFICATES_CONF))
}
impl CertificatesConfig {
    /// Returns `Ok(None)` when the file does not exist. Blocking: call from `spawn_blocking`.
    pub fn load(path: &Path) -> Result<Option<Self>, CertificatesError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CertificatesError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::parse(&text).map(Some).map_err(|err| err.with_path(path))
    }

    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let raw: RawFile = toml::from_str(text).map_err(|err| ParseError::Toml(Box::new(err)))?;
        let acme = validate_acme(raw.acme)?;
        if let Some(renewal) = raw.renewal
            && (!(1..=365 * 86_400).contains(&renewal.renew_before_secs)
                || !(1..=86_400).contains(&renewal.check_interval_secs)
                || !(1..=86_400).contains(&renewal.retry_initial_secs)
                || !(renewal.retry_initial_secs..=7 * 86_400).contains(&renewal.retry_max_secs)
                || !(1..=30 * 86_400).contains(&renewal.rate_limit_delay_secs))
        {
            return Err(ParseError::Invalid(
                "invalid renewal threshold, interval or retry limits".into(),
            ));
        }
        if let Some(cloudflare) = &raw.cloudflare
            && (cloudflare.zone_id.len() != 32
                || !cloudflare.zone_id.bytes().all(|b| b.is_ascii_hexdigit())
                || !cloudflare.token_file.is_absolute())
        {
            return Err(ParseError::Invalid(
                "Cloudflare requires a 32-digit hexadecimal zone_id and an absolute token_file".into(),
            ));
        }
        let mut ids = HashSet::new();
        for cert in &raw.certificates {
            validate_id(&cert.id)?;
            if !ids.insert(cert.id.as_str()) {
                return Err(ParseError::Invalid(format!(
                    "duplicate certificate id {:?}",
                    cert.id
                )));
            }
            validate_domains(cert)?;
            if cert.challenge == Challenge::Dns01 && raw.cloudflare.is_none() {
                return Err(ParseError::Invalid(
                    "dns-01 requires a [cloudflare] section".into(),
                ));
            }
            if let Some(https) = &cert.https
                && (!https.include.is_absolute()
                    || !https
                        .include
                        .as_os_str()
                        .as_encoded_bytes()
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-')))
            {
                return Err(ParseError::Invalid(format!(
                    "certificate {:?}: https.include must be an absolute Nginx snippet path",
                    cert.id
                )));
            }
        }
        Ok(Self {
            acme,
            cloudflare: raw.cloudflare,
            renewal: raw.renewal,
            certificates: raw.certificates,
        })
    }
}

#[derive(Debug)]
pub enum ParseError {
    Toml(Box<toml::de::Error>),
    Invalid(String),
}

impl ParseError {
    fn with_path(self, path: &Path) -> CertificatesError {
        match self {
            Self::Toml(source) => CertificatesError::Parse {
                path: path.to_owned(),
                source,
            },
            Self::Invalid(message) => CertificatesError::Invalid {
                path: path.to_owned(),
                message,
            },
        }
    }
}

fn validate_acme(raw: RawAcme) -> Result<AcmeSettings, ParseError> {
    if !raw.directory.starts_with("https://") {
        return Err(ParseError::Invalid(format!(
            "acme.directory must be an https:// URL, got {:?}",
            raw.directory
        )));
    }
    if !raw.terms_of_service_agreed {
        return Err(ParseError::Invalid(
            "acme.terms_of_service_agreed must be true to register an ACME account".to_owned(),
        ));
    }
    if let Some(contact) = raw.contact.iter().find(|c| !c.starts_with("mailto:")) {
        return Err(ParseError::Invalid(format!(
            "acme.contact entries must start with mailto:, got {contact:?}"
        )));
    }
    if let Some(root) = &raw.ca_root
        && !root.is_absolute()
    {
        return Err(ParseError::Invalid(format!(
            "acme.ca_root must be an absolute path, got {}",
            root.display()
        )));
    }
    let secs = raw
        .challenge_timeout_secs
        .unwrap_or(DEFAULT_CHALLENGE_TIMEOUT_SECS);
    if !(1..=MAX_CHALLENGE_TIMEOUT_SECS).contains(&secs) {
        return Err(ParseError::Invalid(format!(
            "acme.challenge_timeout_secs must be between 1 and {MAX_CHALLENGE_TIMEOUT_SECS}, got {secs}"
        )));
    }
    Ok(AcmeSettings {
        directory: raw.directory,
        contact: raw.contact,
        ca_root: raw.ca_root,
        challenge_timeout: Duration::from_secs(secs),
    })
}

fn validate_id(id: &str) -> Result<(), ParseError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with(['.', '-'])
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ParseError::Invalid(format!(
            "certificate id must be 1-{MAX_ID_LEN} chars of [a-z0-9._-], not starting with '.' or '-', got {id:?}"
        )))
    }
}

fn validate_domains(cert: &CertificateRequest) -> Result<(), ParseError> {
    if cert.domains.is_empty() || cert.domains.len() > MAX_DOMAINS {
        return Err(ParseError::Invalid(format!(
            "certificate {:?} must list between 1 and {MAX_DOMAINS} domains",
            cert.id
        )));
    }
    let mut seen = HashSet::new();
    for domain in &cert.domains {
        let base = if let Some(base) = domain.strip_prefix("*.") {
            if cert.challenge != Challenge::Dns01 || cert.https.is_some() {
                return Err(ParseError::Invalid(format!(
                    "certificate {:?}: wildcard {domain:?} requires DNS-01 and manual HTTPS configuration",
                    cert.id
                )));
            }
            base
        } else {
            domain.as_str()
        };
        if !is_dns_name(base) {
            return Err(ParseError::Invalid(format!(
                "certificate {:?}: {domain:?} is not a valid lowercase DNS name",
                cert.id
            )));
        }
        if !seen.insert(domain.as_str()) {
            return Err(ParseError::Invalid(format!(
                "certificate {:?}: duplicate domain {domain:?}",
                cert.id
            )));
        }
    }
    Ok(())
}

fn is_dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[acme]
directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
contact = ["mailto:admin@example.com"]
terms_of_service_agreed = true

[[certificate]]
id = "example"
domains = ["example.com", "www.example.com"]
"#;

    fn invalid(text: &str) -> String {
        match CertificatesConfig::parse(text) {
            Err(ParseError::Invalid(message)) => message,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn dns_01_accepts_wildcards_and_requires_cloudflare_settings() {
        let config = VALID
            .replace(
                "[[certificate]]",
                "[cloudflare]\nzone_id = \"0123456789abcdef0123456789abcdef\"\n\n[[certificate]]",
            )
            .replace(
                "domains = [\"example.com\", \"www.example.com\"]",
                "domains = [\"example.com\", \"*.example.com\"]\nchallenge = \"dns-01\"",
            );
        let parsed = CertificatesConfig::parse(&config).unwrap();
        assert_eq!(parsed.certificates[0].challenge, Challenge::Dns01);
        assert_eq!(parsed.certificates[0].domains[1], "*.example.com");
        assert!(
            invalid(&config.replace(
                "[cloudflare]\nzone_id = \"0123456789abcdef0123456789abcdef\"\n",
                ""
            ))
            .contains("requires a [cloudflare]")
        );
    }

    #[test]
    fn parses_valid_file() {
        let config = CertificatesConfig::parse(VALID).unwrap();
        assert_eq!(config.acme.challenge_timeout, Duration::from_secs(90));
        assert_eq!(config.acme.contact, ["mailto:admin@example.com"]);
        assert_eq!(config.certificates.len(), 1);
        assert_eq!(config.certificates[0].domains, ["example.com", "www.example.com"]);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            CertificatesConfig::load(&dir.path().join("none.toml"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = VALID.replace("terms_of_service_agreed", "tos");
        assert!(matches!(
            CertificatesConfig::parse(&text),
            Err(ParseError::Toml(_))
        ));
    }

    #[test]
    fn requires_https_and_terms() {
        assert!(invalid(&VALID.replace("https://", "http://")).contains("https://"));
        assert!(invalid(&VALID.replace("= true", "= false")).contains("terms_of_service_agreed"));
    }

    #[test]
    fn rejects_bad_contact_and_timeout() {
        assert!(invalid(&VALID.replace("mailto:", "")).contains("mailto:"));
        let text = VALID.replace("[acme]", "[acme]\nchallenge_timeout_secs = 0");
        assert!(invalid(&text).contains("challenge_timeout_secs"));
    }

    #[test]
    fn rejects_bad_ids() {
        for id in ["", "../x", ".hidden", "UPPER", "a/b"] {
            let text = VALID.replace("id = \"example\"", &format!("id = {id:?}"));
            assert!(invalid(&text).contains("certificate id"), "{id}");
        }
        let dup = format!("{VALID}\n[[certificate]]\nid = \"example\"\ndomains = [\"a.example.com\"]\n");
        assert!(invalid(&dup).contains("duplicate certificate id"));
    }

    #[test]
    fn rejects_bad_domains() {
        for (domain, needle) in [
            ("*.example.com", "wildcard"),
            ("Example.com", "not a valid"),
            ("localhost", "not a valid"),
            ("-a.example.com", "not a valid"),
            ("example.com\", \"example.com", "duplicate domain"),
        ] {
            let text = VALID.replace("\"example.com\", \"www.example.com\"", &format!("\"{domain}\""));
            assert!(invalid(&text).contains(needle), "{domain}");
        }
        assert!(invalid(&VALID.replace("\"example.com\", \"www.example.com\"", "")).contains("between 1"));
    }
}
