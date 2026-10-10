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
use kage_core::config::Config;
use kage_core::domain::{AuditOutcome, Engine as BrowserEngine, RenderMode};
use kage_core::ids::PersonaId;
use kage_core::policy::{PacingRequest, PolicyStore};
use kage_core::schedule::queue::Priority;
use kage_scheduler::{FetchOrder, FetchReport};
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
}

/// Bytes the forwarder of this command relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Relayed {
    up: u64,
    down: u64,
}

/// Runs `kage fetch`, writing its result to `out`.
pub(crate) async fn fetch(
    config: &Config,
    options: &FetchOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = run(config, &session, options, out).await;
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
            .fetch(order(options))
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

    let relayed = Relayed { up, down };
    if let Some(path) = &options.output {
        let document = document_of(&report);
        std::fs::write(path, &document)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    if options.json {
        serde_json::to_writer_pretty(&mut *out, &json_report(&report, relayed))?;
        writeln!(out)?;
    } else {
        write_report(out, &report, relayed)?;
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

fn order(options: &FetchOptions) -> FetchOrder {
    FetchOrder {
        render: options.render,
        pin: options.persona,
        engines: options.engine.into_iter().collect(),
        timeout: options.timeout,
        max_retries: options.retries,
        pacing: options.pacing.clone().map(PacingRequest::Profile),
        spec: json!({
            "url": options.url.as_str(),
            "render": options.render.as_str(),
            "source": "cli",
        }),
        ..FetchOrder::get(options.url.clone(), Priority::Sync)
    }
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

fn json_report(report: &FetchReport, relayed: Relayed) -> Value {
    let last = report.last.as_ref();
    let page = last.and_then(|last| last.page.as_ref());
    json!({
        "request_id": report.request,
        "result_id": report.result,
        "outcome": report.outcome.as_str(),
        "reason": report.reason,
        "retry_at": report.retry_at,
        "status": last.and_then(|last| last.status),
        "final_url": last.and_then(|last| last.final_url.clone()),
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
        "document_bytes": document_of(report).len(),
        "title": page.and_then(|page| page.title.clone()),
        // Everything this command's forwarder relayed for the request.
        "relayed_bytes": { "up": relayed.up, "down": relayed.down },
    })
}

fn kilobytes(bytes: u64) -> String {
    // A page is far below 2^53 bytes.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    format!("{:.1} kB", bytes / 1000.0)
}

fn write_report(
    out: &mut impl Write,
    report: &FetchReport,
    relayed: Relayed,
) -> anyhow::Result<()> {
    let last = report.last.as_ref();
    let status = last
        .and_then(|last| last.status)
        .map_or_else(|| "-".to_owned(), |status| status.to_string());
    let final_url = last
        .and_then(|last| last.final_url.as_deref())
        .unwrap_or("-");
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
    writeln!(
        out,
        "document: {}; relayed: {} up, {} down",
        kilobytes(document_of(report).len() as u64),
        kilobytes(relayed.up),
        kilobytes(relayed.down)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use kage_core::domain::{Attempt, Engine, PersonaSummary, Render, RenderMode, Timing};
    use kage_core::ids::{AttemptId, PersonaId, RequestId, ResultId};
    use kage_core::outcome::{Block, BlockType, Outcome, Vendor};
    use kage_core::policy::PacingRequest;
    use kage_core::schedule::politeness::Courtesy;
    use kage_core::schedule::queue::Priority;
    use kage_scheduler::{AttemptOutput, FetchReport, PacingReport};
    use uuid::Uuid;

    use super::{FetchOptions, Relayed, document_of, json_report, kilobytes, order, write_report};

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
            }],
            last: Some(last),
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
        }
    }

    const RELAYED: Relayed = Relayed {
        up: 1_200,
        down: 480_100,
    };

    #[test]
    fn the_order_is_what_was_asked_and_the_policys_otherwise() {
        let plain = order(&options());
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
        let asked = order(&FetchOptions {
            render: RenderMode::Browser,
            persona: Some(persona),
            engine: Some(Engine::Firefox),
            timeout: Some(Duration::from_secs(40)),
            retries: Some(0),
            pacing: Some("fast".to_owned()),
            ..options()
        });
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
        write_report(&mut out, &report(Outcome::Ok), RELAYED).unwrap();
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
        write_report(&mut out, &report(Outcome::Forbidden), RELAYED).unwrap();
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
        write_report(&mut out, &refused, RELAYED).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("provider_error  -  -\n"), "{text}");
        assert!(
            text.contains("reason: domain_paused; worth asking again at 2026-10-10 12:30:00 UTC\n"),
            "{text}"
        );
        assert!(!text.contains("attempts:"), "{text}");

        refused.retry_at = None;
        let mut out = Vec::new();
        write_report(&mut out, &refused, RELAYED).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("reason: domain_paused\n")
        );
    }

    #[test]
    fn the_json_report_carries_what_an_operator_and_a_test_look_for() {
        let document = json_report(&report(Outcome::Ok), RELAYED);

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
}
