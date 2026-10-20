//! Kage command-line entry point.
//!
//! `serve` runs the API server, the proxy pool, the persona manager, the
//! scheduler and the forwarder, `healthcheck` probes the server, `migrate`
//! applies the database schema, `config check` validates the
//! configuration, `proxy` inspects and operates the proxy pool, `persona`
//! inspects and operates the personas, `fetch` runs one request through
//! the whole pipeline, `selftest http` checks the http path's
//! fingerprint, `selftest browser` runs a browser context against the
//! self-test pages (both for a proxy with a stand-in, or for a persona),
//! `echo serve` runs the fingerprint echo, `key` makes, lists, revokes and
//! rotates API keys, and `openapi` prints the document of the public API.

mod alerts;
mod config_cmd;
mod echo_cmd;
mod engine;
mod fetch_cmd;
mod healthcheck;
mod key_cmd;
mod migrate;
mod persona_cmd;
mod proxy_cmd;
mod selftest_browser;
mod selftest_cmd;
mod serve;
mod telemetry;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use kage_core::config::process_env;
use kage_core::domain::{ClaimedOs, Engine, Level, PersonaState, ProxyState, Scope};
use kage_core::duration::HumanDuration;
use kage_core::ids::{ApiKeyId, PersonaId, ProxyId};

/// Library crates composed into this binary, reported once at startup.
const COMPONENTS: [&str; 10] = [
    kage_api::CRATE_NAME,
    kage_browser::CRATE_NAME,
    kage_core::CRATE_NAME,
    kage_crawl::CRATE_NAME,
    kage_extract::CRATE_NAME,
    kage_http::CRATE_NAME,
    kage_persona::CRATE_NAME,
    kage_proxy::CRATE_NAME,
    kage_scheduler::CRATE_NAME,
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
    /// Inspect and operate the personas. May run next to the server.
    Persona {
        #[command(subcommand)]
        command: PersonaCommand,
    },
    /// Fetch one page through the whole pipeline: the domain policy, the
    /// scheduler, a persona, its path, the classifier and the store. May
    /// run next to the server. Exits non-zero unless the page was fetched.
    Fetch(FetchArgs),
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
    /// Make, list, revoke and rotate API keys. May run next to the
    /// server, and before one was ever started: the first admin key of a
    /// deployment is made here.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Print the OpenAPI document of the public API, as `/openapi.json`
    /// serves it. Needs no configuration.
    Openapi,
}

/// What `kage fetch` takes.
#[derive(Debug, clap::Args)]
struct FetchArgs {
    /// The page.
    #[arg(value_name = "URL")]
    url: url::Url,
    /// The camouflage level, required: 0 (a plain HTTP client that says
    /// it is Kage, no persona), 1 (a persona's http client), 2 (a
    /// persona's browser that loads the page and leaves) or 3 (a
    /// persona's browser as a person). Below 3 Kage is not pretending to
    /// be a person.
    #[arg(long, value_name = "LEVEL")]
    level: Level,
    /// Only this persona.
    #[arg(long, value_name = "PERSONA_ID")]
    persona: Option<PersonaId>,
    /// Only personas on this engine: chromium or firefox.
    #[arg(long)]
    engine: Option<Engine>,
    /// Longest one attempt may take, for example 45s.
    #[arg(long, value_name = "DURATION")]
    timeout: Option<HumanDuration>,
    /// Retries after the first attempt, instead of the policy's.
    #[arg(long, value_name = "N")]
    retries: Option<u32>,
    /// A pacing profile instead of the policy's, unless the policy
    /// locks pacing.
    #[arg(long, value_name = "PROFILE")]
    pacing: Option<String>,
    /// Write the document (the DOM of a browser attempt, the body of
    /// an http one) to this file.
    #[arg(long, short, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print one JSON document instead of sentences.
    #[arg(long)]
    json: bool,
    /// Also read what the page says from the document that was
    /// fetched: its title, its main text as text and markdown, its
    /// links and its metadata.
    #[arg(long)]
    extract: bool,
    /// For how long the result may be answered from the response
    /// cache, for example 10m; 0s is never. Left out, the policy of
    /// the site decides. The command line's entries are its own: no
    /// API key reads them.
    #[arg(long, value_name = "DURATION")]
    cache_ttl: Option<HumanDuration>,
    /// What to do about the site's robots.txt: policy (what the
    /// domain policy says), report, enforce or ignore. More care than
    /// the policy takes may be asked for, never less.
    #[arg(long, default_value = "policy", value_parser = fetch_cmd::parse_robots)]
    robots: kage_scheduler::RobotsWish,
}

impl From<FetchArgs> for fetch_cmd::FetchOptions {
    fn from(args: FetchArgs) -> Self {
        Self {
            url: args.url,
            level: args.level,
            persona: args.persona,
            engine: args.engine,
            timeout: args.timeout.map(HumanDuration::get),
            retries: args.retries,
            pacing: args.pacing,
            output: args.output,
            json: args.json,
            extract: args.extract,
            cache_ttl: args.cache_ttl.map(HumanDuration::get),
            robots: args.robots,
        }
    }
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Make a key and print its token. The token is shown this once:
    /// only a hash of its secret is stored.
    Create {
        /// A name for humans.
        #[arg(long)]
        name: String,
        /// What the key may do: fetch, jobs, crawl, sessions or admin.
        /// Repeat it; scopes are independent, admin implies no other.
        #[arg(long = "scope", value_name = "SCOPE", required = true)]
        scopes: Vec<Scope>,
        /// Requests per minute; 60 when left out.
        #[arg(long, value_name = "N")]
        requests_per_minute: Option<u32>,
        /// Requests in flight at once; 4 when left out.
        #[arg(long, value_name = "N")]
        max_concurrent: Option<u32>,
        /// Pages per day (UTC); no budget when left out.
        #[arg(long, value_name = "N")]
        daily_page_budget: Option<u32>,
        /// Print one JSON document instead of sentences.
        #[arg(long)]
        json: bool,
    },
    /// List the keys. Never prints a token: Kage does not have them.
    List {
        /// Include disabled keys.
        #[arg(long)]
        all: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Disable a key: it stops authenticating at once. Its row and its
    /// audit trail stay.
    Revoke {
        /// The key.
        #[arg(value_name = "KEY_ID")]
        id: ApiKeyId,
        /// Print one JSON document instead of a sentence.
        #[arg(long)]
        json: bool,
    },
    /// Give a key a new secret and print its new token. The old token
    /// stops working at once.
    Rotate {
        /// The key.
        #[arg(value_name = "KEY_ID")]
        id: ApiKeyId,
        /// Print one JSON document instead of sentences.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PersonaCommand {
    /// List the personas and how they spread over `personas.mix`.
    List {
        /// Only personas in this state: active, cooling or retired.
        #[arg(long)]
        state: Option<PersonaState>,
        /// Include retired personas.
        #[arg(long)]
        all: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one persona: its identity, its self-test runs and what it did
    /// on which domain.
    Show {
        /// The persona.
        #[arg(value_name = "PERSONA_ID")]
        id: PersonaId,
        /// Include the stored fingerprint document (with --json).
        #[arg(long, requires = "json")]
        fingerprint: bool,
        /// Print one JSON document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Retire a persona: its sessions end, its http cookies are deleted,
    /// and its profile is archived and later deleted. Its proxy gets a new
    /// persona at the server's next sweep.
    Retire {
        /// The persona.
        #[arg(value_name = "PERSONA_ID")]
        id: PersonaId,
        /// Print one JSON document instead of a sentence.
        #[arg(long)]
        json: bool,
    },
    /// Run the self-tests of a persona through its own proxy: the http
    /// path when it has one, and its own browser context. Exits non-zero
    /// when one fails. A passing run ends a cooling that waits for one.
    ///
    /// By default the operator's own echo and pages are used.
    /// `--third-party` uses the echo and the detection pages run by
    /// others, which see the exit address of the persona's proxy.
    Selftest {
        /// The persona.
        #[arg(value_name = "PERSONA_ID")]
        id: PersonaId,
        /// Use `selftest.http.third_party_url` and also load
        /// `selftest.browser.third_party_pages`.
        #[arg(long)]
        third_party: bool,
        /// Print one JSON document instead of tables.
        #[arg(long)]
        json: bool,
    },
    /// Replace a persona with a new one behind the same proxy. The new
    /// persona inherits nothing.
    Regenerate {
        /// The persona.
        #[arg(value_name = "PERSONA_ID")]
        id: PersonaId,
        /// Print one JSON document instead of a sentence.
        #[arg(long)]
        json: bool,
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
        #[arg(
            long,
            value_name = "PROXY_ID",
            required_unless_present = "persona",
            conflicts_with = "persona"
        )]
        proxy: Option<ProxyId>,
        /// The emulation profile to present as.
        #[arg(
            long,
            value_name = "NAME",
            required_unless_present = "persona",
            conflicts_with = "persona",
            help = "The emulation profile, for example chrome_149 or firefox_151"
        )]
        profile: Option<String>,
        /// Run the test as this persona instead: its profile, its
        /// identity, its proxy. The run is recorded for the persona.
        #[arg(long, value_name = "PERSONA_ID")]
        persona: Option<PersonaId>,
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
        /// The proxy to leave through, with a stand-in persona.
        #[arg(
            long,
            value_name = "PROXY_ID",
            required_unless_present = "persona",
            conflicts_with = "persona"
        )]
        proxy: Option<ProxyId>,
        /// Run the test as this persona instead, in its own context:
        /// its fingerprint, its profile, its proxy. The run is recorded
        /// for the persona.
        #[arg(long, value_name = "PERSONA_ID")]
        persona: Option<PersonaId>,
        /// The engine whose sidecar runs the test.
        #[arg(
            long,
            default_value = "chromium",
            value_name = "ENGINE",
            conflicts_with = "persona"
        )]
        engine: Engine,
        /// The system the stand-in persona claims: windows or macos on
        /// firefox. Default: the engine's own (linux on chromium, windows
        /// on firefox); a chromium sidecar refuses anything but linux.
        #[arg(long, value_name = "OS", conflicts_with = "persona")]
        claimed_os: Option<ClaimedOs>,
        /// The IANA time zone of the stand-in persona, for example
        /// Europe/Berlin. Default: the one of the proxy's geo record.
        #[arg(long, value_name = "ZONE", conflicts_with = "persona")]
        timezone: Option<String>,
        /// A language of the stand-in persona, most preferred first;
        /// repeat it. Default: the list the engine's browser ships with
        /// in the proxy's country.
        #[arg(long = "language", value_name = "TAG", conflicts_with = "persona")]
        languages: Vec<String>,
        /// Also load `selftest.browser.third_party_pages`.
        #[arg(long)]
        third_party: bool,
        /// Keep the context open and untouched for this long before the
        /// pages are loaded, for example 10m. For measuring what the
        /// browser sends by itself; the sidecar's idle limit
        /// (`SIDECAR_CONTEXT_IDLE_MS`) must be longer.
        #[arg(long, value_name = "DURATION")]
        idle_before: Option<HumanDuration>,
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

/// `kage key`.
async fn key(
    config: &kage_core::config::Config,
    command: KeyCommand,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    match command {
        KeyCommand::Create {
            name,
            scopes,
            requests_per_minute,
            max_concurrent,
            daily_page_budget,
            json,
        } => {
            let options = key_cmd::CreateOptions {
                name,
                scopes,
                requests_per_minute,
                max_concurrent,
                daily_page_budget,
                json,
            };
            key_cmd::create(config, &options, out).await
        }
        KeyCommand::List { all, json } => key_cmd::list(config, all, json, out).await,
        KeyCommand::Revoke { id, json } => key_cmd::revoke(config, id, json, out).await,
        KeyCommand::Rotate { id, json } => key_cmd::rotate(config, id, json, out).await,
    }
}

/// Installs the global JSON log subscriber. The level comes from `RUST_LOG`
/// and defaults to `info`. Commands that print their result on stdout log
/// to stderr.
/// Installs the log subscriber of a command that sends no traces.
fn init_tracing(to_stderr: bool) -> anyhow::Result<()> {
    telemetry::install(to_stderr, &kage_core::config::TelemetryConfig::default()).map(drop)
}

/// `kage selftest`. What was found is shown even when the self-test then
/// fails.
async fn selftest(command: SelftestCommand, config_file: Option<&Path>) -> anyhow::Result<()> {
    let config = config_cmd::load(config_file, process_env())?;
    let mut out = Vec::new();
    let result = match command {
        SelftestCommand::Http {
            persona: Some(id),
            echo,
            json,
            ..
        } => {
            let options = persona_cmd::SelftestOptions {
                id,
                paths: persona_cmd::Paths::Http,
                // As without a persona: the operator's own echo when one
                // is configured, else the one run by others.
                third_party: config.selftest.http.echo_url.is_none(),
                echo,
                json,
            };
            persona_cmd::selftest(&config, &options, &mut out).await
        }
        SelftestCommand::Http {
            proxy,
            profile,
            platform,
            echo,
            json,
            persona: None,
        } => match (proxy, profile) {
            (Some(proxy), Some(profile)) => {
                let options = selftest_cmd::HttpSelftestOptions {
                    proxy,
                    profile,
                    platform,
                    echo,
                    json,
                };
                selftest_cmd::http(&config, &options, &mut out).await
            }
            _ => Err(anyhow::anyhow!("name a proxy and a profile, or a persona")),
        },
        SelftestCommand::Browser {
            persona: Some(id),
            third_party,
            json,
            ..
        } => {
            let options = persona_cmd::SelftestOptions {
                id,
                paths: persona_cmd::Paths::Browser,
                third_party,
                echo: None,
                json,
            };
            persona_cmd::selftest(&config, &options, &mut out).await
        }
        SelftestCommand::Browser {
            proxy,
            engine,
            claimed_os,
            timezone,
            languages,
            third_party,
            idle_before,
            json,
            persona: None,
        } => match proxy {
            Some(proxy) => {
                let options = selftest_browser::BrowserSelftestOptions {
                    proxy,
                    engine,
                    claimed_os,
                    timezone,
                    languages,
                    third_party,
                    idle_before: idle_before.map(HumanDuration::get),
                    json,
                };
                selftest_browser::browser(&config, &options, &mut out).await
            }
            None => Err(anyhow::anyhow!("name a proxy or a persona")),
        },
    };
    std::io::Write::write_all(&mut std::io::stdout().lock(), &out)
        .context("cannot write to stdout")?;
    result
}

/// `kage openapi`: the document of the public API on stdout, as
/// `/openapi.json` serves it, with a newline at its end.
fn print_openapi() -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer_pretty(&mut out, &kage_api::openapi())
        .context("cannot write the OpenAPI document")?;
    std::io::Write::write_all(&mut out, b"\n").context("cannot write to stdout")
}

/// Writes what a command produced to stdout, whatever came of the command:
/// what was found, or a token that was made, is shown even when the command
/// then fails.
fn shown(out: &[u8], result: anyhow::Result<()>) -> anyhow::Result<()> {
    std::io::Write::write_all(&mut std::io::stdout().lock(), out)
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

    // The document is the whole output: nothing else may reach stdout.
    if matches!(cli.command, Command::Openapi) {
        return print_openapi();
    }

    // The server sends its traces where its configuration says, so its
    // subscriber is installed once the configuration is read.
    if let Command::Serve { bind } = cli.command {
        let config = config_cmd::load(config_file.as_deref(), process_env())?;
        let telemetry = telemetry::install(false, &config.telemetry)?;
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            components = ?COMPONENTS,
            traces = config.telemetry.otlp_endpoint.is_some(),
            "kage starting"
        );
        let served = serve::run(config, bind).await.context("serve failed");
        telemetry.shut_down().await;
        return served;
    }
    init_tracing(matches!(
        cli.command,
        Command::Proxy { .. }
            | Command::Persona { .. }
            | Command::Selftest { .. }
            | Command::Fetch(_)
            | Command::Key { .. }
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
        Command::Fetch(args) => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            let options = fetch_cmd::FetchOptions::from(args);
            let mut out = Vec::new();
            let result = Box::pin(fetch_cmd::fetch(&config, &options, &mut out)).await;
            shown(&out, result)
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
        }
        | Command::Openapi
        | Command::Serve { .. } => Ok(()),
        Command::Key { command } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            let mut out = Vec::new();
            let result = key(&config, command, &mut out).await;
            shown(&out, result)
        }
        Command::Persona { command } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            let mut out = Vec::new();
            let result = persona(&config, command, &mut out).await;
            shown(&out, result)
        }
        Command::Proxy { command } => {
            let config = config_cmd::load(config_file.as_deref(), process_env())?;
            // The result is written in one piece once the command is done.
            let mut out = Vec::new();
            let result = proxy(&config, command, &mut out).await;
            shown(&out, result)
        }
    }
}

/// Runs one `kage persona` command, writing its result to `out`.
async fn persona(
    config: &kage_core::config::Config,
    command: PersonaCommand,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    match command {
        PersonaCommand::List { state, all, json } => {
            let options = persona_cmd::ListOptions {
                state,
                // Asking for retired personas implies seeing them.
                all: all || state == Some(PersonaState::Retired),
                json,
            };
            persona_cmd::list(config, options, out).await
        }
        PersonaCommand::Show {
            id,
            fingerprint,
            json,
        } => persona_cmd::show(config, id, fingerprint, json, out).await,
        PersonaCommand::Retire { id, json } => persona_cmd::retire(config, id, json, out).await,
        PersonaCommand::Selftest {
            id,
            third_party,
            json,
        } => {
            let options = persona_cmd::SelftestOptions {
                id,
                paths: persona_cmd::Paths::Both,
                third_party,
                echo: None,
                json,
            };
            persona_cmd::selftest(config, &options, out).await
        }
        PersonaCommand::Regenerate { id, json } => {
            persona_cmd::regenerate(config, id, json, out).await
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
    use kage_core::domain::{ClaimedOs, Engine, Level, PersonaState, ProxyState};
    use kage_core::duration::HumanDuration;
    use kage_scheduler::RobotsWish;

    use super::{
        COMPONENTS, Cli, Command, ConfigCommand, EchoCommand, PersonaCommand, ProxyCommand,
        SelftestCommand,
    };

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn fetch_takes_a_url_and_a_level_and_leaves_the_rest_to_the_policy() {
        let cli =
            Cli::try_parse_from(["kage", "fetch", "https://shop.example/page", "--level", "3"])
                .unwrap();
        let Command::Fetch(plain) = cli.command else {
            panic!("expected the fetch command, got {:?}", cli.command);
        };
        assert_eq!(plain.url.as_str(), "https://shop.example/page");
        assert_eq!(plain.level, Level::Human);
        assert_eq!(
            (plain.persona, plain.engine, plain.timeout, plain.retries),
            (None, None, None, None)
        );
        assert_eq!(
            (plain.pacing, plain.output, plain.json),
            (None, None, false)
        );
        // Nothing is extracted, the cache and robots.txt are the policy's.
        assert_eq!(
            (plain.extract, plain.cache_ttl, plain.robots),
            (false, None, RobotsWish::Policy)
        );

        let cli = Cli::try_parse_from([
            "kage",
            "fetch",
            "https://shop.example/page",
            "--level",
            "2",
            "--persona",
            "0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e",
            "--engine",
            "firefox",
            "--timeout",
            "45s",
            "--retries",
            "0",
            "--pacing",
            "fast",
            "-o",
            "page.html",
            "--json",
        ])
        .unwrap();
        let Command::Fetch(asked) = cli.command else {
            panic!("expected the fetch command, got {:?}", cli.command);
        };
        assert_eq!(asked.level, Level::Fast);
        assert_eq!(
            asked.persona.map(|persona| persona.to_string()).as_deref(),
            Some("0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e")
        );
        assert_eq!(asked.engine, Some(Engine::Firefox));
        assert_eq!(
            asked.timeout.map(HumanDuration::get),
            Some(Duration::from_secs(45))
        );
        assert_eq!(asked.retries, Some(0));
        assert_eq!(asked.pacing.as_deref(), Some("fast"));
        assert_eq!(asked.output, Some(PathBuf::from("page.html")));
        assert!(asked.json);

        // Not a URL, not a path: refused before anything starts.
        assert!(Cli::try_parse_from(["kage", "fetch", "not a url", "--level", "0"]).is_err());
        // Nothing is decided for the caller: the level is named, and is one.
        assert!(Cli::try_parse_from(["kage", "fetch", "https://shop.example/"]).is_err());
        for wrong in ["4", "tor", "-1"] {
            assert!(
                Cli::try_parse_from(["kage", "fetch", "https://shop.example/", "--level", wrong])
                    .is_err(),
                "{wrong}"
            );
        }
        // The path of 1.x is gone.
        assert!(
            Cli::try_parse_from([
                "kage",
                "fetch",
                "https://shop.example/",
                "--level",
                "3",
                "--render",
                "browser"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["kage", "fetch"]).is_err());
    }

    #[test]
    fn fetch_says_what_a_request_may_say_about_extraction_the_cache_and_robots_txt() {
        let cli = Cli::try_parse_from([
            "kage",
            "fetch",
            "https://shop.example/page",
            "--level",
            "1",
            "--extract",
            "--cache-ttl",
            "10m",
            "--robots",
            "enforce",
        ])
        .unwrap();
        let Command::Fetch(asked) = cli.command else {
            panic!("expected the fetch command, got {:?}", cli.command);
        };
        // As the command hands it on.
        let options = crate::fetch_cmd::FetchOptions::from(asked);
        assert!(options.extract);
        assert_eq!(options.cache_ttl, Some(Duration::from_mins(10)));
        assert_eq!(options.robots, RobotsWish::Enforce);
        assert_eq!(options.url.as_str(), "https://shop.example/page");
        assert_eq!(options.level, Level::Emulated);
        assert_eq!((options.timeout, options.retries), (None, None));
        // A word that is none of the four, and a time that is none.
        for refused in [["--robots", "obey"], ["--cache-ttl", "soon"]] {
            let mut arguments = vec!["kage", "fetch", "https://shop.example/page", "--level", "0"];
            arguments.extend(refused);
            assert!(Cli::try_parse_from(arguments).is_err(), "{refused:?}");
        }
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
    #[allow(clippy::too_many_lines)]
    fn selftest_browser_needs_a_proxy_or_a_persona() {
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
                    persona,
                    engine,
                    claimed_os,
                    timezone,
                    languages,
                    third_party,
                    idle_before,
                    json,
                },
        } = cli.command
        else {
            panic!("expected the browser self-test, got {:?}", cli.command);
        };
        assert!(idle_before.is_none());
        // The engine's own claim unless another is named.
        assert_eq!(claimed_os, None);
        assert_eq!(persona, None);
        assert_eq!(proxy.unwrap().to_string(), id);
        assert_eq!(engine, Engine::Chromium);
        assert_eq!(timezone.as_deref(), Some("Europe/Vienna"));
        assert_eq!(languages, ["de-AT", "de"]);
        assert!(!third_party);
        assert!(!json);

        let flags = [
            "--engine",
            "firefox",
            "--third-party",
            "--json",
            "--idle-before",
            "10m",
        ];
        let cli = Cli::try_parse_from(base.into_iter().chain(full).chain(flags)).unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Browser {
                    engine: Engine::Firefox,
                    third_party: true,
                    json: true,
                    idle_before: Some(idle),
                    ..
                },
        } = cli.command
        else {
            panic!("expected every flag to be read, got {:?}", cli.command);
        };
        assert_eq!(idle.get(), Duration::from_mins(10));

        // The stand-in may claim another system than its engine's own.
        let cli = Cli::try_parse_from(base.into_iter().chain([
            "--engine",
            "firefox",
            "--claimed-os",
            "macos",
        ]))
        .unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Browser {
                    claimed_os: Some(ClaimedOs::Macos),
                    ..
                },
        } = cli.command
        else {
            panic!("expected the claim to be read, got {:?}", cli.command);
        };

        // The stand-in takes its time zone and languages from the proxy's
        // geo record unless they are named.
        let cli = Cli::try_parse_from(base).unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Browser {
                    proxy: Some(_),
                    timezone: None,
                    languages,
                    ..
                },
        } = cli.command
        else {
            panic!("expected a proxy alone to do, got {:?}", cli.command);
        };
        assert_eq!(languages.len(), 0);

        for arguments in [
            vec!["kage", "selftest", "browser", "--timezone", "Europe/Vienna"],
            [&base[..], &full[..], &["--engine", "safari"]].concat(),
            // A persona brings its own proxy, engine, zone and languages.
            [&base[..], &full[..], &["--persona", id]].concat(),
            vec![
                "kage",
                "selftest",
                "browser",
                "--persona",
                id,
                "--engine",
                "firefox",
            ],
            vec![
                "kage",
                "selftest",
                "browser",
                "--persona",
                id,
                "--language",
                "de",
            ],
            vec!["kage", "selftest", "browser", "--persona", "seven"],
            // A system nobody claims, and a claim for a persona, which
            // has its own.
            [&base[..], &["--claimed-os", "android"]].concat(),
            vec![
                "kage",
                "selftest",
                "browser",
                "--persona",
                id,
                "--claimed-os",
                "macos",
            ],
        ] {
            assert!(
                Cli::try_parse_from(arguments.clone()).is_err(),
                "{arguments:?}"
            );
        }

        let cli = Cli::try_parse_from([
            "kage",
            "selftest",
            "browser",
            "--persona",
            id,
            "--third-party",
        ])
        .unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Browser {
                    proxy: None,
                    persona: Some(persona),
                    timezone: None,
                    third_party: true,
                    json: false,
                    languages,
                    ..
                },
        } = cli.command
        else {
            panic!("expected the self-test of a persona, got {:?}", cli.command);
        };
        assert_eq!(persona.to_string(), id);
        assert_eq!(languages.len(), 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
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
                    persona,
                    platform,
                    echo,
                    json,
                },
        } = cli.command
        else {
            panic!("expected the selftest command, got {:?}", cli.command);
        };
        assert_eq!(persona, None);
        assert_eq!(proxy.unwrap().to_string(), id);
        assert_eq!(profile.as_deref(), Some("chrome_149"));
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
            // A persona brings its own proxy and profile.
            vec!["kage", "selftest", "http", "--persona", id, "--proxy", id],
            vec![
                "kage",
                "selftest",
                "http",
                "--persona",
                id,
                "--profile",
                "chrome_149",
            ],
        ] {
            assert!(
                Cli::try_parse_from(arguments.clone()).is_err(),
                "{arguments:?}"
            );
        }

        let cli =
            Cli::try_parse_from(["kage", "selftest", "http", "--persona", id, "--json"]).unwrap();
        let Command::Selftest {
            command:
                SelftestCommand::Http {
                    proxy: None,
                    profile: None,
                    persona: Some(persona),
                    echo: None,
                    json: true,
                    ..
                },
        } = cli.command
        else {
            panic!("expected the self-test of a persona, got {:?}", cli.command);
        };
        assert_eq!(persona.to_string(), id);
    }

    fn persona(arguments: &[&str]) -> PersonaCommand {
        let cli = Cli::try_parse_from(["kage", "persona"].iter().chain(arguments)).unwrap();
        match cli.command {
            Command::Persona { command } => command,
            other => panic!("expected a persona command, got {other:?}"),
        }
    }

    #[test]
    fn persona_commands_take_an_id_and_list_shows_live_personas_by_default() {
        let id = "0199c2f0-0000-7000-8000-000000000001";
        assert!(matches!(
            persona(&["list"]),
            PersonaCommand::List {
                state: None,
                all: false,
                json: false
            }
        ));
        assert!(matches!(
            persona(&["list", "--state", "cooling", "--all", "--json"]),
            PersonaCommand::List {
                state: Some(PersonaState::Cooling),
                all: true,
                json: true
            }
        ));
        let PersonaCommand::Show {
            id: shown,
            fingerprint: true,
            json: true,
        } = persona(&["show", id, "--json", "--fingerprint"])
        else {
            panic!("expected show");
        };
        assert_eq!(shown.to_string(), id);
        assert!(matches!(
            persona(&["retire", id]),
            PersonaCommand::Retire { json: false, .. }
        ));
        assert!(matches!(
            persona(&["regenerate", id, "--json"]),
            PersonaCommand::Regenerate { json: true, .. }
        ));
        // The self-test asks nobody else unless told to.
        assert!(matches!(
            persona(&["selftest", id]),
            PersonaCommand::Selftest {
                third_party: false,
                json: false,
                ..
            }
        ));
        assert!(matches!(
            persona(&["selftest", id, "--third-party", "--json"]),
            PersonaCommand::Selftest {
                third_party: true,
                json: true,
                ..
            }
        ));

        for arguments in [
            vec!["kage", "persona"],
            vec!["kage", "persona", "show"],
            vec!["kage", "persona", "show", "seven"],
            vec!["kage", "persona", "retire"],
            vec!["kage", "persona", "selftest"],
            vec!["kage", "persona", "regenerate"],
            vec!["kage", "persona", "list", "--state", "asleep"],
            // The fingerprint is a document: it only fits JSON.
            vec!["kage", "persona", "show", id, "--fingerprint"],
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
