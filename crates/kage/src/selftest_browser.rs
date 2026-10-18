//! `kage selftest browser`: the self-test of the browser path (spec §7.4,
//! §10.2) for one engine through one proxy.
//!
//! It asks the engine's sidecar for a persona, opens a context through the
//! proxy, lets the sidecar load the self-test pages and judge them, and
//! closes the context again. The pages are the operator's own
//! (`selftest.browser.pages`); `--third-party` adds the detection pages run
//! by others (`selftest.browser.third_party_pages`), which see the exit
//! address of the proxy. Only this command and the gated live suite ever
//! load those, and the server never does.
//!
//! A persona is tested with `kage persona selftest` (or `--persona`
//! here), in its own context. This command is for a proxy that has none,
//! and for comparing engines through one address: it stands in with one
//! pseudo-persona per proxy and engine. Its id is the proxy's id and its
//! seed is derived from it, so a proxy shows the same browser on every
//! run and keeps one profile directory per engine
//! (`selftest/<engine>/<proxy id>`). The stand-in claims the operating
//! system its engine claims by default: Linux on chromium, Windows on
//! firefox. Its time zone, locale and languages are what a persona behind
//! that proxy would get: derived from the proxy's observed geo record and
//! the stock language list of that browser in that country
//! (`kage_core::persona::locale`), unless they are named.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, bail};
use kage_browser::contract::{
    ClaimedOs, EchoExpectation, ExpectedIdentity, FindingStatus, GeneratePersonaRequest,
    OpenContextRequest, PersonaSummary, Proxy as ContextProxy, SelftestPage, SelftestPageKind,
    SelftestRequest, SelftestResponse,
};
use kage_browser::{BrowserError, SidecarClient, SidecarSettings};
use kage_core::config::{Config, SelftestPageConfig};
use kage_core::domain::{AuditOutcome, Engine, Proxy, ProxyEndpoint};
use kage_core::ids::ProxyId;
use kage_core::persona::locale;
use kage_core::seed::Seed;
use serde_json::json;

use crate::proxy_cmd::{Session, table};

/// What `kage selftest browser` was asked to do.
#[derive(Debug, Clone)]
pub(crate) struct BrowserSelftestOptions {
    /// The proxy to leave through.
    pub(crate) proxy: ProxyId,
    /// The engine whose sidecar runs the test.
    pub(crate) engine: Engine,
    /// The IANA time zone of the stand-in persona; `None` for the one of
    /// the proxy's geo record.
    pub(crate) timezone: Option<String>,
    /// The languages of the stand-in persona, most preferred first; empty
    /// for the stock list of the engine's browser in the proxy's country.
    pub(crate) languages: Vec<String>,
    /// Also load the detection pages run by others.
    pub(crate) third_party: bool,
    /// Keep the context open and untouched for this long before any page
    /// is loaded.
    pub(crate) idle_before: Option<Duration>,
    /// One JSON document instead of a table.
    pub(crate) json: bool,
}

/// `kage selftest browser`.
pub(crate) async fn browser(
    config: &Config,
    options: &BrowserSelftestOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = browser_in(&session, config, options, out).await;
    session.close().await;
    result
}

/// The pages of one run as the contract writes them.
fn pages_of(config: &Config, third_party: bool) -> anyhow::Result<Vec<SelftestPage>> {
    let selftest = &config.selftest.browser;
    let mut chosen: Vec<&SelftestPageConfig> = selftest.pages.iter().collect();
    if third_party {
        chosen.extend(&selftest.third_party_pages);
    }
    if chosen.is_empty() {
        bail!(
            "there is no page to load: name the operator's own pages in selftest.browser.pages, or pass --third-party"
        );
    }
    chosen
        .into_iter()
        .map(|page| {
            let kind: SelftestPageKind = serde_json::from_value(json!(page.kind.as_str()))
                .with_context(|| {
                    format!("the sidecar contract has no page kind {:?}", page.kind)
                })?;
            Ok(SelftestPage {
                id: page.id.clone(),
                kind,
                rules: None,
                settle_ms: None,
                url: page.url.clone(),
                weight: None,
            })
        })
        .collect()
}

/// The seed of the stand-in persona of a proxy: the proxy's id, twice.
fn seed_of(proxy: ProxyId) -> String {
    let id = proxy.as_uuid().as_u128();
    format!("{id:032x}{id:032x}")
}

/// The operating system the stand-in persona of an engine claims.
///
/// A chromium persona claims the system its browser runs on, Linux
/// (docs/adr/0009). Camoufox sets the claimed system inside the browser
/// and does not claim Linux by default, so its stand-in claims Windows,
/// the system the engine's capture was recorded with.
const fn stand_in_os(engine: Engine) -> ClaimedOs {
    match engine {
        Engine::Chromium => ClaimedOs::Linux,
        Engine::Firefox => ClaimedOs::Windows,
    }
}

/// The profile directory of the stand-in persona of a proxy on an engine,
/// below the personas volume. The two sidecars share that volume and one
/// browser cannot open the other's profile, so each engine has its own.
fn profile_dir_of(engine: Engine, proxy: ProxyId) -> String {
    format!("selftest/{}/{proxy}", engine.as_str())
}

/// What an echo page must report: the capture recorded for this browser.
fn echo_expectation(engine: Engine, major: u64) -> Option<EchoExpectation> {
    let major = u32::try_from(major).ok()?;
    let capture = kage_http::selftest::browser_capture(engine.as_str(), major)?;
    Some(EchoExpectation {
        akamai: capture.akamai,
        headers: capture.headers,
        ja4: capture.ja4,
    })
}

/// Where the stand-in persona is and what it speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StandIn {
    timezone: String,
    locale: String,
    languages: Vec<String>,
}

/// The place and languages of the stand-in persona of a proxy: what a
/// persona behind it would get, unless the caller named them.
///
/// A named first language is the locale. Firefox takes its regional
/// formats from the locale's region, and its engine refuses a locale
/// without one, so that is refused here, before a sidecar is asked.
fn stand_in(proxy: &Proxy, options: &BrowserSelftestOptions) -> anyhow::Result<StandIn> {
    let seed = Seed::derive("selftest-stand-in", &[proxy.id.as_uuid().as_bytes()]);
    let place = kage_persona::lifecycle::place_of(proxy, options.engine, &seed);
    let (locale, languages) = match options.languages.first() {
        Some(first) => {
            if options.engine == Engine::Firefox && locale::region_of(first).is_none() {
                bail!(
                    "--language {first}: the first language of a firefox persona is its locale \n                     and needs a region, for example {first}-{}; the languages after it may be \n                     bare",
                    locale::region_of(&place.locale.locale).unwrap_or("DE")
                );
            }
            (first.clone(), options.languages.clone())
        }
        None => (place.locale.locale, place.locale.languages),
    };
    Ok(StandIn {
        timezone: options.timezone.clone().unwrap_or(place.timezone),
        locale,
        languages,
    })
}

/// The conversation with the sidecar: a persona, a context through the
/// proxy, the self-test, and the context closed again whatever it found.
async fn converse(
    client: &SidecarClient,
    config: &Config,
    options: &BrowserSelftestOptions,
    identity: &StandIn,
    endpoint: &ProxyEndpoint,
    seed: &str,
    pages: Vec<SelftestPage>,
) -> anyhow::Result<(PersonaSummary, Result<SelftestResponse, BrowserError>)> {
    let claimed_os = stand_in_os(options.engine);
    let persona = client
        .generate_persona(&GeneratePersonaRequest {
            browser_major: None,
            claimed_os,
            languages: identity.languages.clone(),
            screen: None,
            seed: seed.to_owned(),
        })
        .await
        .context("the sidecar could not generate a persona")?;
    let summary = persona.summary;
    let context = client
        .open_context(&OpenContextRequest {
            fingerprint: persona.fingerprint,
            geolocation: None,
            languages: identity.languages.clone(),
            locale: identity.locale.clone(),
            persona_id: endpoint.id.as_uuid(),
            profile_dir: profile_dir_of(options.engine, endpoint.id),
            proxy: ContextProxy {
                password: endpoint.password.expose().to_owned(),
                server: endpoint.url(),
                username: endpoint.username.expose().to_owned(),
            },
            screen: summary.screen.clone(),
            timezone: identity.timezone.clone(),
        })
        .await
        .context("the sidecar could not open a context")?;

    // What a browser sends when nobody uses it (updates, safe browsing) is
    // only visible on the wire. The gated live suite measures it in this
    // pause; the sidecar's idle limit must be longer, or it closes the
    // context meanwhile.
    if let Some(idle) = options.idle_before {
        tracing::info!(
            seconds = idle.as_secs(),
            "the context is open; leaving it idle"
        );
        tokio::time::sleep(idle).await;
    }

    let selftest = &config.selftest.browser;
    let request = SelftestRequest {
        artifact_prefix: None,
        echo: echo_expectation(options.engine, summary.browser_major),
        expected: ExpectedIdentity {
            // The major of the browser the engine bundles, as its sidecar
            // reports it.
            browser_major: summary.browser_major,
            // The system that was asked for, not the one the sidecar says
            // it generated: a persona that came back claiming another
            // system must fail the test, not move its expectation.
            claimed_os,
            languages: identity.languages.clone(),
            timezone: identity.timezone.clone(),
        },
        min_score: Some(f64::from(selftest.min_score)),
        pages: Some(pages),
        timeout_ms: u64::try_from(selftest.page_timeout.get().as_millis()).ok(),
    };
    let outcome = client.selftest(context.context_id, &request).await;
    if let Err(error) = client.close_context(context.context_id).await {
        tracing::warn!(error = %error, "the self-test's context could not be closed");
    }
    Ok((summary, outcome))
}

async fn browser_in(
    session: &Session,
    config: &Config,
    options: &BrowserSelftestOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let Some(proxy) = session.pool.get(options.proxy).await? else {
        bail!("there is no proxy {}", options.proxy);
    };
    if !proxy.state.is_live() {
        bail!("proxy {} is retired", proxy.id);
    }
    let pages = pages_of(config, options.third_party)?;
    let endpoint = session
        .pool
        .endpoint(&proxy)
        .context("cannot open the proxy's credentials; was the master key changed?")?;
    let client = SidecarClient::new(&SidecarSettings::from_config(config, options.engine))
        .context("cannot build the sidecar client")?;

    let seed = seed_of(proxy.id);
    let identity = stand_in(&proxy, options)?;
    let (summary, outcome) =
        converse(&client, config, options, &identity, &endpoint, &seed, pages).await?;

    let details = |passed: Option<bool>, score: Option<f64>| {
        json!({
            "engine": options.engine.as_str(),
            "browser_version": summary.browser_version,
            "third_party": options.third_party,
            "timezone": identity.timezone,
            "languages": identity.languages,
            "seed": seed,
            "passed": passed,
            "score": score,
        })
    };
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            session
                .audit(
                    "selftest.browser",
                    Some(proxy.id.to_string()),
                    AuditOutcome::Failure,
                    details(None, None),
                )
                .await;
            return Err(error).context("the self-test could not be run");
        }
    };
    session
        .audit(
            "selftest.browser",
            Some(proxy.id.to_string()),
            if report.passed {
                AuditOutcome::Success
            } else {
                AuditOutcome::Failure
            },
            details(Some(report.passed), Some(report.score)),
        )
        .await;

    write_report(out, options, &seed, &summary, &report).context("cannot write the result")?;
    if !report.passed {
        bail!("the browser self-test failed");
    }
    Ok(())
}

fn write_report(
    out: &mut impl Write,
    options: &BrowserSelftestOptions,
    seed: &str,
    summary: &PersonaSummary,
    report: &SelftestResponse,
) -> anyhow::Result<()> {
    if options.json {
        let mut document = serde_json::to_value(report)?;
        if let Some(object) = document.as_object_mut() {
            object.insert("proxy".to_owned(), json!(options.proxy));
            object.insert("engine".to_owned(), json!(options.engine.as_str()));
            object.insert("seed".to_owned(), json!(seed));
            object.insert("persona".to_owned(), serde_json::to_value(summary)?);
        }
        serde_json::to_writer_pretty(&mut *out, &document)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(
        out,
        "{} {} on {} through proxy {}",
        options.engine.as_str(),
        summary.browser_version,
        summary.claimed_os.as_str(),
        options.proxy
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
    for page in &report.pages {
        if page.status.as_str() != "ok" {
            writeln!(out, "page {}: {}", page.id, page.status.as_str())?;
        }
    }
    writeln!(
        out,
        "score {:.1} of 100, threshold {:.0}{}",
        report.score,
        report.min_score,
        if report.complete {
            ""
        } else {
            "; incomplete: a page could not be read"
        }
    )?;
    writeln!(
        out,
        "{}",
        if report.passed {
            "passed: no automation flag, the identity is coherent"
        } else {
            "FAILED: see the findings above"
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kage_browser::contract::{
        ClaimedOs, EXAMPLES_JSON, GeneratePersonaResponse, PersonaSummary, SelftestResponse,
    };
    use kage_core::config::{Config, SelftestPageConfig, SelftestPageKind};
    use kage_core::domain::{
        Engine, GeoRecord, Proxy, ProxyHealth, ProxyProviderKind, ProxyScheme, ProxyState,
    };
    use kage_core::ids::ProxyId;
    use kage_core::secret::SealedSecret;

    use super::{
        BrowserSelftestOptions, echo_expectation, pages_of, profile_dir_of, seed_of, stand_in_os,
        write_report,
    };

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

    fn example(schema: &str) -> serde_json::Value {
        let examples: serde_json::Value =
            serde_json::from_str(EXAMPLES_JSON).expect("the examples are JSON");
        examples[schema][0].clone()
    }

    fn examples() -> (PersonaSummary, SelftestResponse) {
        let persona: GeneratePersonaResponse =
            serde_json::from_value(example("GeneratePersonaResponse")).expect("a persona");
        let report: SelftestResponse =
            serde_json::from_value(example("SelftestResponse")).expect("a report");
        (persona.summary, report)
    }

    fn options(json: bool) -> BrowserSelftestOptions {
        BrowserSelftestOptions {
            proxy: "0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
                .parse::<ProxyId>()
                .expect("a proxy id"),
            engine: Engine::Chromium,
            timezone: Some("Europe/Berlin".to_owned()),
            languages: vec!["de-DE".to_owned(), "de".to_owned()],
            third_party: true,
            idle_before: None,
            json,
        }
    }

    #[test]
    fn third_party_pages_are_only_loaded_when_asked_for() {
        let mut config = config();
        let error = pages_of(&config, false).unwrap_err().to_string();
        assert!(error.contains("--third-party"), "{error}");

        let ids = |pages: Vec<kage_browser::contract::SelftestPage>| {
            pages
                .into_iter()
                .map(|page| format!("{}={}", page.id, page.kind.as_str()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(pages_of(&config, true).expect("pages")),
            ["sannysoft=sannysoft", "creepjs=creepjs"]
        );

        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "https://probe.example.org/probe/".to_owned(),
        }];
        assert_eq!(
            ids(pages_of(&config, false).expect("pages")),
            ["probe=probe"]
        );
        assert_eq!(
            ids(pages_of(&config, true).expect("pages")),
            ["probe=probe", "sannysoft=sannysoft", "creepjs=creepjs"]
        );
    }

    fn proxy(country: &str, observed: Option<(&str, &str)>) -> Proxy {
        Proxy {
            id: ProxyId::new(),
            provider: ProxyProviderKind::Static,
            provider_ref: "fixture".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: 8080,
            username: SealedSecret::new(Vec::new()),
            password: SealedSecret::new(Vec::new()),
            scheme: ProxyScheme::Http,
            country: country.parse().ok(),
            city: None,
            asn: None,
            state: ProxyState::Healthy,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: observed.map(|(country, timezone)| ProxyHealth {
                geo: Some(GeoRecord {
                    country: country.parse().expect("a country"),
                    city: None,
                    region: None,
                    asn: None,
                    org: None,
                    timezone: Some(timezone.to_owned()),
                    loc: None,
                    ip: "203.0.113.9".parse().expect("an address"),
                    checked_at: chrono::Utc::now(),
                }),
                ..ProxyHealth::new(chrono::Utc::now(), true)
            }),
            created_at: chrono::Utc::now(),
            retired_at: None,
        }
    }

    #[test]
    fn the_stand_in_speaks_what_a_persona_behind_the_proxy_would() {
        let derived = |engine| BrowserSelftestOptions {
            engine,
            timezone: None,
            languages: Vec::new(),
            ..options(false)
        };
        // A German address: Firefox ships `de, en-US, en` there, and never
        // the `de-DE, de, en` no stock Firefox sends; Chrome ships
        // `de-DE, de, en-US, en`.
        let german = proxy("DE", Some(("DE", "Europe/Berlin")));
        let stand_in = |proxy: &Proxy, options: &BrowserSelftestOptions| {
            super::stand_in(proxy, options).unwrap()
        };
        let firefox = stand_in(&german, &derived(Engine::Firefox));
        assert_eq!(firefox.languages, ["de", "en-US", "en"]);
        assert_eq!(firefox.locale, "de-DE");
        assert_eq!(firefox.timezone, "Europe/Berlin");
        let chrome = stand_in(&german, &derived(Engine::Chromium));
        assert_eq!(chrome.languages, ["de-DE", "de", "en-US", "en"]);
        assert_eq!(chrome.locale, "de-DE");

        // Where the address was observed to be wins over the provider.
        let moved = proxy("DE", Some(("AT", "Europe/Vienna")));
        let austrian = stand_in(&moved, &derived(Engine::Firefox));
        assert_eq!(austrian.locale, "de-AT");
        assert_eq!(austrian.languages, ["de", "en-US", "en"]);
        assert_eq!(austrian.timezone, "Europe/Vienna");
        // No lookup yet: the provider's country and its main zone.
        let unplaced = stand_in(&proxy("IT", None), &derived(Engine::Chromium));
        assert_eq!(unplaced.languages, ["it-IT", "it", "en-US", "en"]);
        assert_eq!(unplaced.timezone, "Europe/Rome");

        // What the caller names is used as it is.
        let named = stand_in(&german, &options(false));
        assert_eq!(named.languages, ["de-DE", "de"]);
        assert_eq!(named.locale, "de-DE");
        assert_eq!(named.timezone, "Europe/Berlin");
        let zone_only = stand_in(
            &german,
            &BrowserSelftestOptions {
                timezone: Some("Europe/Zurich".to_owned()),
                ..derived(Engine::Firefox)
            },
        );
        assert_eq!(zone_only.timezone, "Europe/Zurich");
        assert_eq!(zone_only.languages, ["de", "en-US", "en"]);
    }

    #[test]
    fn a_firefox_stand_in_needs_a_region_in_its_first_language() {
        let german = proxy("DE", Some(("AT", "Europe/Vienna")));
        let named = |engine, languages: &[&str]| {
            super::stand_in(
                &german,
                &BrowserSelftestOptions {
                    engine,
                    languages: languages.iter().map(|tag| (*tag).to_owned()).collect(),
                    ..options(false)
                },
            )
        };

        // Refused before any sidecar is asked, with the tag to write.
        let error = named(Engine::Firefox, &["de", "en-US"])
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("--language de:"), "{error}");
        assert!(error.contains("de-AT"), "{error}");
        assert!(named(Engine::Firefox, &["zh-Hant"]).is_err());

        // Only the first language is the locale; the others may be bare.
        let regional = named(Engine::Firefox, &["de-AT", "de", "en"]).unwrap();
        assert_eq!(regional.locale, "de-AT");
        assert_eq!(regional.languages, ["de-AT", "de", "en"]);
        // Chrome takes a bare language, as it always did.
        assert_eq!(named(Engine::Chromium, &["de"]).unwrap().locale, "de");
    }

    #[test]
    fn the_stand_in_persona_of_a_proxy_is_always_the_same() {
        let proxy = options(false).proxy;
        let seed = seed_of(proxy);
        assert_eq!(seed.len(), 64);
        assert!(seed.chars().all(|digit| digit.is_ascii_hexdigit()));
        assert_eq!(seed, seed_of(proxy));
        assert_eq!(
            seed,
            "0199c1f26b7a7c3e9d412f8a5b6c7d8e0199c1f26b7a7c3e9d412f8a5b6c7d8e"
        );
    }

    #[test]
    fn the_stand_in_persona_fits_its_engine() {
        // Chrome claims the system it runs on; Camoufox does not claim
        // Linux by default.
        assert_eq!(stand_in_os(Engine::Chromium), ClaimedOs::Linux);
        assert_eq!(stand_in_os(Engine::Firefox), ClaimedOs::Windows);
        // Each is the system the engine's capture was recorded with, so the
        // stand-in is the browser the echo expectation describes.
        for (engine, major) in [(Engine::Chromium, 155), (Engine::Firefox, 156)] {
            let capture = kage_http::selftest::browser_capture(engine.as_str(), major)
                .expect("a capture of the engine's browser");
            assert_eq!(capture.os, stand_in_os(engine).as_str(), "{engine}");
        }

        // One proxy keeps one profile directory per engine: the sidecars
        // share the personas volume.
        let proxy = options(false).proxy;
        assert_eq!(
            profile_dir_of(Engine::Chromium, proxy),
            "selftest/chromium/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
        );
        assert_eq!(
            profile_dir_of(Engine::Firefox, proxy),
            "selftest/firefox/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
        );
    }

    #[test]
    fn an_echo_page_is_judged_against_the_recorded_capture() {
        let expectation = echo_expectation(Engine::Chromium, 155).expect("a capture of Chrome 155");
        assert!(expectation.ja4.starts_with("t13d"));
        assert!(expectation.headers.iter().any(|name| name == "user-agent"));
        assert!(echo_expectation(Engine::Chromium, 1).is_none());
        assert!(echo_expectation(Engine::Chromium, u64::MAX).is_none());

        // The firefox engine is judged against its own capture.
        let firefox = echo_expectation(Engine::Firefox, 156).expect("a capture of Firefox 156");
        assert_eq!(firefox.ja4, "t13d1517h2_8daaf6152771_3cbfd9057e0d");
        assert_eq!(
            firefox.akamai,
            "1:65536;2:0;4:131072;5:16384|12517377|0|m,p,a,s"
        );
        assert_eq!(
            firefox.headers.first().map(String::as_str),
            Some("user-agent")
        );
        assert_eq!(firefox.headers.last().map(String::as_str), Some("te"));
        assert_ne!(firefox.ja4, expectation.ja4);
        // A capture belongs to its engine: the major of one is nothing to
        // the other.
        assert!(echo_expectation(Engine::Firefox, 155).is_none());
        assert!(echo_expectation(Engine::Chromium, 156).is_none());
    }

    #[test]
    fn the_report_is_a_table_or_one_json_document() {
        let (summary, report) = examples();

        let mut table = Vec::new();
        write_report(&mut table, &options(false), "seed", &summary, &report).expect("a table");
        let table = String::from_utf8(table).expect("text");
        assert!(table.starts_with("chromium 155.0.8059.39 on linux through proxy"));
        assert!(table.contains("PAGE"), "{table}");
        assert!(table.contains("score "), "{table}");
        for finding in &report.findings {
            assert!(table.contains(&finding.check), "{table}");
        }

        let mut json = Vec::new();
        write_report(&mut json, &options(true), "seed", &summary, &report).expect("json");
        let document: serde_json::Value = serde_json::from_slice(&json).expect("json");
        assert_eq!(document["engine"], "chromium");
        assert_eq!(document["seed"], "seed");
        assert_eq!(document["persona"]["browser_major"], 155);
        assert_eq!(document["passed"], report.passed);
        assert_eq!(
            document["findings"].as_array().map(Vec::len),
            Some(report.findings.len())
        );

        // The first line names the engine that ran and what its persona
        // claims.
        let mut persona = example("GeneratePersonaResponse");
        for (key, value) in [
            ("engine", serde_json::json!("firefox")),
            ("claimed_os", serde_json::json!("windows")),
            ("browser_version", serde_json::json!("156.0.1")),
            ("browser_major", serde_json::json!(156)),
        ] {
            persona["summary"][key] = value;
        }
        let persona: GeneratePersonaResponse =
            serde_json::from_value(persona).expect("a firefox persona");
        let firefox = BrowserSelftestOptions {
            engine: Engine::Firefox,
            ..options(false)
        };
        let mut table = Vec::new();
        write_report(&mut table, &firefox, "seed", &persona.summary, &report).expect("a table");
        let table = String::from_utf8(table).expect("text");
        assert!(
            table.starts_with("firefox 156.0.1 on windows through proxy"),
            "{table}"
        );
    }
}
