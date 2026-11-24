//! Generate, validate and activate the opt-in Nginx TLS configuration.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::time::{Instant, sleep};

use super::health::{self, ProbeError};
use super::sites::{self, DiscoveryError};
use super::{Nginx, NginxError};
use crate::acme::cert_store::{CertStore, CertStoreError, METADATA_SCHEMA_VERSION};
use crate::acme::certificates::CertificatesConfig;
use crate::acme::{IssueError, lock_issuance};
use crate::config::Config;
use crate::storage::{StorageError, write_atomic};

const ACTIVE: &str = "/run/rustinx/https.conf";
const CANDIDATE: &str = "/run/rustinx/https.candidate";
const CANDIDATE_MAIN: &str = "/run/rustinx/https-nginx.candidate";
const INCLUDE: &str = "include /run/rustinx/https.conf;";
const GENERATION_PREFIX: &str = "# rustinx-generation ";
const INACTIVE: &str = "inactive";

#[derive(Debug, Error)]
pub enum HttpsError {
    #[error("managed HTTPS requires the Rustinx include in the main Nginx config")]
    UnsupportedConfig,
    #[error("managed HTTPS requires exactly one plain HTTP server for {0}")]
    MissingHttpSite(String),
    #[error("multiple HTTP servers declare {0}")]
    DuplicateHttpSite(String),
    #[error("an existing server listens on port 443 for {0}")]
    ExistingHttpsSite(String),
    #[error("multiple certificates are configured to serve HTTPS for {0}")]
    DuplicateDomain(String),
    #[error("HTTP server for {0} must include /etc/rustinx/snippets/managed-redirect.conf")]
    MissingRedirectSnippet(String),
    #[error("certificate {0} has no current issuance for its configured domains and ACME directory")]
    MissingCertificate(String),
    #[error(
        "managed HTTPS requires a data path made of ASCII letters, digits, slash, dot, underscore or hyphen"
    )]
    InvalidPath,
    #[error("candidate Nginx configuration reports conflicting server names")]
    Conflict,
    #[error("Nginx is not serving the configuration recorded on disk")]
    GenerationMismatch,
    #[error("Nginx did not confirm HTTPS generation {0} after reload")]
    ReloadTimeout(String),
    #[error(
        "activation failed ({activation}); restoring the previous configuration also failed ({rollback})"
    )]
    Rollback {
        activation: Box<HttpsError>,
        rollback: Box<HttpsError>,
    },
    #[error("managed HTTPS filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Certificate(#[from] CertStoreError),
    #[error(transparent)]
    Lock(#[from] IssueError),
    #[error(transparent)]
    Nginx(#[from] NginxError),
    #[error(transparent)]
    Health(#[from] ProbeError),
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
}

pub fn requested(requests: &CertificatesConfig) -> bool {
    requests
        .certificates
        .iter()
        .any(|request| request.https.is_some())
}

/// Recreate the runtime include from the last configuration confirmed by Nginx.
pub(crate) fn prepare_startup(config: &Config) -> Result<bool, HttpsError> {
    let main = match fs::read_to_string(&config.nginx_conf) {
        Ok(main) => main,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if !main.lines().any(|line| line.trim() == INCLUDE) {
        return Ok(false);
    }
    let saved = read_saved(&config.data_dir)?;
    let running_https = generation(&saved) != INACTIVE;
    write_atomic(Path::new(ACTIVE), saved.as_bytes())?;
    Ok(running_https)
}

pub(crate) fn prepare_http_only() -> Result<(), HttpsError> {
    write_atomic(Path::new(ACTIVE), render_maps(INACTIVE, &[]).as_bytes())?;
    Ok(())
}

pub async fn apply(config: &Config, requests: &CertificatesConfig) -> Result<(), HttpsError> {
    apply_mode(config, requests, true).await
}

pub(crate) async fn apply_if_changed(
    config: &Config,
    requests: &CertificatesConfig,
) -> Result<(), HttpsError> {
    apply_mode(config, requests, false).await
}

async fn apply_mode(config: &Config, requests: &CertificatesConfig, force: bool) -> Result<(), HttpsError> {
    let _lock = lock_issuance(&config.data_dir)?;
    let root = fs::read_to_string(&config.nginx_conf)?;
    let staged_root = candidate_root(&root)?;
    let nginx = Nginx::new(config.nginx_bin.clone(), config.nginx_conf.clone());
    let active_path = Path::new(ACTIVE);
    let mut previous = fs::read_to_string(active_path)?;
    let served = health::generation(config.health_addr, config.startup_timeout).await?;
    if served != generation(&previous) {
        // Recover an interrupted activation only when the committed file matches the workers.
        previous = read_saved(&config.data_dir)?;
        if served != generation(&previous) {
            return Err(HttpsError::GenerationMismatch);
        }
        write_atomic(active_path, previous.as_bytes())?;
    }

    let effective = nginx.dump_config(config.startup_timeout).await?;
    let sites = sites::discover(&effective)?;
    let id = format!("{}-{}", unix_time().as_nanos(), std::process::id());
    let template = render(config, requests, &sites, "GENERATION")?;
    let fingerprint = fingerprint(config, requests, &effective, &template)?;
    let fingerprint = format!("# rustinx-fingerprint {fingerprint:016x}");
    if !force && previous.lines().nth(1) == Some(fingerprint.as_str()) {
        return Ok(());
    }
    let candidate = template.replacen("GENERATION", &id, 2);
    let (heading, body) = candidate.split_once('\n').expect("rendered generation line");
    let candidate = format!("{heading}\n{fingerprint}\n{body}");

    let _staged_files = StagedFiles;
    write_atomic(Path::new(CANDIDATE), candidate.as_bytes())?;
    write_atomic(Path::new(CANDIDATE_MAIN), staged_root.as_bytes())?;
    let staged = Nginx::new(config.nginx_bin.clone(), CANDIDATE_MAIN.into());
    let output = staged.test_config(config.startup_timeout).await?;
    if output.contains("conflicting server name") {
        return Err(HttpsError::Conflict);
    }
    // Relative includes could resolve differently when the main config is under /run.
    sites::discover(&staged.dump_config(config.startup_timeout).await?)?;

    let activation = async {
        write_atomic(active_path, candidate.as_bytes())?;
        reload_and_confirm(&nginx, config, &id).await?;
        write_atomic(&saved_path(&config.data_dir), candidate.as_bytes())?;
        Ok::<(), HttpsError>(())
    }
    .await;
    if let Err(activation) = activation {
        let rollback = async {
            write_atomic(active_path, previous.as_bytes())?;
            reload_and_confirm(&nginx, config, generation(&previous)).await?;
            Ok::<(), HttpsError>(())
        }
        .await;
        return match rollback {
            Ok(()) => Err(activation),
            Err(rollback) => Err(HttpsError::Rollback {
                activation: Box::new(activation),
                rollback: Box::new(rollback),
            }),
        };
    }
    tracing::info!(target: "rustinx::https", generation = id, "managed HTTPS configuration loaded");
    Ok(())
}

fn fingerprint(
    config: &Config,
    requests: &CertificatesConfig,
    effective: &str,
    template: &str,
) -> Result<u64, HttpsError> {
    let mut hash = DefaultHasher::new();
    template.hash(&mut hash);
    let mut files = BTreeMap::<&str, Vec<&str>>::new();
    let mut path = "";
    for line in effective.lines().filter(|line| !line.is_empty()) {
        if let Some(file) = line
            .strip_prefix("# configuration file ")
            .and_then(|line| line.strip_suffix(':'))
        {
            path = file;
        } else if path != ACTIVE {
            files.entry(path).or_default().push(line);
        }
    }
    files.hash(&mut hash);
    let store = CertStore::new(&config.data_dir);
    for request in requests
        .certificates
        .iter()
        .filter(|request| request.https.is_some())
    {
        fs::read(store.certificate_dir(&request.id).join("fullchain.pem"))?.hash(&mut hash);
    }
    Ok(hash.finish())
}

fn candidate_root(root: &str) -> Result<String, HttpsError> {
    let mut found = false;
    let mut staged = String::new();
    for line in root.lines() {
        if line.trim() == INCLUDE {
            if found {
                return Err(HttpsError::UnsupportedConfig);
            }
            found = true;
            writeln!(staged, "    include {CANDIDATE};").expect("writing to String");
        } else {
            writeln!(staged, "{line}").expect("writing to String");
        }
    }
    if !found {
        return Err(HttpsError::UnsupportedConfig);
    }
    Ok(staged)
}

fn render(
    config: &Config,
    requests: &CertificatesConfig,
    sites: &[sites::Site],
    id: &str,
) -> Result<String, HttpsError> {
    let names = sites::by_name(sites);
    let mut assigned = HashSet::new();
    let mut redirects = Vec::new();
    let mut servers = String::new();
    let store = CertStore::new(&config.data_dir);

    for request in &requests.certificates {
        let Some(https) = &request.https else { continue };
        for domain in &request.domains {
            if !assigned.insert(domain.as_str()) {
                return Err(HttpsError::DuplicateDomain(domain.clone()));
            }
            let matches = names.get(domain.as_str()).map_or(&[][..], Vec::as_slice);
            if matches.iter().any(|site| site.https) {
                return Err(HttpsError::ExistingHttpsSite(domain.clone()));
            }
            let mut http = matches.iter().filter(|site| site.http);
            let site = http
                .next()
                .ok_or_else(|| HttpsError::MissingHttpSite(domain.clone()))?;
            if http.next().is_some() {
                return Err(HttpsError::DuplicateHttpSite(domain.clone()));
            }
            if https.redirect_http {
                if !site.redirect_enabled {
                    return Err(HttpsError::MissingRedirectSnippet(domain.clone()));
                }
                redirects.push(domain.as_str());
            }
        }
        store.recover(&request.id)?;
        let timestamp = unix_time().as_secs() as i64;
        if !store.load_metadata(&request.id)?.is_some_and(|meta| {
            meta.schema_version == METADATA_SCHEMA_VERSION
                && meta.domains == request.domains
                && meta.directory == requests.acme.directory
                && meta.not_before_unix <= timestamp
                && meta.not_after_unix > timestamp
        }) {
            return Err(HttpsError::MissingCertificate(request.id.clone()));
        }
        let cert = store.certificate_dir(&request.id);
        let cert = cert
            .to_str()
            .filter(|path| {
                path.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
            })
            .ok_or(HttpsError::InvalidPath)?;
        writeln!(
            servers,
            "server {{\n    listen 443 ssl;\n    server_name {};\n    ssl_certificate {cert}/fullchain.pem;\n    ssl_certificate_key {cert}/privkey.pem;\n    include {};\n}}",
            request.domains.join(" "),
            https.include.display()
        ).expect("writing to String");
    }
    Ok(render_maps(id, &redirects) + &servers)
}

fn render_maps(id: &str, redirects: &[&str]) -> String {
    let mut generated = format!(
        "{GENERATION_PREFIX}{id}\nmap $host $rustinx_https_generation {{\n    default {id};\n}}\n\
         map \"$scheme:$host:$uri\" $rustinx_https_redirect {{\n    default \"\";\n\
         \x20   ~^http:[^:]+:/\\.well-known/acme-challenge/ \"\";\n"
    );
    for host in redirects {
        let pattern = host.replace('.', "\\.");
        writeln!(generated, "    ~^http:{pattern}: https://{host}$request_uri;").expect("writing to String");
    }
    generated.push_str("}\n");
    generated
}

fn generation(file: &str) -> &str {
    file.lines()
        .next()
        .and_then(|line| line.strip_prefix(GENERATION_PREFIX))
        .unwrap_or(INACTIVE)
}

fn saved_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state/https.conf")
}

fn read_saved(data_dir: &Path) -> Result<String, HttpsError> {
    match fs::read_to_string(saved_path(data_dir)) {
        Ok(saved) => Ok(saved),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(render_maps(INACTIVE, &[])),
        Err(err) => Err(err.into()),
    }
}

fn unix_time() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
}

async fn reload_and_confirm(nginx: &Nginx, config: &Config, expected: &str) -> Result<(), HttpsError> {
    nginx.reload(config.startup_timeout).await?;
    let deadline = Instant::now() + config.startup_timeout;
    while Instant::now() < deadline {
        let timeout = (deadline - Instant::now()).min(Duration::from_millis(500));
        if health::generation(config.health_addr, timeout)
            .await
            .is_ok_and(|id| id == expected)
        {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(HttpsError::ReloadTimeout(expected.to_owned()))
}

struct StagedFiles;

impl Drop for StagedFiles {
    fn drop(&mut self) {
        for path in [CANDIDATE, CANDIDATE_MAIN] {
            if let Err(err) = fs::remove_file(path)
                && err.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(target: "rustinx::https", path, error = %err, "could not remove staged Nginx file");
            }
        }
    }
}
