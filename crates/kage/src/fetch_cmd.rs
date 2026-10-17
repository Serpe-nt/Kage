//! `kage fetch <url>`: one request through the whole pipeline, in this
//! process: the policy, the scheduler, a persona, its path, the
//! classifier, and the store.
//!
//! The command binds a forwarder of its own on a free port, so it can run
//! next to a server. It shares the store with the server (cooldowns, daily
//! caps, breakers and the bandwidth budget hold for it) but not the
//! server's memory: a persona's pace is this process's own, and a persona
//! whose browser the server holds open cannot be opened here
//! (`context_conflict`); another persona then takes the request.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use kage_core::alert::AlertSink;
use kage_core::cache::CacheKey;
use kage_core::config::Config;
use kage_core::domain::{
    ArtifactKind, ArtifactRef, AuditOutcome, Engine as BrowserEngine, RenderMode,
};
use kage_core::duration::HumanDuration;
use kage_core::ids::PersonaId;
use kage_core::policy::{PacingRequest, PolicySet, PolicyStore};
use kage_core::schedule::queue::Priority;
use kage_extract::Options as ExtractOptions;
use kage_scheduler::{CachePlan, Conflict, FetchOrder, FetchReport, Keep, RobotsWish, robots};
use kage_store::ArtifactStore;
use serde_json::{Value, json};
use url::Url;

use crate::engine::Engine;
use crate::proxy_cmd::Session;

/// What `kage fetch` was asked to do.
#[derive(Debug, Clone)]
pub(crate) struct FetchOptions {
    /// The page.
    pub(crate) url: Url,
    /// The path: `auto`, `http` or `browser`.
    pub(crate) render: RenderMode,
    /// Only this persona.
    pub(crate) persona: Option<PersonaId>,
    /// Only personas on this engine.
    pub(crate) engine: Option<BrowserEngine>,
    /// Longest one attempt may take.
    pub(crate) timeout: Option<Duration>,
    /// Retries after the first attempt, instead of the policy's.
    pub(crate) retries: Option<u32>,
    /// A pacing profile, instead of the policy's.
    pub(crate) pacing: Option<String>,
    /// Where the document is written.
    pub(crate) output: Option<PathBuf>,
    /// One JSON document instead of sentences.
    pub(crate) json: bool,
    /// Whether what the page says is read from its document (spec §14).
    pub(crate) extract: bool,
    /// For how long the result may be answered from the response cache;
    /// the policy of the site decides when `None` (spec §15).
    pub(crate) cache_ttl: Option<Duration>,
    /// What the command says about the site's `robots.txt` (spec §14).
    pub(crate) robots: RobotsWish,
}

/// Bytes the forwarder of this command relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Relayed {
    up: u64,
    down: u64,
}

/// What a report says besides what the pipeline reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shown {
    /// Everything this command's forwarder relayed for the request.
    relayed: Relayed,
    /// The size of the document: of the attempt, or of the stored one a
    /// cached answer points at.
    document_bytes: usize,
    /// For how long the result may be answered from the cache, when the
    /// cache has a part in the fetch.
    cache_ttl: Option<Duration>,
    /// Whether the command itself said something about `robots.txt`.
    robots_asked: bool,
}

/// `--robots`: one of the four words a request of the API may say.
///
/// # Errors
///
/// A sentence that names the four words.
pub(crate) fn parse_robots(word: &str) -> Result<RobotsWish, String> {
    match word {
        "policy" => Ok(RobotsWish::Policy),
        "report" => Ok(RobotsWish::Report),
        "enforce" => Ok(RobotsWish::Enforce),
        "ignore" => Ok(RobotsWish::Ignore),
        other => Err(format!(
            "{other:?} is not one of policy, report, enforce or ignore"
        )),
    }
}

const fn robots_word(wish: RobotsWish) -> &'static str {
    match wish {
        RobotsWish::Policy => "policy",
        RobotsWish::Report => "report",
        RobotsWish::Enforce => "enforce",
        RobotsWish::Ignore => "ignore",
    }
}

/// Runs `kage fetch`, writing its result to `out`.
pub(crate) async fn fetch(
    config: &Config,
    options: &FetchOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = Box::pin(run(config, &session, options, out)).await;
    session.close().await;
    result
}

async fn run(
    config: &Config,
    session: &Session,
    options: &FetchOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let alerts: Arc<dyn AlertSink> = session.alerts.clone();
    let policies = Arc::new(
        PolicyStore::open(&config.policies.dir, Arc::clone(&alerts))
            .context("policies are invalid")?,
    );
    // Settled against the site's policy before anything starts: a wish
    // the policy does not allow is refused, never quietly dropped.
    let order = order(options, config, &policies.current())?;
    let cache_ttl = order.cache.as_ref().map(|cache| cache.ttl);
    // A free port on the address the server's forwarder listens on: the
    // sidecars reach this process the same way.
    let bind = SocketAddr::new(config.forwarder.bind.ip(), 0);
    let engine = Engine::start(
        config,
        &session.store,
        Arc::clone(&session.pool),
        alerts,
        policies,
        bind,
    )
    .await?;

    let fetched = async {
        // The budget of the proxy plan holds for an operator's fetch too.
        engine.check_budget().await?;
        engine
            .pipeline
            .fetch(order)
            .await
            .context("the request could not be accepted")
    }
    .await;
    let (up, down) = engine.forwarder.meter().totals();
    engine.stop().await;
    let report = fetched?;

    session
        .audit(
            "fetch",
            Some(options.url.to_string()),
            if report.outcome.is_success() {
                AuditOutcome::Success
            } else {
                AuditOutcome::Failure
            },
            json!({
                "request_id": report.request,
                "outcome": report.outcome.as_str(),
                "attempts": report.attempts.len(),
            }),
        )
        .await;

    // An answer from the cache made no attempt: its document is the one
    // the fetch that filled the cache stored.
    let document = match stored_document(&report) {
        Some(stored) if report.cached.is_some() => {
            ArtifactStore::new(session.store.pool().clone(), &config.artifacts)
                .read(stored.id)
                .await
                .context("cannot read the stored document of a cached answer")?
                .map(|(_, bytes)| bytes)
                .unwrap_or_default()
        }
        _ => document_of(&report),
    };
    if let Some(path) = &options.output {
        std::fs::write(path, &document)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    let shown = Shown {
        relayed: Relayed { up, down },
        document_bytes: document.len(),
        cache_ttl,
        robots_asked: options.robots != RobotsWish::Policy,
    };
    if options.json {
        serde_json::to_writer_pretty(&mut *out, &json_report(&report, &shown))?;
        writeln!(out)?;
    } else {
        write_report(out, &report, &shown)?;
    }
    if !report.outcome.is_success() {
        bail!(
            "{} was not fetched: {}",
            options.url,
            report
                .reason
                .as_deref()
                .unwrap_or_else(|| report.outcome.as_str())
        );
    }
    Ok(())
}

/// The order of the command: what was asked, with what it says about
/// `robots.txt`, the response cache and extraction settled against the
/// policy of the page's site, as a request of the API is (spec §14, §15).
///
/// # Errors
///
/// When the command asks for less care about `robots.txt` than the policy
/// takes, for a file the policy never has asked for, or for a longer life
/// in the cache than results of the site may have.
fn order(
    options: &FetchOptions,
    config: &Config,
    policies: &PolicySet,
) -> anyhow::Result<FetchOrder> {
    let host = options
        .url
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let policy = policies.domains.resolve(&host);

    if let Err(conflict) = robots::mode(policy.robots, options.robots) {
        match conflict {
            Conflict::Enforced => bail!(
                "the policy for {host} enforces its robots.txt, and --robots cannot ask for \
                 less ({}): leave it out, or say enforce",
                conflict.as_str()
            ),
            Conflict::Ignored => bail!(
                "the policy for {host} never asks for its robots.txt, so nothing could be \
                 enforced ({}): set robots for the site in the domain policies",
                conflict.as_str()
            ),
        }
    }

    let extract = options.extract.then(|| ExtractOptions {
        markdown: true,
        links: true,
        thin_words: usize::try_from(config.extract.thin_words).unwrap_or(usize::MAX),
    });

    let ttl = policy
        .cache
        .ttl_for(options.cache_ttl, config.cache.max_ttl.get())
        .map_err(|longest| {
            anyhow::anyhow!(
                "--cache-ttl is at most {} for {host}: results are not served for longer",
                HumanDuration::new(longest)
            )
        })?;
    // An answer from the cache can only return what was stored, so a fetch
    // that may fill it keeps the document: the DOM of a browser attempt,
    // the body of an http one.
    let keep = Keep {
        dom: ttl.is_some() && options.render != RenderMode::Http,
        raw: ttl.is_some() && options.render != RenderMode::Browser,
        ..Keep::default()
    };
    let cache = ttl.map(|ttl| {
        let mut include = vec!["document"];
        if options.extract {
            include.push("extract");
        }
        // The command line's entries are its own: no API key reads them,
        // and it reads no key's.
        let shaping = json!({
            "source": "cli",
            "persona": options.persona,
            "engine": options.engine.map(BrowserEngine::as_str),
        });
        let digest = CacheKey {
            url: &options.url,
            method: "GET",
            body: None,
            request_kind: "navigation",
            headers: &[],
            render: options.render,
            include: &include,
            options: &shaping,
            scope: None,
        }
        .digest(&config.urls);
        CachePlan {
            key: digest.key,
            url_key: digest.url_key,
            scope: None,
            ttl,
            read: true,
        }
    });

    Ok(FetchOrder {
        render: options.render,
        pin: options.persona,
        engines: options.engine.into_iter().collect(),
        timeout: options.timeout,
        max_retries: options.retries,
        pacing: options.pacing.clone().map(PacingRequest::Profile),
        keep,
        robots: options.robots,
        extract,
        cache,
        spec: json!({
            "url": options.url.as_str(),
            "render": options.render.as_str(),
            "extract": options.extract,
            "cache_ttl_s": options.cache_ttl.map(|ttl| ttl.as_secs()),
            "robots": robots_word(options.robots),
            "source": "cli",
        }),
        ..FetchOrder::get(options.url.clone(), Priority::Sync)
    })
}

/// The stored document of a result: the DOM a browser had, else the body
/// a server sent.
fn stored_document(report: &FetchReport) -> Option<ArtifactRef> {
    [ArtifactKind::DomHtml, ArtifactKind::RawBody]
        .into_iter()
        .find_map(|kind| {
            report
                .artifacts
                .iter()
                .find(|artifact| artifact.kind == kind)
                .copied()
        })
}

/// Whether the cache keeps the result: a page that was fetched and says
/// more than next to nothing (spec §15).
fn cache_keeps(report: &FetchReport) -> Result<(), &'static str> {
    let status = report.last.as_ref().and_then(|last| last.status);
    if !report.outcome.is_success() || !status.is_some_and(|status| (200..300).contains(&status)) {
        return Err("the page was not fetched");
    }
    let thin = report
        .extract
        .as_ref()
        .and_then(|extract| extract.get("thin_content"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if thin {
        return Err("the page says next to nothing");
    }
    Ok(())
}

/// The document a fetch returned: the DOM of a browser attempt, the body
/// of an http one.
fn document_of(report: &FetchReport) -> Vec<u8> {
    let Some(last) = &report.last else {
        return Vec::new();
    };
    if let Some(page) = &last.page {
        return page
            .dom_html
            .clone()
            .or_else(|| page.raw_body.as_ref().map(|body| body.data.clone()))
            .unwrap_or_default()
            .into_bytes();
    }
    last.fetched
        .as_ref()
        .map(|fetched| fetched.body.to_vec())
        .unwrap_or_default()
}

/// The status and the final address of a report: the last attempt's, or
/// the stored ones of an answer from the cache.
fn arrived(report: &FetchReport) -> (Option<u16>, Option<&str>) {
    match (&report.last, &report.cached) {
        (Some(last), _) => (last.status, last.final_url.as_deref()),
        (None, Some(stored)) => (stored.status, stored.final_url.as_deref()),
        (None, None) => (None, None),
    }
}

fn json_report(report: &FetchReport, shown: &Shown) -> Value {
    let last = report.last.as_ref();
    let page = last.and_then(|last| last.page.as_ref());
    let (status, final_url) = arrived(report);
    let relayed = shown.relayed;
    json!({
        "request_id": report.request,
        "result_id": report.result,
        "outcome": report.outcome.as_str(),
        "reason": report.reason,
        "retry_at": report.retry_at,
        "status": status,
        "final_url": final_url,
        "escalated": report.escalated,
        "persona": report.persona,
        "block": report.block,
        "attempts": report.attempts.iter().map(|attempt| json!({
            "n": attempt.n,
            "persona_id": attempt.persona_id,
            "engine": attempt.engine.as_str(),
            "render": attempt.render.as_str(),
            "outcome": attempt.outcome.as_str(),
            "status": attempt.status,
            "reason": attempt.reason,
            "block": attempt.block,
            "seed": attempt.seed.map(|seed| seed.to_string()),
            "started_at": attempt.started_at,
            "finished_at": attempt.finished_at,
            "total_ms": attempt.timing.total_ms,
        })).collect::<Vec<_>>(),
        "pacing": {
            "applied": report.pacing.applied,
            "request_ignored": report.pacing.request_ignored,
            "waited_ms": u64::try_from(report.pacing.waited.as_millis()).unwrap_or(u64::MAX),
        },
        // What the browser did on the page, as the sidecar reports it.
        "behavior": page.and_then(|page| serde_json::to_value(&page.behavior).ok()),
        "challenge": page.and_then(|page| serde_json::to_value(&page.challenge).ok()),
        "handoff": {
            "cookies_to_browser": last.and_then(|last| last.cookies_to_browser),
            "cookies_to_http": last.and_then(|last| last.cookies_to_http),
        },
        "document_bytes": shown.document_bytes,
        "title": page.and_then(|page| page.title.clone()),
        // What the page says, when --extract asked (spec §14).
        "extract": report.extract,
        // What the site's robots.txt says of the address; `fetched: false`
        // where nobody asks for the file.
        "robots": report.robots,
        // Whether the answer came from the response cache, when its page
        // was fetched, and for how long the result may be served from it.
        "cached": report.cached.is_some(),
        "cached_at": report.cached.as_ref().and_then(|stored| stored.cached_at),
        "cache_ttl_s": shown.cache_ttl.map(|ttl| ttl.as_secs()),
        // Everything this command's forwarder relayed for the request.
        "relayed_bytes": { "up": relayed.up, "down": relayed.down },
    })
}

/// What the site's `robots.txt` says of the address, in a sentence.
fn robots_sentence(verdict: &kage_core::domain::RobotsVerdict) -> String {
    if !verdict.fetched {
        return "robots.txt: not asked for".to_owned();
    }
    let word = if verdict.allowed {
        "allowed"
    } else {
        "disallowed"
    };
    let rule = verdict
        .matched_rule
        .as_ref()
        .map(|rule| format!(" by \"{rule}\""))
        .unwrap_or_default();
    let delay = verdict
        .crawl_delay
        .map(|delay| format!("; crawl-delay {delay} s"))
        .unwrap_or_default();
    format!("robots.txt: {word}{rule}{delay}")
}

/// What was extracted, in a sentence: the title, the words of the main
/// content, the links a reader sees.
fn extract_sentence(extract: &Value) -> String {
    let title = extract
        .get("title")
        .and_then(Value::as_str)
        .map_or_else(|| "no title".to_owned(), |title| format!("\"{title}\""));
    let words = extract
        .get("word_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let links = extract
        .get("links")
        .and_then(Value::as_array)
        .map(|links| format!(", {} links", links.len()))
        .unwrap_or_default();
    let thin = if extract.get("thin_content").and_then(Value::as_bool) == Some(true) {
        ", thin"
    } else {
        ""
    };
    format!("extract: {title}, {words} words{links}{thin}")
}

fn kilobytes(bytes: u64) -> String {
    // A page is far below 2^53 bytes.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    format!("{:.1} kB", bytes / 1000.0)
}

fn write_report(out: &mut impl Write, report: &FetchReport, shown: &Shown) -> anyhow::Result<()> {
    let last = report.last.as_ref();
    let relayed = shown.relayed;
    let (status, final_url) = arrived(report);
    let status = status.map_or_else(|| "-".to_owned(), |status| status.to_string());
    let final_url = final_url.unwrap_or("-");
    writeln!(out, "{}  {status}  {final_url}", report.outcome)?;
    if let Some(reason) = &report.reason {
        match report.retry_at {
            Some(at) => writeln!(
                out,
                "reason: {reason}; worth asking again at {}",
                at.format("%Y-%m-%d %H:%M:%S UTC")
            )?,
            None => writeln!(out, "reason: {reason}")?,
        }
    }
    if let Some(block) = &report.block {
        writeln!(
            out,
            "block: {} {} (confidence {:.2})",
            block.vendor, block.block_type, block.confidence
        )?;
    }
    writeln!(out, "request {}  result {}", report.request, report.result)?;
    if !report.attempts.is_empty() {
        writeln!(out, "attempts:")?;
    }
    for attempt in &report.attempts {
        writeln!(
            out,
            "  {}  {:<7}  {:<8}  persona {}  {}  {}  {} ms",
            attempt.n,
            attempt.render.as_str(),
            attempt.engine.as_str(),
            attempt.persona_id,
            attempt.outcome,
            attempt
                .status
                .map_or_else(|| "-".to_owned(), |status| status.to_string()),
            attempt.timing.total_ms,
        )?;
    }
    if report.escalated {
        writeln!(out, "the request moved from the http path to the browser")?;
    }
    writeln!(
        out,
        "pacing: {}, waited {:.1} s",
        report.pacing.applied,
        report.pacing.waited.as_secs_f64()
    )?;
    if let Some(last) = last
        && (last.cookies_to_browser.is_some() || last.cookies_to_http.is_some())
    {
        writeln!(
            out,
            "cookies handed over: {} to the browser, {} to the http path",
            last.cookies_to_browser.unwrap_or(0),
            last.cookies_to_http.unwrap_or(0)
        )?;
    }
    // The file is mentioned when it was asked for, by the policy or by the
    // command; where nobody asks, every result says the same nothing.
    if let Some(verdict) = &report.robots
        && (verdict.fetched || shown.robots_asked)
    {
        writeln!(out, "{}", robots_sentence(verdict))?;
    }
    if let Some(stored) = &report.cached {
        match stored.cached_at {
            Some(at) => writeln!(
                out,
                "cache: answered from the cache, fetched {}",
                at.format("%Y-%m-%d %H:%M:%S UTC")
            )?,
            None => writeln!(out, "cache: answered from the cache")?,
        }
    } else if let Some(ttl) = shown.cache_ttl {
        match cache_keeps(report) {
            Ok(()) => writeln!(
                out,
                "cache: not in the cache; fetched, and kept for {}",
                HumanDuration::new(ttl)
            )?,
            Err(why) if report.outcome.is_success() => {
                writeln!(out, "cache: not in the cache; fetched, and not kept: {why}")?;
            }
            Err(why) => writeln!(out, "cache: not in the cache; not kept: {why}")?,
        }
    }
    if let Some(extract) = &report.extract {
        writeln!(out, "{}", extract_sentence(extract))?;
    }
    writeln!(
        out,
        "document: {}; relayed: {} up, {} down",
        kilobytes(shown.document_bytes as u64),
        kilobytes(relayed.up),
        kilobytes(relayed.down)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use kage_core::config::Config;
    use kage_core::domain::{ArtifactKind, ArtifactRef, FetchRecord, RobotsVerdict};
    use kage_core::domain::{Attempt, Engine, PersonaSummary, Render, RenderMode, Timing};
    use kage_core::ids::ArtifactId;
    use kage_core::ids::{AttemptId, PersonaId, RequestId, ResultId};
    use kage_core::outcome::{Block, BlockType, Outcome, Vendor};
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::{DomainPolicies, PacingRequest, PolicySet};
    use kage_core::schedule::politeness::Courtesy;
    use kage_core::schedule::queue::Priority;
    use kage_extract::Options as ExtractOptions;
    use kage_scheduler::{AttemptOutput, FetchReport, PacingReport, RobotsWish};
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        FetchOptions, Relayed, Shown, document_of, json_report, kilobytes, order, parse_robots,
        stored_document, write_report,
    };

    const PACING: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
fast:
  gap_ms: { dist: uniform, min: 300, max: 1200 }
  burst_pages: { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
";

    const SIGNATURES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/block-signatures.yaml"
    ));

    const DOMAINS: &str = "
domains:
  enforced.example: { robots: enforce }
  reported.example: { robots: report }
  short.example: { cache: { max_ttl: 5m } }
  kept.example: { cache: { ttl: 2m } }
";

    fn config() -> Config {
        let env = [
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        Config::load(
            None,
            env.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        )
        .unwrap()
    }

    fn policies() -> PolicySet {
        PolicySet {
            domains: DomainPolicies::from_yaml(DOMAINS).unwrap(),
            pacing: PacingProfiles::from_yaml(PACING).unwrap(),
            signatures: kage_core::classifier::SignatureSet::from_yaml(SIGNATURES).unwrap(),
        }
    }

    fn at(url: &str) -> FetchOptions {
        FetchOptions {
            url: url.parse().unwrap(),
            ..options()
        }
    }

    fn shown() -> Shown {
        Shown {
            relayed: RELAYED,
            document_bytes: 0,
            cache_ttl: None,
            robots_asked: false,
        }
    }

    /// The report in sentences.
    fn said(report: &FetchReport, shown: &Shown) -> String {
        let mut out = Vec::new();
        write_report(&mut out, report, shown).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn options() -> FetchOptions {
        FetchOptions {
            url: "https://shop.example/page".parse().unwrap(),
            render: RenderMode::Auto,
            persona: None,
            engine: None,
            timeout: None,
            retries: None,
            pacing: None,
            output: None,
            json: false,
            extract: false,
            cache_ttl: None,
            robots: RobotsWish::Policy,
        }
    }

    fn report(outcome: Outcome) -> FetchReport {
        let request = RequestId::from_uuid(Uuid::from_u128(1));
        let persona = PersonaId::from_uuid(Uuid::from_u128(2));
        let at = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        let blocked = outcome.is_block();
        let block = blocked.then(|| Block {
            vendor: Vendor::new("cloudflare").unwrap(),
            block_type: BlockType::new("waf_block").unwrap(),
            confidence: 0.95,
            evidence: Vec::new(),
        });
        let mut last = AttemptOutput::failed(outcome, "test", Courtesy::Answered);
        last.reason = None;
        last.status = Some(if blocked { 403 } else { 200 });
        last.final_url = Some("https://shop.example/page".to_owned());
        last.cookies_to_browser = Some(1);
        last.cookies_to_http = Some(2);
        FetchReport {
            request,
            result: ResultId::from_uuid(Uuid::from_u128(3)),
            outcome,
            reason: None,
            retry_at: None,
            attempts: vec![Attempt {
                id: AttemptId::from_uuid(Uuid::from_u128(4)),
                request_id: request,
                n: 1,
                persona_id: persona,
                engine: Engine::Chromium,
                render: Render::Browser,
                started_at: at,
                finished_at: at,
                status: last.status,
                final_url: last.final_url.clone(),
                outcome,
                block: block.clone(),
                timing: Timing {
                    total_ms: 3400,
                    ..Timing::default()
                },
                seed: None,
                reason: None,
                patience_ms: None,
            }],
            last: Some(last),
            headers: Vec::new(),
            artifacts: Vec::new(),
            xhr: None,
            finished_at: at,
            persona: Some(PersonaSummary {
                id: persona,
                engine: Engine::Chromium,
                claimed_os: kage_core::domain::ClaimedOs::Linux,
                country: "DE".parse().ok(),
            }),
            block,
            pacing: PacingReport {
                applied: "human".to_owned(),
                request_ignored: false,
                waited: Duration::from_millis(700),
            },
            escalated: true,
            extract: None,
            robots: None,
            page: None,
            reading: None,
            cached: None,
            visit: kage_core::domain::Visit::default(),
        }
    }

    const RELAYED: Relayed = Relayed {
        up: 1_200,
        down: 480_100,
    };

    #[test]
    fn the_order_is_what_was_asked_and_the_policys_otherwise() {
        let (config, policies) = (config(), policies());
        let plain = order(&options(), &config, &policies).unwrap();
        assert_eq!(plain.url.as_str(), "https://shop.example/page");
        assert_eq!(plain.render, RenderMode::Auto);
        assert_eq!(plain.priority, Priority::Sync);
        assert_eq!(
            (plain.pin, plain.timeout, plain.max_retries),
            (None, None, None)
        );
        assert!(plain.engines.is_empty() && plain.pacing.is_none());
        assert_eq!(plain.spec["source"], "cli");

        let persona = PersonaId::from_uuid(Uuid::from_u128(9));
        let asked = order(
            &FetchOptions {
                render: RenderMode::Browser,
                persona: Some(persona),
                engine: Some(Engine::Firefox),
                timeout: Some(Duration::from_secs(40)),
                retries: Some(0),
                pacing: Some("fast".to_owned()),
                ..options()
            },
            &config,
            &policies,
        )
        .unwrap();
        assert_eq!(asked.render, RenderMode::Browser);
        assert_eq!(asked.pin, Some(persona));
        assert_eq!(asked.engines, [Engine::Firefox]);
        assert_eq!(asked.timeout, Some(Duration::from_secs(40)));
        assert_eq!(asked.max_retries, Some(0));
        assert_eq!(
            asked.pacing,
            Some(PacingRequest::Profile("fast".to_owned()))
        );
        assert_eq!(asked.spec["render"], "browser");
    }

    #[test]
    fn a_fetched_page_is_reported_in_sentences() {
        let mut out = Vec::new();
        write_report(&mut out, &report(Outcome::Ok), &shown()).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(
            text.starts_with("ok  200  https://shop.example/page\n"),
            "{text}"
        );
        assert!(
            text.contains("attempts:\n  1  browser  chromium  persona "),
            "{text}"
        );
        assert!(text.contains("  ok  200  3400 ms\n"), "{text}");
        assert!(text.contains("the request moved from the http path to the browser\n"));
        assert!(text.contains("pacing: human, waited 0.7 s\n"), "{text}");
        assert!(text.contains("cookies handed over: 1 to the browser, 2 to the http path\n"));
        assert!(
            text.ends_with("document: 0.0 kB; relayed: 1.2 kB up, 480.1 kB down\n"),
            "{text}"
        );
        assert!(!text.contains("block:") && !text.contains("reason:"));
    }

    #[test]
    fn a_block_and_a_refusal_say_what_happened() {
        let mut out = Vec::new();
        write_report(&mut out, &report(Outcome::Forbidden), &shown()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("forbidden  403  "), "{text}");
        assert!(
            text.contains("block: cloudflare waf_block (confidence 0.95)\n"),
            "{text}"
        );

        let mut refused = report(Outcome::ProviderError);
        refused.attempts.clear();
        refused.last = None;
        refused.escalated = false;
        refused.reason = Some("domain_paused".to_owned());
        refused.retry_at = Some(Utc.with_ymd_and_hms(2026, 10, 10, 12, 30, 0).unwrap());
        let mut out = Vec::new();
        write_report(&mut out, &refused, &shown()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("provider_error  -  -\n"), "{text}");
        assert!(
            text.contains("reason: domain_paused; worth asking again at 2026-10-10 12:30:00 UTC\n"),
            "{text}"
        );
        assert!(!text.contains("attempts:"), "{text}");

        refused.retry_at = None;
        let mut out = Vec::new();
        write_report(&mut out, &refused, &shown()).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("reason: domain_paused\n")
        );
    }

    #[test]
    fn the_json_report_carries_what_an_operator_and_a_test_look_for() {
        let document = json_report(&report(Outcome::Ok), &shown());

        assert_eq!(document["outcome"], "ok");
        assert_eq!(document["status"], 200);
        assert_eq!(document["escalated"], true);
        assert_eq!(document["attempts"][0]["render"], "browser");
        assert_eq!(document["attempts"][0]["engine"], "chromium");
        assert_eq!(document["attempts"][0]["total_ms"], 3400);
        assert_eq!(document["pacing"]["applied"], "human");
        assert_eq!(document["pacing"]["waited_ms"], 700);
        assert_eq!(document["handoff"]["cookies_to_browser"], 1);
        assert_eq!(document["handoff"]["cookies_to_http"], 2);
        assert_eq!(document["relayed_bytes"]["down"], 480_100);
        assert_eq!(document["persona"]["engine"], "chromium");
        // Nothing was captured by this stand-in attempt.
        assert_eq!(document["behavior"], serde_json::Value::Null);
        assert_eq!(document["document_bytes"], 0);
        assert_eq!(document_of(&report(Outcome::Ok)), b"");
        assert_eq!(kilobytes(1_500), "1.5 kB");
    }

    #[test]
    fn the_order_carries_what_was_asked_about_extraction_the_cache_and_robots_txt() {
        let (config, policies) = (config(), policies());
        // Nothing asked: nothing extracted, nothing of the cache, the
        // policy's word on robots.txt, and nothing kept in the store.
        let plain = order(&options(), &config, &policies).unwrap();
        assert_eq!((plain.extract, plain.cache.clone()), (None, None));
        assert_eq!(plain.robots, RobotsWish::Policy);
        assert!(plain.keep.is_nothing());

        let asked = order(
            &FetchOptions {
                extract: true,
                cache_ttl: Some(Duration::from_mins(10)),
                robots: RobotsWish::Enforce,
                ..at("https://reported.example/a?utm_source=x#top")
            },
            &config,
            &policies,
        )
        .unwrap();
        assert_eq!(
            asked.extract,
            Some(ExtractOptions {
                markdown: true,
                links: true,
                thin_words: 80,
            })
        );
        assert_eq!(asked.robots, RobotsWish::Enforce);
        let cache = asked.cache.expect("a lifetime was named");
        assert_eq!(cache.ttl, Duration::from_mins(10));
        // The command line's entries are no key's, and it reads them.
        assert_eq!((cache.scope, cache.read), (None, true));
        assert_eq!(cache.key.len(), 64);
        assert_eq!(cache.url_key, "https://reported.example/a");
        // An answer from the cache can only return what was stored: the
        // document is kept, whichever path gets the page.
        assert!(asked.keep.dom && asked.keep.raw);
        assert!(!(asked.keep.xhr || asked.keep.screenshot || asked.keep.pdf));
        assert_eq!(asked.spec["extract"], true);
        assert_eq!(asked.spec["cache_ttl_s"], 600);
        assert_eq!(asked.spec["robots"], "enforce");

        let kept = |render| {
            let order = order(
                &FetchOptions {
                    render,
                    cache_ttl: Some(Duration::from_secs(1)),
                    ..options()
                },
                &config,
                &policies,
            )
            .unwrap();
            (order.keep.dom, order.keep.raw)
        };
        assert_eq!(kept(RenderMode::Browser), (true, false));
        assert_eq!(kept(RenderMode::Http), (false, true));

        // Left out, the site's policy decides; zero is never.
        let policys = order(&at("https://kept.example/a"), &config, &policies).unwrap();
        assert_eq!(
            policys.cache.map(|cache| cache.ttl),
            Some(Duration::from_mins(2))
        );
        let never = order(
            &FetchOptions {
                cache_ttl: Some(Duration::ZERO),
                ..at("https://kept.example/a")
            },
            &config,
            &policies,
        )
        .unwrap();
        assert_eq!(never.cache, None);
        assert!(never.keep.is_nothing());
    }

    #[test]
    fn a_wish_the_sites_policy_does_not_allow_is_refused_before_anything_starts() {
        let (config, policies) = (config(), policies());
        let refused = |options: FetchOptions| {
            format!("{:#}", order(&options, &config, &policies).unwrap_err())
        };
        // Nobody asks this site for its robots.txt: nothing could be
        // enforced.
        let ignored = refused(FetchOptions {
            robots: RobotsWish::Enforce,
            ..options()
        });
        assert!(ignored.contains("robots_ignored"), "{ignored}");
        assert!(ignored.contains("shop.example"), "{ignored}");
        // A command asks for more care than the policy, never for less.
        for wish in [RobotsWish::Report, RobotsWish::Ignore] {
            let enforced = refused(FetchOptions {
                robots: wish,
                ..at("https://enforced.example/a")
            });
            assert!(enforced.contains("robots_enforced"), "{enforced}");
        }
        for (url, wish) in [
            ("https://enforced.example/a", RobotsWish::Enforce),
            ("https://reported.example/a", RobotsWish::Enforce),
            ("https://reported.example/a", RobotsWish::Report),
            ("https://reported.example/a", RobotsWish::Ignore),
            ("https://shop.example/a", RobotsWish::Report),
            ("https://shop.example/a", RobotsWish::Ignore),
        ] {
            let options = FetchOptions {
                robots: wish,
                ..at(url)
            };
            assert!(
                order(&options, &config, &policies).is_ok(),
                "{url} {wish:?}"
            );
        }

        // Longer than results of the site may be served: refused, not cut.
        let too_long = refused(FetchOptions {
            cache_ttl: Some(Duration::from_secs(86_401)),
            ..options()
        });
        assert!(
            too_long.contains("--cache-ttl is at most 1d for shop.example"),
            "{too_long}"
        );
        let too_long = refused(FetchOptions {
            cache_ttl: Some(Duration::from_secs(301)),
            ..at("https://short.example/a")
        });
        assert!(
            too_long.contains("at most 5m for short.example"),
            "{too_long}"
        );
        let longest = FetchOptions {
            cache_ttl: Some(Duration::from_mins(5)),
            ..at("https://short.example/a")
        };
        assert!(order(&longest, &config, &policies).is_ok());
    }

    #[test]
    fn the_command_lines_cache_is_keyed_by_what_changes_the_result() {
        let (config, policies) = (config(), policies());
        let key = |options: FetchOptions| {
            let options = FetchOptions {
                cache_ttl: options.cache_ttl.or(Some(Duration::from_mins(1))),
                ..options
            };
            order(&options, &config, &policies)
                .unwrap()
                .cache
                .unwrap()
                .key
        };
        let base = key(options());
        // When and how politely a page is fetched is no part of the key,
        // nor is an address's fragment or a tracking parameter.
        for same in [
            FetchOptions {
                timeout: Some(Duration::from_secs(40)),
                retries: Some(0),
                pacing: Some("fast".to_owned()),
                cache_ttl: Some(Duration::from_mins(9)),
                output: Some("page.html".into()),
                json: true,
                ..options()
            },
            at("https://shop.example/page#reviews"),
            at("https://shop.example/page?utm_source=mail"),
        ] {
            assert_eq!(key(same), base);
        }
        // What changes what the result holds is.
        let persona = PersonaId::from_uuid(Uuid::from_u128(9));
        let others = [
            FetchOptions {
                extract: true,
                ..options()
            },
            FetchOptions {
                render: RenderMode::Browser,
                ..options()
            },
            FetchOptions {
                render: RenderMode::Http,
                ..options()
            },
            FetchOptions {
                engine: Some(Engine::Firefox),
                ..options()
            },
            FetchOptions {
                persona: Some(persona),
                ..options()
            },
            at("https://shop.example/page?id=2"),
        ];
        let mut keys: Vec<String> = others.into_iter().map(key).collect();
        keys.push(base);
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count);
    }

    #[test]
    fn what_was_extracted_the_cache_and_robots_txt_are_shown() {
        // Nothing asked and nothing known: none of the three is mentioned.
        let mut plain = report(Outcome::Ok);
        plain.robots = Some(RobotsVerdict {
            fetched: false,
            allowed: true,
            matched_rule: None,
            crawl_delay: None,
        });
        let text = said(&plain, &shown());
        assert!(
            !text.contains("robots.txt") && !text.contains("cache:") && !text.contains("extract:"),
            "{text}"
        );
        // Asked of a site whose file nobody asks for: the answer says so.
        let asked = Shown {
            robots_asked: true,
            ..shown()
        };
        let text = said(&plain, &asked);
        assert!(text.contains("robots.txt: not asked for\n"), "{text}");

        let mut read = report(Outcome::Ok);
        read.robots = Some(RobotsVerdict {
            fetched: true,
            allowed: true,
            matched_rule: Some("Allow: /page".to_owned()),
            crawl_delay: Some(2.5),
        });
        read.extract = Some(json!({
            "title": "The shop",
            "word_count": 335,
            "thin_content": false,
            "links": [{ "url": "https://shop.example/a" }, { "url": "https://shop.example/b" }],
        }));
        let kept = Shown {
            cache_ttl: Some(Duration::from_mins(10)),
            ..shown()
        };
        let text = said(&read, &kept);
        assert!(
            text.contains("robots.txt: allowed by \"Allow: /page\"; crawl-delay 2.5 s\n"),
            "{text}"
        );
        assert!(
            text.contains("extract: \"The shop\", 335 words, 2 links\n"),
            "{text}"
        );
        assert!(
            text.contains("cache: not in the cache; fetched, and kept for 10m\n"),
            "{text}"
        );
        let document = json_report(&read, &kept);
        assert_eq!(document["robots"]["matched_rule"], "Allow: /page");
        assert_eq!(document["extract"]["word_count"], 335);
        assert_eq!(document["cached"], false);
        assert_eq!(document["cached_at"], serde_json::Value::Null);
        assert_eq!(document["cache_ttl_s"], 600);

        // A page that says next to nothing is not kept, and neither is one
        // that was not fetched.
        read.extract = Some(json!({ "title": null, "word_count": 12, "thin_content": true }));
        let text = said(&read, &kept);
        assert!(
            text.contains("extract: no title, 12 words, thin\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "cache: not in the cache; fetched, and not kept: the page says next to nothing\n"
            ),
            "{text}"
        );
        let text = said(&report(Outcome::Forbidden), &kept);
        assert!(
            text.contains("cache: not in the cache; not kept: the page was not fetched\n"),
            "{text}"
        );

        // Disallowed: nothing was asked of the site.
        let mut denied = report(Outcome::RobotsDenied);
        denied.attempts.clear();
        denied.last = None;
        denied.block = None;
        denied.escalated = false;
        denied.robots = Some(RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: None,
        });
        let text = said(&denied, &shown());
        assert!(text.starts_with("robots_denied  -  -\n"), "{text}");
        assert!(
            text.contains("robots.txt: disallowed by \"Disallow: /private\"\n"),
            "{text}"
        );
    }

    #[test]
    fn an_answer_from_the_cache_shows_the_stored_page() {
        // No attempt was made, and the document is the one in the store.
        let kept = Shown {
            cache_ttl: Some(Duration::from_mins(10)),
            ..shown()
        };
        let at = Utc.with_ymd_and_hms(2026, 10, 13, 9, 0, 0).unwrap();
        let mut hit = report(Outcome::Ok);
        let stored = ArtifactRef {
            kind: ArtifactKind::DomHtml,
            id: ArtifactId::from_uuid(Uuid::from_u128(7)),
        };
        hit.attempts.clear();
        hit.last = None;
        hit.escalated = false;
        hit.artifacts = vec![
            ArtifactRef {
                kind: ArtifactKind::RawBody,
                id: ArtifactId::from_uuid(Uuid::from_u128(6)),
            },
            stored,
        ];
        hit.cached = Some(Box::new(FetchRecord {
            id: hit.result,
            request_id: hit.request,
            outcome: Outcome::Ok,
            status: Some(200),
            headers: Vec::new(),
            final_url: Some("https://shop.example/page".to_owned()),
            redirects: Vec::new(),
            extract: None,
            xhr: None,
            artifacts: hit.artifacts.clone(),
            persona: None,
            block: None,
            robots: None,
            cached: true,
            cached_at: Some(at),
            reason: None,
            visit: kage_core::domain::Visit::default(),
            reading: None,
            created_at: at,
        }));
        let served = Shown {
            document_bytes: 4_200,
            ..kept
        };
        let text = said(&hit, &served);
        assert!(
            text.starts_with("ok  200  https://shop.example/page\n"),
            "{text}"
        );
        assert!(
            text.contains("cache: answered from the cache, fetched 2026-10-13 09:00:00 UTC\n"),
            "{text}"
        );
        assert!(text.contains("document: 4.2 kB; "), "{text}");
        assert!(!text.contains("attempts:"), "{text}");
        let document = json_report(&hit, &served);
        assert_eq!(document["cached"], true);
        assert_eq!(document["cached_at"], "2026-10-13T09:00:00Z");
        assert_eq!(document["status"], 200);
        assert_eq!(document["final_url"], "https://shop.example/page");
        assert_eq!(document["document_bytes"], 4_200);
        // The document of a browser before the body a server sent.
        assert_eq!(stored_document(&hit), Some(stored));
        assert_eq!(stored_document(&report(Outcome::Ok)), None);
    }

    #[test]
    fn robots_is_one_of_four_words() {
        assert_eq!(parse_robots("policy"), Ok(RobotsWish::Policy));
        assert_eq!(parse_robots("report"), Ok(RobotsWish::Report));
        assert_eq!(parse_robots("enforce"), Ok(RobotsWish::Enforce));
        assert_eq!(parse_robots("ignore"), Ok(RobotsWish::Ignore));
        assert!(
            parse_robots("obey")
                .unwrap_err()
                .contains("policy, report, enforce or ignore")
        );
    }
}
