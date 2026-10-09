//! `kage echo serve`: the fingerprint echo (spec §7.4).
//!
//! A small TLS server that answers every request with what the request
//! looked like on the wire: JA4, the HTTP/2 fingerprint, the header order.
//! It needs no database and no secrets of Kage, only a certificate and its
//! key, so it can run in the compose stack or on any host of its own. See
//! `docs/runbooks/fingerprint-echo.md`.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use kage_http::echo::{EchoServer, EchoTls};

/// What `kage echo serve` was asked to do.
#[derive(Debug, Clone)]
pub(crate) struct EchoOptions {
    /// Address to listen on.
    pub(crate) bind: SocketAddr,
    /// PEM file with the certificate chain.
    pub(crate) cert: PathBuf,
    /// PEM file with the private key.
    pub(crate) key: PathBuf,
}

/// Opens the echo's listener.
pub(crate) async fn bind(options: &EchoOptions) -> anyhow::Result<EchoServer> {
    let cert = std::fs::read(&options.cert)
        .with_context(|| format!("cannot read the certificate {}", options.cert.display()))?;
    let key = std::fs::read(&options.key)
        .with_context(|| format!("cannot read the private key {}", options.key.display()))?;
    let tls = EchoTls::from_pem(&cert, &key)?;
    let server = EchoServer::bind(options.bind, tls)
        .await
        .with_context(|| format!("cannot listen on {}", options.bind))?;
    Ok(server)
}

/// Serves the echo until `shutdown` resolves.
pub(crate) async fn serve(
    options: &EchoOptions,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let server = bind(options).await?;
    tracing::info!(
        address = %server.local_addr().context("the listener has no address")?,
        "the fingerprint echo is listening"
    );
    tokio::select! {
        () = server.serve() => {}
        () = shutdown => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{EchoOptions, serve};

    #[tokio::test]
    async fn missing_or_broken_certificate_files_are_named() {
        let dir = TempDir::new().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let options = EchoOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            cert: cert.clone(),
            key: key.clone(),
        };

        let error = serve(&options, async {}).await.unwrap_err();
        assert!(
            error.to_string().contains("cannot read the certificate"),
            "{error}"
        );

        std::fs::write(&cert, "not a certificate").unwrap();
        let error = serve(&options, async {}).await.unwrap_err();
        assert!(
            error.to_string().contains("cannot read the private key"),
            "{error}"
        );

        std::fs::write(&key, "not a key").unwrap();
        let error = serve(&options, async {}).await.unwrap_err();
        assert!(error.to_string().contains("not valid PEM"), "{error}");
        assert_eq!(options.cert, PathBuf::from(&cert));
    }
}
