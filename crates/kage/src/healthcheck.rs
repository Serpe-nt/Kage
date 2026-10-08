//! `kage healthcheck`: a dependency-free liveness probe for the container
//! healthcheck. The runtime image has no shell and no curl, so the binary
//! probes itself.
//!
//! This is a loopback request to the local server, not traffic to a target
//! site, which is why it does not go through the wreq HTTP path.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Upper bound for the whole probe. The container healthcheck adds its own
/// timeout on top of this one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Only the status line is inspected; this caps how much of a response is read.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024;

/// Succeeds when the server at `addr` answers its liveness endpoint with 200.
pub(crate) async fn run(addr: SocketAddr) -> anyhow::Result<()> {
    timeout(PROBE_TIMEOUT, probe(addr))
        .await
        .with_context(|| format!("no answer from {addr} within {PROBE_TIMEOUT:?}"))?
}

async fn probe(addr: SocketAddr) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("cannot connect to {addr}"))?;

    let path = kage_api::HEALTHZ_PATH;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("cannot send the probe request")?;

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .await
        .context("cannot read the probe response")?;

    check_status_line(&String::from_utf8_lossy(&response))
}

/// Accepts exactly an HTTP/1.x response whose status code is 200.
fn check_status_line(response: &str) -> anyhow::Result<()> {
    let status_line = response.lines().next().unwrap_or_default();
    let mut parts = status_line.split_ascii_whitespace();
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        bail!("malformed status line: {status_line:?}");
    };

    ensure!(
        version.starts_with("HTTP/1."),
        "unexpected protocol in status line: {status_line:?}"
    );
    ensure!(code == "200", "unhealthy, status line: {status_line:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{check_status_line, run};
    use crate::serve::test_support::RunningServer;

    #[tokio::test]
    async fn healthy_server_passes() {
        let server = RunningServer::start().await;

        run(server.addr).await.unwrap();

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn closed_port_fails() {
        // Bind then drop to get a loopback port that nothing listens on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let error = run(addr).await.unwrap_err();

        assert!(format!("{error:#}").contains("cannot connect"), "{error:#}");
    }

    #[tokio::test]
    async fn non_200_answer_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        let error = run(addr).await.unwrap_err();

        assert!(format!("{error:#}").contains("503"), "{error:#}");
        responder.await.unwrap();
    }

    #[test]
    fn status_line_rules() {
        assert!(check_status_line("HTTP/1.1 200 OK\r\n\r\n").is_ok());
        assert!(check_status_line("HTTP/1.0 200 OK\r\n\r\n").is_ok());
        assert!(check_status_line("HTTP/1.1 204 No Content\r\n\r\n").is_err());
        assert!(check_status_line("HTTP/1.1 2000 OK\r\n\r\n").is_err());
        assert!(check_status_line("SSH-2.0 200\r\n").is_err());
        assert!(check_status_line("garbage").is_err());
        assert!(check_status_line("").is_err());
    }
}
