//! Kage command-line entry point.
//!
//! Scaffold only. `serve` exposes the liveness endpoint and `healthcheck`
//! probes it; the remaining commands (`migrate`, `selftest`, `persona`,
//! `proxy`, `key`) land with the milestones that own them (see
//! `docs/02-plan.md`).

mod healthcheck;
mod serve;

use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Library crates composed into this binary, reported once at startup.
const COMPONENTS: [&str; 8] = [
    kage_api::CRATE_NAME,
    kage_browser::CRATE_NAME,
    kage_core::CRATE_NAME,
    kage_crawl::CRATE_NAME,
    kage_extract::CRATE_NAME,
    kage_http::CRATE_NAME,
    kage_proxy::CRATE_NAME,
    kage_store::CRATE_NAME,
];

/// Self-hosted page fetching API that presents as a real household user.
#[derive(Debug, Parser)]
#[command(name = "kage", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the API server until SIGINT or SIGTERM.
    Serve {
        /// Socket address to listen on.
        #[arg(long, env = "KAGE__SERVER__BIND", default_value = "0.0.0.0:8080")]
        bind: SocketAddr,
    },
    /// Probe the liveness endpoint of a running server; exits non-zero when
    /// it is not healthy. Used as the container healthcheck.
    Healthcheck {
        /// Address of the server to probe.
        #[arg(
            long,
            env = "KAGE__SERVER__HEALTHCHECK_ADDR",
            default_value = "127.0.0.1:8080"
        )]
        addr: SocketAddr,
    },
}

/// Installs the global JSON log subscriber. The level comes from `RUST_LOG`
/// and defaults to `info`.
fn init_tracing() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install the log subscriber: {error}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing()?;

    match cli.command {
        Command::Serve { bind } => {
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                components = ?COMPONENTS,
                "kage starting"
            );
            serve::run(bind).await.context("serve failed")
        }
        Command::Healthcheck { addr } => healthcheck::run(addr).await.context("healthcheck failed"),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{COMPONENTS, Cli, Command};

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_defaults_to_all_interfaces_on_8080() {
        let cli = Cli::try_parse_from(["kage", "serve"]).unwrap();

        let Command::Serve { bind } = cli.command else {
            panic!("expected the serve command, got {:?}", cli.command);
        };
        assert_eq!(bind.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn healthcheck_accepts_an_address() {
        let cli = Cli::try_parse_from(["kage", "healthcheck", "--addr", "127.0.0.1:9"]).unwrap();

        let Command::Healthcheck { addr } = cli.command else {
            panic!("expected the healthcheck command, got {:?}", cli.command);
        };
        assert_eq!(addr.port(), 9);
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(Cli::try_parse_from(["kage", "frobnicate"]).is_err());
    }

    #[test]
    fn every_library_crate_is_composed() {
        let mut names = COMPONENTS.to_vec();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), COMPONENTS.len(), "duplicate component name");
        assert!(names.iter().all(|name| name.starts_with("kage-")));
    }
}
