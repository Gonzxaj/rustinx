//! Probe for the internal Nginx health endpoint (`GET /healthz` on a loopback address).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const HEALTH_PATH: &str = "/healthz";
const MAX_RESPONSE_BYTES: u64 = 4096;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("health probe timed out after {0:?}")]
    Timeout(Duration),
    #[error("health probe I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("unexpected health response status line: {0:?}")]
    BadStatus(String),
}

/// Returns `Ok(())` only when the endpoint answers `200` within `timeout`.
pub async fn probe(addr: SocketAddr, timeout: Duration) -> Result<(), ProbeError> {
    tokio::time::timeout(timeout, request(addr, HEALTH_PATH))
        .await
        .map_err(|_| ProbeError::Timeout(timeout))?
        .map(|_| ())
}

pub(crate) async fn generation(addr: SocketAddr, timeout: Duration) -> Result<String, ProbeError> {
    let response = tokio::time::timeout(timeout, request(addr, "/__rustinx/reload"))
        .await
        .map_err(|_| ProbeError::Timeout(timeout))??;
    let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    Ok(body.trim().to_owned())
}

async fn request(addr: SocketAddr, path: &str) -> Result<String, ProbeError> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut response).await?;
    check_status(&response)?;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn check_status(response: &[u8]) -> Result<(), ProbeError> {
    let status_line = response.split(|b| *b == b'\n').next().unwrap_or_default();
    let status_line = String::from_utf8_lossy(status_line).trim_end().to_owned();
    let mut parts = status_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(version), Some("200")) if version.starts_with("HTTP/1.") => Ok(()),
        _ => Err(ProbeError::BadStatus(status_line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn serve_once(response: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        addr
    }

    #[test]
    fn status_line_parsing() {
        assert!(check_status(b"HTTP/1.1 200 OK\r\n\r\nok").is_ok());
        assert!(check_status(b"HTTP/1.0 200 OK\r\n").is_ok());
        assert!(check_status(b"HTTP/1.1 503 Service Unavailable\r\n").is_err());
        assert!(check_status(b"garbage").is_err());
        assert!(check_status(b"").is_err());
    }

    #[tokio::test]
    async fn healthy_endpoint() {
        let addr = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok\n").await;
        probe(addr, Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn unhealthy_status() {
        let addr = serve_once("HTTP/1.1 500 Internal Server Error\r\n\r\n").await;
        let err = probe(addr, Duration::from_secs(2)).await.unwrap_err();
        assert!(matches!(err, ProbeError::BadStatus(_)));
    }

    #[tokio::test]
    async fn connection_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = probe(addr, Duration::from_secs(2)).await.unwrap_err();
        assert!(matches!(err, ProbeError::Io(_)));
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _keep = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let err = probe(addr, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, ProbeError::Timeout(_)));
    }
}
