//! Kage command-line entry point.
//!
//! `serve` runs the API server and the proxy pool, `healthcheck` probes the
//! server, `migrate` applies the database schema, `config check` validates
//! the configuration, `proxy` inspects and operates the proxy pool,
//! `selftest http` checks the http path's fingerprint, `selftest browser` runs
//! a browser context against the self-test pages, and `echo serve`
//! runs the fingerprint echo. The remaining commands (`persona`, `key`)
//! land with the milestones that own them (see `docs/02-plan.md`).

mod alerts;
mod config_cmd;
mod echo_cmd;
mod healthcheck;
mod migrate;
mod proxy_cmd;
mod selftest_browser;
mod selftest_cmd;
mod serve;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use kage_core::config::process_env;
use kage_core::domain::{Engine, ProxyState};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
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
    /// Inspect and operate the proxy pool. May run next to the server.
    Proxy {
        #[command(subcommand)]
        command: ProxyCommand,
    },
    /// Check that Kage still looks like the browsers it presents as.
    Selftest {
        #[command(subcommand)]
        command: SelftestCommand,
    },
    /// The fingerprint echo: a TLS server that tells a client what its
    /// connection and request looked like on the wire.
    Echo {
        #[command(subcommand)]
        command: EchoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SelftestCommand {
    /// Send one navigation through a proxy to a fingerprint echo and
    /// compare JA4, the HTTP/2 fingerprint and the header order with the
    /// values recorded for the profile. Exits non-zero when they differ.
    ///
    /// The echo is the operator's own when `selftest.http.echo_url` is set,
    /// else the third-party one, which sees the proxy's exit address.
    Http {
        /// The proxy to leave through.
        #[arg(long, value_name = "PROXY_ID")]
        proxy: ProxyId,
        /// The emulation profile to present as.
        #[arg(
            long,
            value_name = "NAME",
            help = "The emulation profile, for example chrome_149 or firefox_151"
        )]
        profile: String,
        /// The operating system the profile claims: windows, macos or
        /// linux.
        #[arg(long, default_value = "windows", value_name = "OS")]
        platform: String,
        /// Ask this echo instead of the configured one.
        #[arg(long, value_name = "URL")]
        echo: Option<String>,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Open a browser context through a proxy, load the self-test pages
    /// and let the sidecar judge what they show: automation flags and the
    /// coherence of the identity. Exits non-zero when the test fails.
    ///
    /// The pages are the operator's own (`selftest.browser.pages`).
    /// `--third-party` adds the detection pages run by others, which see
    /// the proxy's exit address.
    Browser {
        /// The proxy to leave through.
        #[arg(long, value_name = "PROXY_ID")]
        proxy: ProxyId,
        /// The engine whose sidecar runs the test.
        #[arg(long, default_value = "chromium", value_name = "ENGINE")]
        engine: Engine,
        /// The IANA time zone of the proxy's exit, for example
        /// Europe/Berlin.
        #[arg(long, value_name = "ZONE")]
        timezone: String,
        /// A language of the browser, most preferred first; repeat it.
        #[arg(long = "language", value_name = "TAG", required = true)]
        languages: Vec<String>,
        /// Also load `selftest.browser.third_party_pages`.
        #[arg(long)]
        third_party: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum EchoCommand {
    /// Serve the fingerprint echo until SIGINT or SIGTERM. Needs no
    /// database and none of Kage's secrets.
    Serve {
        /// Socket address to listen on.
        #[arg(long, env = "KAGE_ECHO_BIND", default_value = "0.0.0.0:8443")]
        bind: SocketAddr,
        /// PEM file with the certificate chain for the echo's host name.
        #[arg(long, env = "KAGE_ECHO_CERT", value_name = "FILE")]
        cert: PathBuf,
        /// PEM file with the certificate's private key.
        #[arg(long, env = "KAGE_ECHO_KEY", value_name = "FILE")]
        key: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the configuration and the policy files, then print the
    /// effective configuration as JSON with every secret redacted.
    Check,
}

#[derive(Debug, Subcommand)]
enum ProxyCommand {
    /// List the proxies in the pool with their state and last health check.
    List {
        /// Only proxies in this state: new, healthy, degraded, backoff,
        /// replacing or retired.
        #[arg(long)]
        state: Option<ProxyState>,
        /// Include retired proxies.
        #[arg(long)]
        all: bool,
        /// Sync with the provider's list first.
        #[arg(long)]
        sync: bool,
        /// Decrypt and show the proxy user names. Writes an audit event.
        #[arg(long)]
        reveal_username: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Probe proxies now, through the configured echo, and print latency,
    /// exit address and country. A proxy in backoff is probed too.
    Health {
        /// Only this proxy; repeat for several. Default: every live proxy.
        #[arg(long = "id", value_name = "PROXY_ID")]
        ids: Vec<ProxyId>,
        /// Do not sync with the provider's list first.
        #[arg(long)]
        no_sync: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Ask the provider to replace a proxy, within the monthly budget, and
    /// wait until it has. Exits non-zero when the proxy was not replaced.
    Replace {
        /// The proxy to replace.
        #[arg(value_name = "PROXY_ID")]
        id: ProxyId,
        /// Return as soon as the provider accepted the request; the server
        /// follows it up.
        #[arg(long)]
        no_wait: bool,
        /// How long to wait for the provider, for example 5m.
        #[arg(long, default_value = "5m", value_name = "DURATION")]
        wait_timeout: HumanDuration,
        /// Print one JSON document instead of a sentence.
        #[arg(long)]
        json: bool,
    },
    /// Show whether a leak is on record (`egress_unsafe`), and clear it.
    Egress {
        /// Declare every recorded leak dealt with. Do this only after the
        /// cause is fixed: while the flag is set nothing is fetched.
        #[arg(long)]
        clear: bool,
        /// Print one JSON document instead of sentences.
        #[arg(long)]
        json: bool,
    },
}

/// Installs the global JSON log subscriber. The level comes from `RUST_LOG`
/// and defaults to `info`. Commands that print their result on stdout log
/// to stderr.
fn init_tracing(to_stderr: bool) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().json().with_env_filter(filter);
    let installed = if to_stderr {
        builder.with_writer(std::io::stderr).try_init()
    } else {
        builder.try_init()
    };
    installed.map_err(|error| anyhow::anyhow!("failed to install the log subscriber: {error}"))
}

/// `kage selftest`. What was found is shown even when the self-test then
/// fails.
async fn selftest(command: SelftestCommand, config_file: Option<&Path>) -> anyhow::Result<()> {
    let config = config_cmd::load(config_file, process_env())?;
    let mut out = Vec::new();
    let result = match command {
        SelftestCommand::Http {
            proxy,
            profile,
            platform,
            echo,
            json,
        } => {
            let options = selftest_cmd::HttpSelftestOptions {
                proxy,
                profile,
                platform,
                echo,
                json,
            };
            selftest_cmd::http(&config, &options, &mut out).await
        }
        SelftestCommand::Browser {
            proxy,
            engine,
            timezone,
            languages,
            third_party,
            json,
        } => {
            let options = selftest_browser::BrowserSelftestOptions {
                proxy,
                engine,
                timezone,
                languages,
                third_party,
                json,
            };
            selftest_browser::browser(&config, &options, &mut out).await
        }
    };
    std::io::Write::write_all(&mut std::io::stdout().lock(), &out)
        .context("cannot write to stdout")?;
    result
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

    init_tracing(matches!(
        cli.command,
        Command::Proxy { .. } | Command::Selftest { .. }
    ))?;
    match cli.command {
        // The echo is a tool of its own: it reads no configuration.
        Command::Echo {
            command: EchoCommand::Serve { bind, cert, key },
        } => {
            let options = echo_cmd::EchoOptions { bind, cert, key };
            echo_cmd::serve(&options, serve::shutdown_signal())
                .await
                .context("the fingerprint echo failed")
        }
        Command::Selftest { command } => selftest(command, config_file.as_deref()).await,
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
        Command::Proxy { command } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            // The result is written in one piece once the command is done.
            let mut out = Vec::new();
            let result = proxy(&config, command, &mut out).await;
            // What was produced is shown even when the command then fails.
            std::io::Write::write_all(&mut std::io::stdout().lock(), &out)
                .context("cannot write to stdout")?;
            result
        }
    }
}

/// Runs one `kage proxy` command, writing its result to `out`.
async fn proxy(
    config: &kage_core::config::Config,
    command: ProxyCommand,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    match command {
        ProxyCommand::List {
            state,
            all,
            sync,
            reveal_username,
            json,
        } => {
            let options = proxy_cmd::ListOptions {
                state,
                // Asking for retired proxies implies seeing them.
                all: all || state == Some(ProxyState::Retired),
                sync,
                json,
                reveal_username,
            };
            proxy_cmd::list(config, &options, out).await
        }
        ProxyCommand::Health { ids, no_sync, json } => {
            let options = proxy_cmd::HealthOptions { ids, no_sync, json };
            proxy_cmd::health(config, &options, out).await
        }
        ProxyCommand::Replace {
            id,
            no_wait,
            wait_timeout,
            json,
        } => {
            let options = proxy_cmd::ReplaceOptions {
                id,
                wait: (!no_wait).then(|| wait_timeout.get()),
                json,
            };
            proxy_cmd::replace(config, &options, out).await
        }
        ProxyCommand::Egress { clear, json } => proxy_cmd::egress(config, clear, json, out).await,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use clap::{CommandFactory, Parser};
    use kage_core::domain::{Engine, ProxyState};

    use super::{
        COMPONENTS, Cli, Command, ConfigCommand, EchoCommand, ProxyCommand, SelftestCommand,
    };

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

    fn proxy(arguments: &[&str]) -> ProxyCommand {
        let mut all = vec!["kage", "proxy"];
        all.extend_from_slice(arguments);
        match Cli::try_parse_from(all).unwrap().command {
            Command::Proxy { command } => command,
            other => panic!("expected a proxy command, got {other:?}"),
        }
    }

    #[test]
    fn proxy_list_shows_live_proxies_as_a_table_by_default() {
        let ProxyCommand::List {
            state,
            all,
            sync,
            reveal_username,
            json,
        } = proxy(&["list"])
        else {
            panic!("expected the list command");
        };
        assert_eq!(state, None);
        assert!(!all && !sync && !reveal_username && !json);

        let ProxyCommand::List {
            state,
            all,
            sync,
            reveal_username,
            json,
        } = proxy(&[
            "list",
            "--state",
            "backoff",
            "--all",
            "--sync",
            "--reveal-username",
            "--json",
        ])
        else {
            panic!("expected the list command");
        };
        assert_eq!(state, Some(ProxyState::Backoff));
        assert!(all && sync && reveal_username && json);

        assert!(Cli::try_parse_from(["kage", "proxy", "list", "--state", "zombie"]).is_err());
    }

    #[test]
    fn proxy_health_takes_any_number_of_ids() {
        let ProxyCommand::Health { ids, no_sync, json } = proxy(&["health"]) else {
            panic!("expected the health command");
        };
        assert!(ids.is_empty() && !no_sync && !json);

        let first = "0199c2f0-0000-7000-8000-000000000001";
        let second = "0199c2f0-0000-7000-8000-000000000002";
        let ProxyCommand::Health { ids, no_sync, json } = proxy(&[
            "health",
            "--id",
            first,
            "--id",
            second,
            "--no-sync",
            "--json",
        ]) else {
            panic!("expected the health command");
        };
        assert_eq!(
            ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [first, second]
        );
        assert!(no_sync && json);

        assert!(Cli::try_parse_from(["kage", "proxy", "health", "--id", "seven"]).is_err());
    }

    #[test]
    fn proxy_replace_needs_an_id_and_waits_five_minutes_by_default() {
        let id = "0199c2f0-0000-7000-8000-000000000001";
        let ProxyCommand::Replace {
            id: parsed,
            no_wait,
            wait_timeout,
            json,
        } = proxy(&["replace", id])
        else {
            panic!("expected the replace command");
        };
        assert_eq!(parsed.to_string(), id);
        assert!(!no_wait && !json);
        assert_eq!(wait_timeout.get(), Duration::from_mins(5));

        let ProxyCommand::Replace {
            no_wait,
            wait_timeout,
            ..
        } = proxy(&["replace", id, "--no-wait", "--wait-timeout", "30s"])
        else {
            panic!("expected the replace command");
        };
        assert!(no_wait);
        assert_eq!(wait_timeout.get(), Duration::from_secs(30));

        assert!(Cli::try_parse_from(["kage", "proxy", "replace"]).is_err());
        assert!(Cli::try_parse_from(["kage", "proxy", "replace", "not-an-id"]).is_err());
        assert!(
            Cli::try_parse_from(["kage", "proxy", "replace", id, "--wait-timeout", "soon"])
                .is_err()
        );
    }

    #[test]
    fn proxy_egress_shows_by_default_and_clears_on_request() {
        assert!(matches!(
            proxy(&["egress"]),
            ProxyCommand::Egress {
                clear: false,
                json: false
            }
        ));
        assert!(matches!(
            proxy(&["egress", "--clear", "--json"]),
            ProxyCommand::Egress {
                clear: true,
                json: true
            }
        ));
        assert!(Cli::try_parse_from(["kage", "proxy"]).is_err());
        assert!(Cli::try_parse_from(["kage", "proxy", "delete"]).is_err());
    }

    #[test]
    fn selftest_browser_needs_a_proxy_a_time_zone_and_a_language() {
        let id = "0199c2f0-0000-7000-8000-000000000001";
        let base = ["kage", "selftest", "browser", "--proxy", id];
        let full = [
            "--timezone",
            "Europe/Vienna",
            "--language",
            "de-AT",
            "--language",
            "de",
        ];
        let cli = Cli::try_parse_from(base.into_iter().chain(full)).unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Browser {
                    proxy,
                    engine,
                    timezone,
                    languages,
                    third_party,
                    json,
                },
        } = cli.command
        else {
            panic!("expected the browser self-test, got {:?}", cli.command);
        };
        assert_eq!(proxy.to_string(), id);
        assert_eq!(engine, Engine::Chromium);
        assert_eq!(timezone, "Europe/Vienna");
        assert_eq!(languages, ["de-AT", "de"]);
        assert!(!third_party);
        assert!(!json);

        let flags = ["--engine", "firefox", "--third-party", "--json"];
        let cli = Cli::try_parse_from(base.into_iter().chain(full).chain(flags)).unwrap();
        assert!(matches!(
            cli.command,
            Command::Selftest {
                command: SelftestCommand::Browser {
                    engine: Engine::Firefox,
                    third_party: true,
                    json: true,
                    ..
                }
            }
        ));

        for arguments in [
            base.to_vec(),
            [&base[..], &["--timezone", "Europe/Vienna"]].concat(),
            [&base[..], &["--language", "de"]].concat(),
            [&base[..], &full[..], &["--engine", "safari"]].concat(),
        ] {
            assert!(
                Cli::try_parse_from(arguments.clone()).is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn selftest_http_needs_a_proxy_and_a_profile() {
        let id = "0199c2f0-0000-7000-8000-000000000001";
        let cli = Cli::try_parse_from([
            "kage",
            "selftest",
            "http",
            "--proxy",
            id,
            "--profile",
            "chrome_149",
        ])
        .unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Http {
                    proxy,
                    profile,
                    platform,
                    echo,
                    json,
                },
        } = cli.command
        else {
            panic!("expected the selftest command, got {:?}", cli.command);
        };
        assert_eq!(proxy.to_string(), id);
        assert_eq!(profile, "chrome_149");
        assert_eq!(platform, "windows");
        assert_eq!(echo, None);
        assert!(!json);

        let cli = Cli::try_parse_from([
            "kage",
            "selftest",
            "http",
            "--proxy",
            id,
            "--profile",
            "firefox_151",
            "--platform",
            "macos",
            "--echo",
            "https://echo.example.org/",
            "--json",
        ])
        .unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Http {
                    platform,
                    echo,
                    json,
                    ..
                },
        } = cli.command
        else {
            panic!("expected the selftest command");
        };
        assert_eq!(platform, "macos");
        assert_eq!(echo.as_deref(), Some("https://echo.example.org/"));
        assert!(json);

        for arguments in [
            vec!["kage", "selftest"],
            vec!["kage", "selftest", "http"],
            vec!["kage", "selftest", "http", "--proxy", id],
            vec!["kage", "selftest", "http", "--profile", "chrome_149"],
            vec![
                "kage",
                "selftest",
                "http",
                "--proxy",
                "seven",
                "--profile",
                "chrome_149",
            ],
            vec!["kage", "selftest", "browser"],
        ] {
            assert!(
                Cli::try_parse_from(arguments.clone()).is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn echo_serve_needs_a_certificate_and_a_key() {
        let cli = Cli::try_parse_from([
            "kage",
            "echo",
            "serve",
            "--cert",
            "/run/echo/cert.pem",
            "--key",
            "/run/echo/key.pem",
        ])
        .unwrap();
        let Command::Echo {
            command: EchoCommand::Serve { bind, cert, key },
        } = cli.command
        else {
            panic!("expected the echo command, got {:?}", cli.command);
        };
        assert_eq!(bind.to_string(), "0.0.0.0:8443");
        assert_eq!(cert, PathBuf::from("/run/echo/cert.pem"));
        assert_eq!(key, PathBuf::from("/run/echo/key.pem"));

        let cli = Cli::try_parse_from([
            "kage",
            "echo",
            "serve",
            "--bind",
            "127.0.0.1:9443",
            "--cert",
            "c",
            "--key",
            "k",
        ])
        .unwrap();
        let Command::Echo {
            command: EchoCommand::Serve { bind, .. },
        } = cli.command
        else {
            panic!("expected the echo command");
        };
        assert_eq!(bind.port(), 9443);
        assert!(Cli::try_parse_from(["kage", "echo"]).is_err());
        assert!(Cli::try_parse_from(["kage", "echo", "serve", "--bind", "nowhere"]).is_err());
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
