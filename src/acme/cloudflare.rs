//! Scoped Cloudflare TXT creation and durable cleanup under the issuance lock.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::certificates::CloudflareSettings;
use crate::storage::{StorageError, write_atomic};

const API: &str = "https://api.cloudflare.com/client/v4";
const RESOLVER: &str = "https://one.one.one.one/dns-query";

#[derive(Debug, Error)]
pub enum DnsError {
    #[error("could not read Cloudflare token file")]
    TokenFile,
    #[error("Cloudflare token file is empty or invalid")]
    Token,
    #[error("DNS provider request failed")]
    Transport,
    #[error("DNS provider rejected the request (HTTP {0})")]
    Http(u16),
    #[error("DNS provider response was invalid")]
    Response,
    #[error("Cloudflare returned an invalid record ID")]
    RecordId,
    #[error("the DNS cleanup journal belongs to another Cloudflare zone")]
    Zone,
    #[error("DNS challenge uses an unsupported identifier")]
    Identifier,
    #[error("could not read the DNS cleanup journal")]
    Journal,
    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub(super) struct Cloudflare {
    api: &'static str,
    resolver: &'static str,
    token: String,
    journal_path: PathBuf,
    journal: Journal,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    zone: String,
    records: Vec<Record>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    name: String,
    marker: String,
    id: Option<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct RemoteRecord {
    id: String,
    comment: Option<String>,
}

#[derive(Deserialize)]
struct Lookup {
    #[serde(rename = "Status")]
    status: u8,
    #[serde(default, rename = "Answer")]
    answers: Vec<Answer>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "type")]
    kind: u16,
    data: String,
}

impl Cloudflare {
    pub(super) fn open(settings: &CloudflareSettings, journal_path: PathBuf) -> Result<Self, DnsError> {
        let token = fs::read_to_string(&settings.token_file).map_err(|_| DnsError::TokenFile)?;
        let token = token.trim();
        if token.is_empty()
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(DnsError::Token);
        }
        let journal: Journal = match fs::read(&journal_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| DnsError::Journal)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Journal {
                zone: settings.zone_id.clone(),
                records: Vec::new(),
            },
            Err(_) => return Err(DnsError::Journal),
        };
        if journal.zone != settings.zone_id {
            return Err(DnsError::Zone);
        }
        Ok(Self {
            api: API,
            resolver: RESOLVER,
            token: token.into(),
            journal_path,
            journal,
        })
    }

    pub(super) async fn publish(&mut self, name: &str, value: &str) -> Result<(), DnsError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let marker = format!(
            "rustinx-dns01-{nonce}-{}-{}",
            std::process::id(),
            self.journal.records.len()
        );
        self.journal.records.push(Record {
            name: name.into(),
            marker: marker.clone(),
            id: None,
        });
        // Record the ownership marker first so even a lost POST response can be recovered.
        self.save()?;
        let payload = json!({"type": "TXT", "name": name, "content": format!("\"{value}\""), "ttl": 60, "comment": marker});
        let (_, body) = request("POST", &self.records_url(), Some(&self.token), Some(&payload)).await?;
        let record: RemoteRecord = decode(&body)?;
        validate_id(&record.id)?;
        self.journal.records.last_mut().expect("record was registered").id = Some(record.id);
        self.save()
    }

    pub(super) async fn wait_visible(&self, name: &str, value: &str) -> Result<(), DnsError> {
        loop {
            let url = format!("{}?name={name}&type=TXT", self.resolver);
            let (_, body) = request("GET", &url, None, None).await?;
            let lookup: Lookup = serde_json::from_slice(&body).map_err(|_| DnsError::Response)?;
            match lookup.status {
                0 if lookup.answers.iter().any(|answer| {
                    answer.kind == 16
                        && (answer.data == value
                            || answer.data.strip_prefix('"').and_then(|s| s.strip_suffix('"')) == Some(value))
                }) =>
                {
                    return Ok(());
                }
                0 | 3 => tokio::time::sleep(Duration::from_secs(2)).await,
                _ => return Err(DnsError::Response),
            }
        }
    }

    pub(super) async fn cleanup(&mut self) -> Result<(), DnsError> {
        let mut first_error = None;
        let mut index = 0;
        while index < self.journal.records.len() {
            let record = self.journal.records[index].clone();
            match self.remove(&record).await {
                Ok(()) => {
                    self.journal.records.remove(index);
                    self.save()?;
                }
                Err(err) => {
                    first_error.get_or_insert(err);
                    index += 1;
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn remove(&self, record: &Record) -> Result<(), DnsError> {
        let url = self.records_url();
        let id = match &record.id {
            Some(id) => id.clone(),
            None => {
                // A pending entry may correspond to a failed POST. Look up only its exact marker.
                let list = format!(
                    "{url}?type=TXT&name.exact={}&comment.exact={}&per_page=100",
                    record.name, record.marker
                );
                let (_, body) = request("GET", &list, Some(&self.token), None).await?;
                let records: Vec<RemoteRecord> = decode(&body)?;
                let Some(remote) = records
                    .into_iter()
                    .find(|remote| remote.comment.as_deref() == Some(&record.marker))
                else {
                    return Ok(());
                };
                remote.id
            }
        };
        validate_id(&id)?;
        let target = format!("{url}/{id}");
        match request("DELETE", &target, Some(&self.token), None).await {
            Ok((_, body)) => decode::<serde_json::Value>(&body).map(|_| ()),
            Err(DnsError::Http(404)) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn records_url(&self) -> String {
        format!("{}/zones/{}/dns_records", self.api, self.journal.zone)
    }

    fn save(&self) -> Result<(), DnsError> {
        if self.journal.records.is_empty() {
            return match fs::remove_file(&self.journal_path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(DnsError::Journal),
            };
        }
        let bytes = serde_json::to_vec(&self.journal).map_err(|_| DnsError::Journal)?;
        write_atomic(&self.journal_path, &bytes)?;
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), DnsError> {
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(DnsError::RecordId)
    }
}

fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, DnsError> {
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|_| DnsError::Response)?;
    envelope
        .result
        .filter(|_| envelope.success)
        .ok_or(DnsError::Response)
}

async fn request(
    method: &str,
    url: &str,
    token: Option<&str>,
    payload: Option<&serde_json::Value>,
) -> Result<(u16, Vec<u8>), DnsError> {
    // Keep the bearer token out of argv, the environment, and diagnostic output.
    let mut options = String::new();
    if let Some(token) = token {
        options.push_str(&format!("header = \"Authorization: Bearer {token}\"\n"));
    } else {
        options.push_str("header = \"Accept: application/dns-json\"\n");
    }
    if let Some(payload) = payload {
        let escaped = serde_json::to_string(&payload.to_string()).map_err(|_| DnsError::Response)?;
        options.push_str(&format!(
            "header = \"Content-Type: application/json\"\ndata-binary = {escaped}\n"
        ));
    }
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--disable",
            "--silent",
            "--max-time",
            "8",
            "--max-filesize",
            "1048576",
            "--request",
            method,
            "--write-out",
            "\n%{http_code}",
            "--config",
            "-",
            "--url",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| DnsError::Transport)?;
    child
        .stdin
        .take()
        .ok_or(DnsError::Transport)?
        .write_all(options.as_bytes())
        .await
        .map_err(|_| DnsError::Transport)?;
    let output = child.wait_with_output().await.map_err(|_| DnsError::Transport)?;
    if !output.status.success() {
        return Err(DnsError::Transport);
    }
    let split = output
        .stdout
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or(DnsError::Response)?;
    let status: u16 = std::str::from_utf8(&output.stdout[split + 1..])
        .map_err(|_| DnsError::Response)?
        .parse()
        .map_err(|_| DnsError::Response)?;
    if !(200..300).contains(&status) {
        return Err(DnsError::Http(status));
    }
    Ok((status, output.stdout[..split].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    const FIRST: &str = "11111111111111111111111111111111";
    const SECOND: &str = "22222222222222222222222222222222";
    const EXISTING: &str = "33333333333333333333333333333333";
    const NAME: &str = "_acme-challenge.example.test";

    async fn local_server(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (&'static str, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap()).leak();
        let server =
            tokio::spawn(async move {
                let mut requests = Vec::new();
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let header_end = loop {
                        bytes.push(stream.read_u8().await.unwrap());
                        if bytes.ends_with(b"\r\n\r\n") {
                            break bytes.len();
                        }
                    };
                    let content_length = String::from_utf8_lossy(&bytes)
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or_default();
                    bytes.resize(header_end + content_length, 0);
                    stream.read_exact(&mut bytes[header_end..]).await.unwrap();
                    requests.push(String::from_utf8(bytes).unwrap());
                    let body = body.to_string();
                    stream.write_all(format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ).as_bytes()).await.unwrap();
                }
                requests
            });
        (endpoint, server)
    }

    fn settings(root: &std::path::Path) -> CloudflareSettings {
        let token_file = root.join("token");
        fs::write(&token_file, "fake-token\n").unwrap();
        CloudflareSettings {
            zone_id: "00000000000000000000000000000000".into(),
            token_file,
        }
    }

    #[tokio::test]
    async fn creates_distinct_txt_records_and_recovers_cleanup_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let settings = settings(root.path());
        let journal = root.path().join("journal.json");
        let (endpoint, server) = local_server(vec![
            (200, json!({"success": true, "result": {"id": FIRST}})),
            (200, json!({"success": true, "result": {"id": SECOND}})),
            (200, json!({"Status": 3})),
            (200, json!({"Status": 0, "Answer": [{"type": 16, "data": "\"two\""}]})),
            (200, json!({"success": true, "result": [{"id": EXISTING}, {"id": FIRST, "comment": "our-pending-marker"}]})),
            (200, json!({"success": true, "result": {"id": FIRST}})),
            (200, json!({"success": true, "result": {"id": SECOND}})),
        ]).await;

        let mut cloudflare = Cloudflare::open(&settings, journal.clone()).unwrap();
        cloudflare.api = endpoint;
        cloudflare.resolver = endpoint;
        cloudflare.publish(NAME, "one").await.unwrap();
        cloudflare.publish(NAME, "two").await.unwrap();
        cloudflare.wait_visible(NAME, "two").await.unwrap();
        // Simulate a crash after a POST succeeded but before its returned ID was saved.
        cloudflare.journal.records[0].marker = "our-pending-marker".into();
        cloudflare.journal.records[0].id = None;
        cloudflare.save().unwrap();
        drop(cloudflare);

        let mut recovered = Cloudflare::open(&settings, journal.clone()).unwrap();
        recovered.api = endpoint;
        recovered.cleanup().await.unwrap();
        assert!(!journal.exists());
        let requests = server.await.unwrap();
        for (request, digest) in requests[..2].iter().zip(["one", "two"]) {
            assert!(request.starts_with("POST /zones/"));
            assert!(request.contains("Authorization: Bearer fake-token"));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["name"], NAME);
            assert_eq!(body["content"], format!("\"{digest}\""));
        }
        assert!(!requests[2].contains("Authorization:"));
        assert!(requests[4].contains("comment.exact=our-pending-marker"));
        assert!(requests[5].contains(&format!("/dns_records/{FIRST}")));
        assert!(requests[6].contains(&format!("/dns_records/{SECOND}")));
        assert!(requests.iter().all(|request| !request.contains(EXISTING)));
    }

    #[tokio::test]
    async fn reports_rejection_unavailability_and_propagation_timeout() {
        let root = tempfile::tempdir().unwrap();
        let settings = settings(root.path());
        let (endpoint, server) = local_server(vec![
            (403, json!({"success": false})),
            (503, json!({"success": false})),
            (
                200,
                json!({"Status": 0, "Answer": [{"type": 16, "data": "\"unrelated\""}]}),
            ),
        ])
        .await;
        let mut provider = Cloudflare::open(&settings, root.path().join("journal")).unwrap();
        provider.api = endpoint;
        provider.resolver = endpoint;
        assert!(matches!(
            provider.publish(NAME, "one").await,
            Err(DnsError::Http(403))
        ));
        assert!(matches!(provider.cleanup().await, Err(DnsError::Http(503))));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), provider.wait_visible(NAME, "one"))
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}
