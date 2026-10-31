//! `kage persona`: inspect and operate the personas (spec §6, §10.2).
//!
//! The commands may run next to the server: every change goes through the
//! same row locks the server takes. `list` and `show` only read. `retire`,
//! `regenerate` and `selftest` change something and leave an audit event.
//!
//! `selftest` runs as the persona through its own proxy: the http path
//! when the persona has one, the browser path in the persona's own
//! context. By default it asks the operator's own echo and loads the
//! operator's own pages; `--third-party` uses the echo and the detection
//! pages run by others, which see the exit address of the persona's proxy.
//! Only this command and the gated live suite ever do that; the server
//! never does.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, bail};
use kage_browser::contract::FindingStatus;
use kage_core::alert::AlertSink;
use kage_core::config::Config;
use kage_core::domain::{
    AuditOutcome, CoolingReason, Persona, PersonaDomainState, PersonaState, Render, SelftestRun,
};
use kage_core::ids::PersonaId;
use kage_core::persona::mix::MixEntry;
use kage_http::{ClientFactory, CookieBackend, HttpSettings, StoreCookies};
use kage_persona::identity::http_spec;
use kage_persona::manager::REASON_OPERATOR;
use kage_persona::selftest::{BrowserRun, HttpRun, Verdict, pages};
use kage_persona::{Egress, PersonaError, PersonaManager, PersonaParts, Sidecars};
use kage_proxy::Pool;
use kage_scheduler::{Forwarder, ForwarderSettings, Routes, Tokens};
use kage_store::Store;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

use crate::proxy_cmd::{Session, table};

/// What `kage persona list` was asked to show.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ListOptions {
    /// Only personas in this state.
    pub(crate) state: Option<PersonaState>,
    /// Retired personas too.
    pub(crate) all: bool,
    /// One JSON document instead of a table.
    pub(crate) json: bool,
}

/// Which paths a self-test covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Paths {
    /// Every path the persona has.
    Both,
    /// The http path only.
    Http,
    /// The browser path only.
    Browser,
}

/// What a self-test of a persona was asked to do.
#[derive(Debug, Clone)]
pub(crate) struct SelftestOptions {
    /// The persona.
    pub(crate) id: PersonaId,
    /// Which paths to test.
    pub(crate) paths: Paths,
    /// Use the echo and the detection pages run by others.
    pub(crate) third_party: bool,
    /// Ask this echo instead of the configured one.
    pub(crate) echo: Option<String>,
    /// One JSON document instead of tables.
    pub(crate) json: bool,
}

/// The persona manager of one command, over the command's session.
pub(crate) fn manager(session: &Session, config: &Config) -> anyhow::Result<PersonaManager> {
    build(
        config,
        &session.store,
        Arc::clone(&session.pool),
        session.alerts.clone(),
    )
}

/// A persona manager over a store and a pool: the server's, or a
/// command's.
pub(crate) fn build(
    config: &Config,
    store: &Store,
    pool: Arc<Pool>,
    alerts: Arc<dyn AlertSink>,
) -> anyhow::Result<PersonaManager> {
    let echo_trust_anchor = config
        .selftest
        .http
        .echo_ca_file
        .as_ref()
        .map(|path| {
            std::fs::read(path).with_context(|| {
                format!("cannot read selftest.http.echo_ca_file {}", path.display())
            })
        })
        .transpose()?;
    let http = Arc::new(http_factory(
        config,
        Arc::new(StoreCookies::new(store.clone())),
    ));
    PersonaManager::new(PersonaParts {
        store: store.clone(),
        pool,
        sidecars: Sidecars::from_config(config).context("cannot build the sidecar clients")?,
        http,
        alerts,
        config: config.clone(),
        echo_trust_anchor,
    })
    .context("cannot set up the persona manager")
}

/// The client factory of the http path, the server's and a command's
/// alike. Under `security.resolve_on_host` it carries the host's resolver
/// (spec §19), so that every client it builds, the persona's and the plain
/// one of level 0, resolves each redirect hop here and refuses one whose
/// name points into a denied range, or has no address, before it is asked
/// for. Off by default: the proxy resolves, and no name a consumer asks for
/// is looked up from this host.
pub(crate) fn http_factory(config: &Config, cookies: Arc<dyn CookieBackend>) -> ClientFactory {
    let http = ClientFactory::new(HttpSettings::from_config(&config.http), cookies);
    if config.security.resolve_on_host {
        http.with_resolver(Arc::new(kage_api::resolve::HostResolver::system()))
    } else {
        http
    }
}

/// Runs `work` with a session and a manager, and closes the session
/// whatever it returned.
async fn with_manager<T, F>(config: &Config, work: F) -> anyhow::Result<T>
where
    F: AsyncFnOnce(&Session, &PersonaManager) -> anyhow::Result<T>,
{
    let session = Session::open(config).await?;
    let result = match manager(&session, config) {
        Ok(manager) => work(&session, &manager).await,
        Err(error) => Err(error),
    };
    session.close().await;
    result
}

/// A forwarder of a command's own, on a free port of the address the
/// server's forwarder listens on, which the sidecars reach this process at
/// too. A persona's browser and http client are handed it under the
/// persona's credential, as the server hands them its own: the provider's
/// credentials stay in this process (spec §7.5, security review G4).
struct OwnForwarder {
    forwarder: Arc<Forwarder>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl OwnForwarder {
    async fn start(config: &Config, session: &Session) -> anyhow::Result<Self> {
        let key = config
            .secrets
            .master_key_bytes()
            .context("secrets.master_key is not 32 bytes of hexadecimal")?;
        // The server's routes: under `egress.mode: direct` a host persona
        // leaves from the host's own connection, and nothing else does.
        let routes: Arc<dyn Routes> =
            Arc::new(crate::engine::routes(config, &session.store, &session.pool));
        let bind = SocketAddr::new(config.forwarder.bind.ip(), 0);
        let (forwarder, listener) = Forwarder::bind(
            bind,
            ForwarderSettings::from_config(config),
            Tokens::new(&key),
            routes,
        )
        .await
        .with_context(|| format!("cannot bind a forwarder to {bind}"))?;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(Arc::clone(&forwarder).serve(listener, async move {
            let _ = stopped.await;
        }));
        Ok(Self {
            forwarder,
            stop,
            task,
        })
    }

    fn egress(&self) -> Arc<dyn Egress> {
        Arc::new(self.forwarder.egress())
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "the command's forwarder ended badly");
        }
    }
}

/// [`with_manager`] for work that opens a persona's browser or sends its
/// http client out: the manager hands out a forwarder of the command's own.
async fn with_forwarded_manager<T, F>(config: &Config, work: F) -> anyhow::Result<T>
where
    F: AsyncFnOnce(&Session, &PersonaManager) -> anyhow::Result<T>,
{
    let session = Session::open(config).await?;
    let result = match OwnForwarder::start(config, &session).await {
        Ok(own) => {
            let result = match manager(&session, config) {
                Ok(manager) => work(&session, &manager.with_egress(own.egress())).await,
                Err(error) => Err(error),
            };
            own.stop().await;
            result
        }
        Err(error) => Err(error),
    };
    session.close().await;
    result
}

fn time(at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    at.map_or_else(
        || "-".to_owned(),
        |at| at.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// A persona as a JSON document: everything but its fingerprint, which
/// only the sidecar reads.
fn document(persona: &Persona) -> Value {
    json!({
        "id": persona.id,
        "proxy_id": persona.proxy_id,
        "state": persona.state.as_str(),
        "cooling_reason": persona.cooling_reason.map(CoolingReason::as_str),
        "engine": persona.engine.as_str(),
        "claimed_os": persona.claimed_os.as_str(),
        "browser_major": persona.browser_major,
        "user_agent": persona.user_agent,
        "wreq_profile": persona.wreq_profile,
        "country": persona.country.map(|country| country.to_string()),
        "locale": persona.locale,
        "languages": persona.languages,
        "timezone": persona.timezone,
        "geolocation": persona.geolocation,
        "screen": persona.screen,
        "hardware_concurrency": persona.hardware_concurrency,
        "seed": persona.seed.to_string(),
        "tables_version": persona.tables_version,
        "profile_dir": persona.profile_dir,
        "warmup_visits": persona.warmup_visits,
        "warmed_at": persona.warmed_at,
        "ready": persona.is_ready(),
        "created_at": persona.created_at,
        "retired_at": persona.retired_at,
        "retire_reason": persona.retire_reason,
        "profile_archived_at": persona.profile_archived_at,
        "profile_archive": persona.profile_archive,
        "profile_deleted_at": persona.profile_deleted_at,
    })
}

/// The live personas per entry of the mix, as `engine/os count (share)`.
fn distribution(mix: &[MixEntry], personas: &[Persona]) -> Vec<(String, usize)> {
    let live: Vec<&Persona> = personas
        .iter()
        .filter(|persona| persona.is_live())
        .collect();
    let mut counts: Vec<(String, usize)> = mix
        .iter()
        .map(|entry| {
            let count = live
                .iter()
                .filter(|persona| entry.is(persona.engine, persona.claimed_os))
                .count();
            (format!("{}/{}", entry.engine, entry.os), count)
        })
        .collect();
    let other = live
        .iter()
        .filter(|persona| {
            !mix.iter()
                .any(|entry| entry.is(persona.engine, persona.claimed_os))
        })
        .count();
    if other > 0 {
        counts.push(("outside the mix".to_owned(), other));
    }
    counts
}

/// `kage persona list`.
pub(crate) async fn list(
    config: &Config,
    options: ListOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let personas = session.store.personas().list(options.state).await;
    session.close().await;
    let mut personas = personas.context("cannot read the personas")?;
    if !options.all && options.state.is_none() {
        personas.retain(Persona::is_live);
    }
    let mix = distribution(&config.personas.mix, &personas);
    if options.json {
        let document = json!({
            "personas": personas.iter().map(document).collect::<Vec<_>>(),
            "distribution": mix.iter().map(|(entry, count)| json!({ "entry": entry, "count": count })).collect::<Vec<_>>(),
        });
        serde_json::to_writer_pretty(&mut *out, &document)?;
        writeln!(out)?;
        return Ok(());
    }
    let rows: Vec<Vec<String>> = personas
        .iter()
        .map(|persona| {
            let state = match persona.cooling_reason {
                Some(reason) => format!("{} ({reason})", persona.state),
                None if persona.is_live() && persona.warmed_at.is_none() => {
                    format!("{} (warming up)", persona.state)
                }
                None => persona.state.to_string(),
            };
            vec![
                persona.id.to_string(),
                state,
                format!("{} {}", persona.engine, persona.browser_major),
                persona.claimed_os.to_string(),
                persona
                    .country
                    .map_or_else(|| "-".to_owned(), |country| country.to_string()),
                persona.locale.clone(),
                persona.timezone.clone(),
                format!("{}x{}", persona.screen.width, persona.screen.height),
                persona
                    .wreq_profile
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
                persona.proxy_id.to_string(),
            ]
        })
        .collect();
    table(
        out,
        &[
            "ID", "STATE", "BROWSER", "OS", "COUNTRY", "LOCALE", "TIMEZONE", "SCREEN", "HTTP",
            "PROXY",
        ],
        &rows,
    )?;
    let live: usize = mix.iter().map(|(_, count)| count).sum();
    let shares: Vec<String> = mix
        .iter()
        .map(|(entry, count)| format!("{entry} {count}"))
        .collect();
    writeln!(out, "{live} live: {}", shares.join(", "))?;
    Ok(())
}

/// `kage persona show`.
pub(crate) async fn show(
    config: &Config,
    id: PersonaId,
    fingerprint: bool,
    json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let read = async {
        let store = &session.store;
        let Some(persona) = store.personas().get(id).await? else {
            bail!("there is no persona {id}");
        };
        Ok(Shown {
            runs: store.selftest_runs().recent(id, 10).await?,
            domains: store.persona_domain_state().list_for_persona(id).await?,
            sessions: store
                .sessions()
                .count_for_persona(id, chrono::Utc::now())
                .await?,
            persona,
        })
    }
    .await;
    session.close().await;
    let shown = read?;
    if json {
        serde_json::to_writer_pretty(&mut *out, &shown.document(fingerprint))?;
        writeln!(out)?;
        Ok(())
    } else {
        shown.write(out)
    }
}

/// Everything `kage persona show` shows of one persona.
struct Shown {
    persona: Persona,
    runs: Vec<SelftestRun>,
    domains: Vec<PersonaDomainState>,
    sessions: u64,
}

impl Shown {
    fn document(&self, fingerprint: bool) -> Value {
        let persona = &self.persona;
        let mut document = document(persona);
        let Some(object) = document.as_object_mut() else {
            return document;
        };
        if fingerprint {
            object.insert("fingerprint".to_owned(), persona.fingerprint.clone());
        }
        object.insert("open_sessions".to_owned(), json!(self.sessions));
        object.insert(
            "http_path_off".to_owned(),
            json!(http_spec(persona).err().map(|off| off.to_string())),
        );
        object.insert(
            "selftest_runs".to_owned(),
            self.runs
                .iter()
                .map(|run| {
                    json!({
                        "id": run.id,
                        "render": run.render.as_str(),
                        "started_at": run.started_at,
                        "finished_at": run.finished_at,
                        "passed": run.passed,
                        "verdict": run.findings.get("verdict"),
                        "score": run.score,
                    })
                })
                .collect(),
        );
        object.insert(
            "domains".to_owned(),
            self.domains
                .iter()
                .map(|state| {
                    json!({
                        "domain": state.domain,
                        "cooldown_until": state.cooldown_until,
                        "requests_today": state.requests_today,
                        "counter_day": state.counter_day,
                        "last_request_at": state.last_request_at,
                        "last_outcome": state.last_outcome,
                        "block_count_24h": state.block_count_24h,
                    })
                })
                .collect(),
        );
        document
    }

    fn identity(&self) -> Vec<(&'static str, String)> {
        let persona = &self.persona;
        let state = match persona.cooling_reason {
            Some(reason) => format!("{} ({reason})", persona.state),
            None => persona.state.to_string(),
        };
        let position = persona.geolocation.map_or_else(
            || "-".to_owned(),
            |position| format!("{:.4}, {:.4}", position.latitude, position.longitude),
        );
        let http = match http_spec(persona) {
            Ok(spec) => spec.profile.to_string(),
            Err(off) => format!("off: {off}"),
        };
        vec![
            ("persona", persona.id.to_string()),
            ("proxy", persona.proxy_id.to_string()),
            ("state", state),
            (
                "browser",
                format!(
                    "{} {} on {}",
                    persona.engine, persona.browser_major, persona.claimed_os
                ),
            ),
            ("user agent", persona.user_agent.clone()),
            ("http path", http),
            (
                "country",
                persona
                    .country
                    .map_or_else(|| "-".to_owned(), |country| country.to_string()),
            ),
            ("locale", persona.locale.clone()),
            ("languages", persona.languages.join(", ")),
            ("time zone", persona.timezone.clone()),
            ("position", position),
            (
                "screen",
                format!(
                    "{}x{}, {} processors",
                    persona.screen.width, persona.screen.height, persona.hardware_concurrency
                ),
            ),
            ("seed", persona.seed.to_string()),
            ("tables", persona.tables_version.clone()),
            ("profile", persona.profile_dir.clone()),
            ("created", time(Some(persona.created_at))),
            (
                "warm-up",
                format!(
                    "{} visits, ready {}",
                    persona.warmup_visits,
                    time(persona.warmed_at)
                ),
            ),
            ("open sessions", self.sessions.to_string()),
        ]
    }

    fn write(&self, out: &mut impl Write) -> anyhow::Result<()> {
        let persona = &self.persona;
        for (name, value) in self.identity() {
            writeln!(out, "{name:<14} {value}")?;
        }
        if let Some(retired_at) = persona.retired_at {
            writeln!(
                out,
                "{:<14} {} ({}); profile archived {}, deleted {}",
                "retired",
                time(Some(retired_at)),
                persona.retire_reason.as_deref().unwrap_or("-"),
                time(persona.profile_archived_at),
                time(persona.profile_deleted_at),
            )?;
        }
        if !self.runs.is_empty() {
            writeln!(out)?;
            let rows: Vec<Vec<String>> = self
                .runs
                .iter()
                .map(|run| {
                    vec![
                        time(Some(run.finished_at)),
                        run.render.to_string(),
                        run.findings
                            .get("verdict")
                            .and_then(Value::as_str)
                            .unwrap_or(if run.passed { "passed" } else { "failed" })
                            .to_owned(),
                        run.score
                            .map_or_else(|| "-".to_owned(), |score| format!("{score:.1}")),
                    ]
                })
                .collect();
            table(out, &["SELF-TEST", "PATH", "VERDICT", "SCORE"], &rows)?;
        }
        if !self.domains.is_empty() {
            writeln!(out)?;
            let rows: Vec<Vec<String>> = self
                .domains
                .iter()
                .map(|state| {
                    vec![
                        state.domain.clone(),
                        state.requests_today.to_string(),
                        state
                            .last_outcome
                            .map_or_else(|| "-".to_owned(), |outcome| outcome.as_str().to_owned()),
                        state.block_count_24h.to_string(),
                        time(state.cooldown_until),
                    ]
                })
                .collect();
            table(
                out,
                &["DOMAIN", "TODAY", "LAST", "BLOCKS 24H", "COOLDOWN UNTIL"],
                &rows,
            )?;
        }
        Ok(())
    }
}

fn written(
    out: &mut impl Write,
    json: bool,
    persona: &Persona,
    sentence: &str,
) -> anyhow::Result<()> {
    if json {
        serde_json::to_writer_pretty(&mut *out, &document(persona))?;
        writeln!(out)?;
    } else {
        writeln!(out, "{sentence}")?;
    }
    Ok(())
}

/// `kage persona retire`.
pub(crate) async fn retire(
    config: &Config,
    id: PersonaId,
    json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    with_manager(config, async |session, manager| {
        let result = manager.retire(id, REASON_OPERATOR).await;
        session
            .audit(
                "persona.retire",
                Some(id.to_string()),
                outcome_of(result.is_ok()),
                json!({ "error": result.as_ref().err().map(ToString::to_string) }),
            )
            .await;
        let persona = result?;
        written(
            out,
            json,
            &persona,
            &format!(
                "persona {id} is retired ({}); its sessions have ended and its http cookies \
                 are deleted. The server archives its profile",
                persona.retire_reason.as_deref().unwrap_or(REASON_OPERATOR)
            ),
        )
    })
    .await
}

/// `kage persona regenerate`.
pub(crate) async fn regenerate(
    config: &Config,
    id: PersonaId,
    json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    with_manager(config, async |session, manager| {
        let result = manager.regenerate(id).await;
        session
            .audit(
                "persona.regenerate",
                Some(id.to_string()),
                outcome_of(result.is_ok()),
                json!({
                    "new": result.as_ref().ok().map(|persona| persona.id),
                    "error": result.as_ref().err().map(ToString::to_string),
                }),
            )
            .await;
        let persona = result?;
        written(
            out,
            json,
            &persona,
            &format!(
                "persona {id} is retired; proxy {} now has persona {} ({} {} on {})",
                persona.proxy_id,
                persona.id,
                persona.engine,
                persona.browser_major,
                persona.claimed_os
            ),
        )
    })
    .await
}

const fn outcome_of(succeeded: bool) -> AuditOutcome {
    if succeeded {
        AuditOutcome::Success
    } else {
        AuditOutcome::Failure
    }
}

/// The echo a persona's http self-test asks: the one named, else with
/// `--third-party` the one run by others, else the operator's own.
fn echo_of(config: &Config, options: &SelftestOptions) -> anyhow::Result<Option<Url>> {
    let selftest = &config.selftest.http;
    let text = match (&options.echo, options.third_party, &selftest.echo_url) {
        (Some(named), _, _) => named.as_str(),
        (None, true, _) => selftest.third_party_url.as_str(),
        (None, false, Some(own)) => own.as_str(),
        (None, false, None) => return Ok(None),
    };
    let url: Url = text
        .parse()
        .with_context(|| format!("the echo {text:?} is not a URL"))?;
    if url.scheme() != "https" {
        bail!("the echo must be an https URL: a fingerprint echo reads the TLS handshake");
    }
    Ok(Some(url))
}

/// One path's part of a self-test: what ran, or why nothing did.
enum Part<Run> {
    Ran(Box<Run>),
    Skipped(String),
}

fn write_http(out: &mut impl Write, part: &Part<HttpRun>) -> anyhow::Result<()> {
    match part {
        Part::Skipped(why) => writeln!(out, "http: not run ({why})")?,
        Part::Ran(run) => {
            writeln!(out, "http: {}", run.verdict.as_str())?;
            match &run.report {
                Some(report) => {
                    let rows: Vec<Vec<String>> = report
                        .findings
                        .iter()
                        .map(|finding| {
                            vec![
                                finding.check.to_owned(),
                                if finding.passed { "ok" } else { "DIFFERS" }.to_owned(),
                                finding.observed.clone(),
                            ]
                        })
                        .collect();
                    table(out, &["CHECK", "RESULT", "OBSERVED"], &rows)?;
                }
                None => writeln!(
                    out,
                    "the echo could not be read: {}",
                    run.run.findings["error"].as_str().unwrap_or("-")
                )?,
            }
        }
    }
    Ok(())
}

fn write_browser(out: &mut impl Write, part: &Part<BrowserRun>) -> anyhow::Result<()> {
    match part {
        Part::Skipped(why) => writeln!(out, "browser: not run ({why})")?,
        Part::Ran(run) => {
            let report = &run.report;
            writeln!(
                out,
                "browser: {}, score {:.1} of 100, threshold {:.0}{}",
                run.verdict.as_str(),
                report.score,
                report.min_score,
                if report.complete {
                    ""
                } else {
                    "; incomplete: a page could not be read"
                }
            )?;
            let rows: Vec<Vec<String>> = report
                .findings
                .iter()
                .map(|finding| {
                    let verdict = match finding.status {
                        FindingStatus::Pass => "ok",
                        FindingStatus::Warn => "warn",
                        FindingStatus::Fail if finding.hard => "FAIL",
                        FindingStatus::Fail => "fail",
                        FindingStatus::Unknown => "unknown",
                    };
                    vec![
                        finding.page.clone(),
                        finding.check.clone(),
                        verdict.to_owned(),
                        finding.observed.clone().unwrap_or_else(|| "-".to_owned()),
                    ]
                })
                .collect();
            table(out, &["PAGE", "CHECK", "RESULT", "OBSERVED"], &rows)?;
            for line in crate::selftest_browser::page_notes(&report.pages) {
                writeln!(out, "{line}")?;
            }
        }
    }
    Ok(())
}

/// Why a path's self-test was not run, when that is no failure of the
/// command: the persona has no such path, or there is nothing to ask.
fn skipped(error: &PersonaError) -> Option<String> {
    match error {
        PersonaError::HttpPathOff { reason, .. } => Some(reason.to_string()),
        PersonaError::NothingToTest(why) => Some(why.clone()),
        PersonaError::Selftest(kage_http::selftest::SelftestError::NoExpectation { .. }) => {
            Some(error.to_string())
        }
        _ => None,
    }
}

/// What the self-tests of one command found.
struct Tested {
    http: Option<Part<HttpRun>>,
    browser: Option<Part<BrowserRun>>,
}

impl Tested {
    fn verdicts(&self) -> Vec<(Render, Verdict)> {
        let http = match &self.http {
            Some(Part::Ran(run)) => Some((Render::Http, run.verdict)),
            _ => None,
        };
        let browser = match &self.browser {
            Some(Part::Ran(run)) => Some((Render::Browser, run.verdict)),
            _ => None,
        };
        http.into_iter().chain(browser).collect()
    }

    fn document(&self, persona: &Persona, third_party: bool) -> Value {
        let verdicts = self.verdicts();
        let http = self.http.as_ref().map(|http| match http {
            Part::Ran(run) => json!({
                "verdict": run.verdict.as_str(),
                "run": run.run.id,
                "report": run.report,
                "findings": run.run.findings,
            }),
            Part::Skipped(why) => json!({ "skipped": why }),
        });
        let browser = self.browser.as_ref().map(|browser| match browser {
            Part::Ran(run) => json!({
                "verdict": run.verdict.as_str(),
                "run": run.run.id,
                "report": run.report,
            }),
            Part::Skipped(why) => json!({ "skipped": why }),
        });
        json!({
            "persona": document(persona),
            "third_party": third_party,
            "http": http,
            "browser": browser,
            "passed": !verdicts.is_empty()
                && verdicts.iter().all(|(_, verdict)| *verdict == Verdict::Passed),
        })
    }
}

/// Runs the self-tests a command asked for.
async fn run_selftests(
    config: &Config,
    manager: &PersonaManager,
    options: &SelftestOptions,
) -> anyhow::Result<Tested> {
    let http = if options.paths == Paths::Browser {
        None
    } else {
        Some(match echo_of(config, options)? {
            None => Part::Skipped(
                "selftest.http.echo_url is not set; name an echo with --echo or pass \
                 --third-party"
                    .to_owned(),
            ),
            Some(echo) => match manager.selftest_http(options.id, &echo).await {
                Ok(run) => Part::Ran(Box::new(run)),
                Err(error) => match skipped(&error) {
                    Some(why) => Part::Skipped(why),
                    None => return Err(error).context("the http self-test could not be run"),
                },
            },
        })
    };
    let browser = if options.paths == Paths::Http {
        None
    } else {
        let pages = pages(&config.selftest.browser, options.third_party);
        Some(match manager.selftest_browser(options.id, pages).await {
            Ok(run) => Part::Ran(Box::new(run)),
            Err(error) => match skipped(&error) {
                Some(why) => Part::Skipped(why),
                None => return Err(error).context("the browser self-test could not be run"),
            },
        })
    };
    Ok(Tested { http, browser })
}

/// `kage persona selftest`, `kage selftest http --persona` and `kage
/// selftest browser --persona`.
pub(crate) async fn selftest(
    config: &Config,
    options: &SelftestOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    with_forwarded_manager(config, async |session, manager| {
        let persona = manager.get(options.id).await?;
        let tested = run_selftests(config, manager, options).await?;
        let verdicts = tested.verdicts();
        let failed = verdicts
            .iter()
            .any(|(_, verdict)| *verdict == Verdict::Failed);
        session
            .audit(
                "persona.selftest",
                Some(options.id.to_string()),
                outcome_of(!failed && !verdicts.is_empty()),
                json!({
                    "third_party": options.third_party,
                    "verdicts": verdicts
                        .iter()
                        .map(|(render, verdict)| {
                            json!({ "render": render.as_str(), "verdict": verdict.as_str() })
                        })
                        .collect::<Vec<_>>(),
                }),
            )
            .await;
        let after = manager.get(options.id).await?;

        if options.json {
            let document = tested.document(&after, options.third_party);
            serde_json::to_writer_pretty(&mut *out, &document)?;
            writeln!(out)?;
        } else {
            writeln!(
                out,
                "persona {}: {} {} on {} through proxy {}",
                persona.id,
                persona.engine,
                persona.browser_major,
                persona.claimed_os,
                persona.proxy_id
            )?;
            if let Some(http) = &tested.http {
                write_http(out, http)?;
            }
            if let Some(browser) = &tested.browser {
                write_browser(out, browser)?;
            }
            let state = match after.cooling_reason {
                Some(reason) => format!("{} ({reason})", after.state),
                None => after.state.to_string(),
            };
            writeln!(out, "the persona is {state}")?;
        }
        if failed {
            bail!("the self-test of persona {} failed", options.id);
        }
        if verdicts.is_empty() {
            bail!("nothing was tested for persona {}", options.id);
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_core::config::Config;
    use kage_core::domain::{
        ClaimedOs, Engine, Persona, PersonaState, ProxyEndpoint, ProxyScheme, Screen,
    };
    use kage_core::ids::{PersonaId, ProxyId};
    use kage_core::persona::mix::default_mix;
    use kage_core::persona::{self};
    use kage_core::secret::Secret;
    use kage_http::{ClientSpec, MemoryCookies, ProfileChoice};

    use super::{Paths, SelftestOptions, distribution, echo_of, http_factory};

    fn config() -> Config {
        Config::load(
            None,
            [
                ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
                ("KAGE__SIDECAR__TOKEN", "token"),
                (
                    "KAGE__SECRETS__MASTER_KEY",
                    "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
                ),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )
        .expect("the configuration loads")
    }

    fn options(third_party: bool, echo: Option<&str>) -> SelftestOptions {
        SelftestOptions {
            id: PersonaId::new(),
            paths: Paths::Both,
            third_party,
            echo: echo.map(str::to_owned),
            json: false,
        }
    }

    #[test]
    fn the_hosts_resolver_reaches_both_clients_of_the_http_path_when_the_setting_is_on() {
        let proxy = ProxyEndpoint {
            id: ProxyId::new(),
            host: "127.0.0.1".to_owned(),
            port: 3128,
            username: Secret::new("user"),
            password: Secret::new("pass"),
            scheme: ProxyScheme::Http,
        };
        let spec = ClientSpec::new(ProfileChoice::named("chrome_149", ClaimedOs::Windows).unwrap());
        let mut config = config();

        // Off, the default: no name a consumer asks for is looked up here.
        let factory = http_factory(&config, Arc::new(MemoryCookies::new()));
        assert!(factory.resolver().is_none());
        assert!(
            factory
                .plain(&proxy, "kage/test")
                .unwrap()
                .resolver()
                .is_none()
        );
        assert!(
            factory
                .ephemeral(&spec, &proxy)
                .unwrap()
                .resolver()
                .is_none()
        );

        // On: the factory `kage serve` and the persona commands build from
        // hands the host's resolver to the plain client of level 0 and to
        // the persona's client alike (F98).
        config.security.resolve_on_host = true;
        let factory = http_factory(&config, Arc::new(MemoryCookies::new()));
        assert!(factory.resolver().is_some());
        assert!(
            factory
                .plain(&proxy, "kage/test")
                .unwrap()
                .resolver()
                .is_some()
        );
        assert!(
            factory
                .ephemeral(&spec, &proxy)
                .unwrap()
                .resolver()
                .is_some()
        );
    }

    #[test]
    fn a_third_party_echo_is_only_asked_when_asked_for() {
        let mut config = config();
        // Nothing of the operator's own is configured: nothing is asked.
        assert_eq!(echo_of(&config, &options(false, None)).unwrap(), None);
        // The echo run by others, on request only.
        assert_eq!(
            echo_of(&config, &options(true, None))
                .unwrap()
                .unwrap()
                .as_str(),
            config.selftest.http.third_party_url
        );
        config.selftest.http.echo_url = Some("https://echo.kage.test/".to_owned());
        assert_eq!(
            echo_of(&config, &options(false, None))
                .unwrap()
                .unwrap()
                .as_str(),
            "https://echo.kage.test/"
        );
        // A named echo outranks both.
        assert_eq!(
            echo_of(&config, &options(true, Some("https://other.kage.test/")))
                .unwrap()
                .unwrap()
                .as_str(),
            "https://other.kage.test/"
        );
        for refused in ["http://echo.kage.test/", "not a url"] {
            assert!(echo_of(&config, &options(false, Some(refused))).is_err());
        }
    }

    #[test]
    fn the_distribution_counts_live_personas_per_entry_of_the_mix() {
        let persona = |engine: Engine, claimed_os: ClaimedOs, live: bool| {
            let id = PersonaId::new();
            Persona {
                id,
                proxy_id: ProxyId::new(),
                engine,
                claimed_os,
                browser_major: 155,
                seed: persona::seed_for(id),
                fingerprint: serde_json::json!({}),
                user_agent: "Mozilla/5.0".to_owned(),
                hardware_concurrency: 8,
                wreq_profile: None,
                country: None,
                locale: "en-US".to_owned(),
                languages: vec!["en-US".to_owned(), "en".to_owned()],
                timezone: "Etc/UTC".to_owned(),
                geolocation: None,
                screen: Screen {
                    width: 1920,
                    height: 1080,
                    dpr: 1.0,
                    color_depth: 24,
                },
                tables_version: persona::TABLES_VERSION.to_owned(),
                profile_dir: persona::profile_dir(engine, id),
                state: if live {
                    PersonaState::Active
                } else {
                    PersonaState::Retired
                },
                cooling_reason: None,
                warmup_visits: 0,
                warmed_at: None,
                created_at: chrono::Utc::now(),
                retired_at: None,
                retire_reason: None,
                profile_archived_at: None,
                profile_archive: None,
                profile_deleted_at: None,
            }
        };
        let personas = [
            persona(Engine::Chromium, ClaimedOs::Linux, true),
            persona(Engine::Chromium, ClaimedOs::Linux, true),
            persona(Engine::Firefox, ClaimedOs::Windows, true),
            persona(Engine::Firefox, ClaimedOs::Windows, false),
            persona(Engine::Firefox, ClaimedOs::Linux, true),
        ];
        assert_eq!(
            distribution(&default_mix(), &personas),
            [
                ("chromium/linux".to_owned(), 2),
                ("firefox/windows".to_owned(), 1),
                ("firefox/macos".to_owned(), 0),
                ("outside the mix".to_owned(), 1),
            ]
        );
    }
}
