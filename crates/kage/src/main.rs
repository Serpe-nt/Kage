//! Kage command-line entry point.
//!
//! `serve` runs the API server, `healthcheck` probes it, `migrate` applies
//! the database schema and `config check` validates the configuration. The
//! remaining commands (`selftest`, `persona`, `proxy`, `key`) land with the
//! milestones that own them (see `docs/02-plan.md`).

mod config_cmd;
mod healthcheck;
mod migrate;
mod serve;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use kage_core::config::process_env;
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
    /// Configuration file. Without it, `kage.yaml` in the working directory
    /// is read when it exists. Environment variables of the form
    /// `KAGE__SECTION__KEY` override the file; secrets come only from them.
    #[arg(long, global = true, env = "KAGE_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the API server until SIGINT or SIGTERM.
    Serve {
        /// Socket address to listen on; overrides `server.bind`.
        #[arg(long)]
        bind: Option<SocketAddr>,
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
    /// Apply the database schema migrations. Applying them again changes
    /// nothing.
    Migrate {
        /// Change nothing; exit non-zero unless the schema is up to date.
        #[arg(long)]
        check: bool,
    },
    /// Inspect the configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the configuration and the policy files, then print the
    /// effective configuration as JSON with every secret redacted.
    Check,
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
    let config_file = config_cmd::config_file(cli.config);

    // `config check` prints one JSON document on stdout, so it runs before
    // the log subscriber, which also writes to stdout, is installed.
    if let Command::Config {
        command: ConfigCommand::Check,
    } = cli.command
    {
        let stdout = std::io::stdout();
        return config_cmd::check(config_file.as_deref(), process_env(), &mut stdout.lock());
    }

    init_tracing()?;
    match cli.command {
        Command::Serve { bind } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                components = ?COMPONENTS,
                "kage starting"
            );
            serve::run(config, bind).await.context("serve failed")
        }
        Command::Healthcheck { addr } => healthcheck::run(addr).await.context("healthcheck failed"),
        Command::Migrate { check } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            if check {
                migrate::check(&config).await
            } else {
                migrate::run(&config).await
            }
        }
        Command::Config {
            command: ConfigCommand::Check,
        } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::{CommandFactory, Parser};

    use super::{COMPONENTS, Cli, Command, ConfigCommand};

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_takes_its_address_from_configuration_unless_overridden() {
        let cli = Cli::try_parse_from(["kage", "serve"]).unwrap();
        let Command::Serve { bind } = cli.command else {
            panic!("expected the serve command, got {:?}", cli.command);
        };
        assert_eq!(bind, None);

        let cli = Cli::try_parse_from(["kage", "serve", "--bind", "127.0.0.1:9000"]).unwrap();
        let Command::Serve { bind } = cli.command else {
            panic!("expected the serve command, got {:?}", cli.command);
        };
        assert_eq!(bind.unwrap().to_string(), "127.0.0.1:9000");
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
    fn migrate_applies_by_default_and_checks_on_request() {
        let cli = Cli::try_parse_from(["kage", "migrate"]).unwrap();
        assert!(matches!(cli.command, Command::Migrate { check: false }));

        let cli = Cli::try_parse_from(["kage", "migrate", "--check"]).unwrap();
        assert!(matches!(cli.command, Command::Migrate { check: true }));
    }

    #[test]
    fn config_check_is_a_subcommand_and_the_file_is_a_global_option() {
        let cli = Cli::try_parse_from(["kage", "config", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                command: ConfigCommand::Check
            }
        ));
        assert_eq!(cli.config, None);

        // The option is accepted before and after the subcommand.
        for arguments in [
            ["kage", "--config", "/etc/kage/kage.yaml", "config", "check"],
            ["kage", "config", "check", "--config", "/etc/kage/kage.yaml"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(cli.config, Some(PathBuf::from("/etc/kage/kage.yaml")));
        }

        assert!(Cli::try_parse_from(["kage", "config"]).is_err());
        assert!(Cli::try_parse_from(["kage", "config", "print"]).is_err());
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
