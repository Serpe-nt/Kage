//! `kage selftest http`: the fingerprint self-test of the http path (spec
//! §7.4) for one emulation profile through one proxy.
//!
//! It sends one typed navigation to a fingerprint echo and compares what
//! the echo saw (JA4, the HTTP/2 fingerprint, the header order) with the
//! values recorded for the profile. The echo is, in this order: the one
//! named with `--echo`, the operator's own (`selftest.http.echo_url`), or
//! the third-party echo (`selftest.http.third_party_url`). A third party
//! sees the exit address of the proxy that asks, which is why only this
//! command and the gated live suite ever ask it, and the server never does.
//!
//! The persona flag arrives with personas (milestone M6); until then the
//! profile and the proxy are named separately.

use std::io::Write;

use anyhow::{Context, bail};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::config::Config;
use kage_core::domain::AuditOutcome;
use kage_core::ids::ProxyId;
use kage_http::selftest::{self, SelftestReport};
use kage_http::{ClientFactory, ClientSpec, HttpSettings, ProfileChoice, profile};
use serde_json::json;
use url::Url;

use crate::proxy_cmd::{Session, table};

/// What `kage selftest http` was asked to do.
#[derive(Debug, Clone)]
pub(crate) struct HttpSelftestOptions {
    /// The proxy to leave through.
    pub(crate) proxy: ProxyId,
    /// The wreq-util profile, for example `chrome_149`.
    pub(crate) profile: String,
    /// The operating system the profile claims.
    pub(crate) platform: String,
    /// An echo to ask instead of the configured one.
    pub(crate) echo: Option<String>,
    /// One JSON document instead of a table.
    pub(crate) json: bool,
}

/// `kage selftest http`.
pub(crate) async fn http(
    config: &Config,
    options: &HttpSelftestOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = http_in(&session, config, options, out).await;
    session.close().await;
    result
}

/// The echo to ask and where its address came from.
fn echo_of(config: &Config, named: Option<&str>) -> anyhow::Result<(Url, &'static str)> {
    let selftest = &config.selftest.http;
    let (text, source) = match (named, &selftest.echo_url) {
        (Some(url), _) => (url, "--echo"),
        (None, Some(url)) => (url.as_str(), "selftest.http.echo_url"),
        (None, None) => (
            selftest.third_party_url.as_str(),
            "selftest.http.third_party_url",
        ),
    };
    let url: Url = text
        .parse()
        .with_context(|| format!("the echo {text:?} is not a URL"))?;
    if url.scheme() != "https" {
        bail!("the echo must be an https URL: a fingerprint echo reads the TLS handshake");
    }
    Ok((url, source))
}

async fn http_in(
    session: &Session,
    config: &Config,
    options: &HttpSelftestOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let Some(proxy) = session.pool.get(options.proxy).await? else {
        bail!("there is no proxy {}", options.proxy);
    };
    if !proxy.state.is_live() {
        bail!("proxy {} is retired", proxy.id);
    }
    let choice = ProfileChoice::named(&options.profile, profile::os_named(&options.platform)?)?;
    let (echo, source) = echo_of(config, options.echo.as_deref())?;
    let endpoint = session
        .pool
        .endpoint(&proxy)
        .context("cannot open the proxy's credentials; was the master key changed?")?;

    let mut factory = ClientFactory::in_memory(HttpSettings::from_config(&config.http));
    if let Some(path) = &config.selftest.http.echo_ca_file {
        let pem = std::fs::read(path).with_context(|| {
            format!("cannot read selftest.http.echo_ca_file {}", path.display())
        })?;
        factory = factory.with_trust_anchor(pem);
    }
    // A fresh client: a new connection and no cookies, so the echo sees a
    // first visit.
    let client = factory
        .ephemeral(&ClientSpec::new(choice), &endpoint)
        .context("cannot build the client")?;

    let outcome = selftest::run(&client, &echo, config.selftest.http.timeout.get()).await;
    let details = |passed: Option<bool>| {
        json!({
            "profile": options.profile,
            "platform": options.platform,
            "echo": echo.host_str(),
            "echo_from": source,
            "passed": passed,
        })
    };
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            session
                .audit(
                    "selftest.http",
                    Some(proxy.id.to_string()),
                    AuditOutcome::Failure,
                    details(None),
                )
                .await;
            return Err(error).context("the self-test could not be judged");
        }
    };
    session
        .audit(
            "selftest.http",
            Some(proxy.id.to_string()),
            if report.passed {
                AuditOutcome::Success
            } else {
                AuditOutcome::Failure
            },
            details(Some(report.passed)),
        )
        .await;
    if !report.passed {
        let differing: Vec<&str> = report
            .findings
            .iter()
            .filter(|finding| !finding.passed)
            .map(|finding| finding.check)
            .collect();
        session.alerts.raise(
            Alert::new(
                AlertKind::HttpSelftestFailed,
                AlertSeverity::Warning,
                format!(
                    "the http fingerprint of {} on {} differs from the recorded one in: {}",
                    report.profile,
                    report.os,
                    differing.join(", ")
                ),
                json!({ "profile": report.profile, "os": report.os, "differing": differing }),
            )
            .about(proxy.id),
        );
    }

    write_report(out, options, &proxy.id, source, &report).context("cannot write the result")?;
    if !report.passed {
        bail!("the fingerprint differs from the recorded one");
    }
    Ok(())
}

fn write_report(
    out: &mut impl Write,
    options: &HttpSelftestOptions,
    proxy: &ProxyId,
    source: &str,
    report: &SelftestReport,
) -> anyhow::Result<()> {
    if options.json {
        let mut document = serde_json::to_value(report)?;
        if let Some(object) = document.as_object_mut() {
            object.insert("proxy".to_owned(), json!(proxy));
            object.insert("echo_from".to_owned(), json!(source));
        }
        serde_json::to_writer_pretty(&mut *out, &document)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(
        out,
        "{} on {} through proxy {proxy}, asked {} ({source})",
        report.profile, report.os, report.echo
    )?;
    let mut rows = Vec::new();
    for finding in &report.findings {
        if finding.passed {
            rows.push(vec![
                finding.check.to_owned(),
                "ok".to_owned(),
                finding.observed.clone(),
            ]);
        } else {
            rows.push(vec![
                finding.check.to_owned(),
                "DIFFERS".to_owned(),
                format!("recorded: {}", finding.expected),
            ]);
            rows.push(vec![
                String::new(),
                String::new(),
                format!("observed: {}", finding.observed),
            ]);
        }
    }
    table(out, &["CHECK", "RESULT", "VALUE"], &rows)?;
    let timing = &report.timing;
    let phase = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |ms| format!("{ms} ms"));
    writeln!(
        out,
        "timing: tunnel {}, handshake {}, first byte {}, total {} ms",
        phase(timing.connect_ms),
        phase(timing.tls_ms),
        phase(timing.ttfb_ms),
        timing.total_ms
    )?;
    if let Some(peer) = &report.observed.peer {
        writeln!(out, "the echo saw the request come from {peer}")?;
    }
    writeln!(
        out,
        "{}",
        if report.passed {
            "passed: the fingerprint is the recorded one"
        } else {
            "FAILED: the fingerprint differs from the recorded one"
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kage_core::config::Config;
    use kage_core::domain::Timing;
    use kage_core::ids::ProxyId;
    use kage_http::selftest::{EchoKind, Finding, Observed, SelftestReport};

    use super::{HttpSelftestOptions, echo_of, write_report};

    fn config(extra: &[(&str, &str)]) -> Config {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        pairs.extend_from_slice(extra);
        Config::load(
            None,
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned())),
        )
        .unwrap()
    }

    #[test]
    fn the_echo_is_the_named_one_then_the_operators_then_the_third_party() {
        let plain = config(&[]);
        let (url, source) = echo_of(&plain, None).unwrap();
        assert_eq!(url.as_str(), "https://tls.peet.ws/api/all");
        assert_eq!(source, "selftest.http.third_party_url");

        let own = config(&[(
            "KAGE__SELFTEST__HTTP__ECHO_URL",
            "https://echo.example.org/",
        )]);
        let (url, source) = echo_of(&own, None).unwrap();
        assert_eq!(url.as_str(), "https://echo.example.org/");
        assert_eq!(source, "selftest.http.echo_url");

        let (url, source) = echo_of(&own, Some("https://other.example.net/x")).unwrap();
        assert_eq!(url.as_str(), "https://other.example.net/x");
        assert_eq!(source, "--echo");

        assert!(echo_of(&plain, Some("not a url")).is_err());
        let error = echo_of(&plain, Some("http://echo.example.org/")).unwrap_err();
        assert!(error.to_string().contains("https"), "{error}");
    }

    fn report(passed: bool) -> SelftestReport {
        let finding = |check, expected: &str, observed: &str| Finding {
            check,
            expected: expected.to_owned(),
            observed: observed.to_owned(),
            passed: expected == observed,
        };
        SelftestReport {
            profile: "chrome_149".to_owned(),
            os: "windows".to_owned(),
            echo: "https://tls.peet.ws/api/all".to_owned(),
            passed,
            findings: vec![
                finding("http_version", "h2", "h2"),
                finding(
                    "ja4",
                    "t13d1516h2_aaa",
                    if passed {
                        "t13d1516h2_aaa"
                    } else {
                        "t13d1516h2_bbb"
                    },
                ),
            ],
            observed: Observed {
                source: EchoKind::Peet,
                http_version: "h2".to_owned(),
                ja4: "t13d1516h2_aaa".to_owned(),
                akamai: None,
                headers: Vec::new(),
                peer: Some("203.0.113.20".to_owned()),
            },
            timing: Timing {
                connect_ms: Some(320),
                tls_ms: Some(140),
                ttfb_ms: Some(700),
                total_ms: 720,
                ..Timing::default()
            },
        }
    }

    fn options(json: bool) -> HttpSelftestOptions {
        HttpSelftestOptions {
            proxy: ProxyId::new(),
            profile: "chrome_149".to_owned(),
            platform: "windows".to_owned(),
            echo: None,
            json,
        }
    }

    #[test]
    fn a_passing_and_a_failing_report_read_differently() {
        let proxy = ProxyId::new();
        let render = |passed: bool| {
            let mut out = Vec::new();
            write_report(&mut out, &options(false), &proxy, "--echo", &report(passed)).unwrap();
            String::from_utf8(out).unwrap()
        };

        let passing = render(true);
        assert!(passing.starts_with(&format!(
            "chrome_149 on windows through proxy {proxy}, asked https://tls.peet.ws/api/all (--echo)"
        )));
        assert!(
            passing.contains("ja4           ok      t13d1516h2_aaa"),
            "{passing}"
        );
        assert!(
            passing.contains(
                "timing: tunnel 320 ms, handshake 140 ms, first byte 700 ms, total 720 ms"
            ),
            "{passing}"
        );
        assert!(passing.contains("come from 203.0.113.20"));
        assert!(
            passing
                .trim_end()
                .ends_with("passed: the fingerprint is the recorded one")
        );

        let failing = render(false);
        assert!(failing.contains("DIFFERS"), "{failing}");
        assert!(failing.contains("recorded: t13d1516h2_aaa"), "{failing}");
        assert!(failing.contains("observed: t13d1516h2_bbb"), "{failing}");
        assert!(
            failing
                .trim_end()
                .ends_with("FAILED: the fingerprint differs from the recorded one")
        );
    }

    #[test]
    fn the_json_report_names_the_proxy_and_the_source_of_the_echo() {
        let proxy = ProxyId::new();
        let mut out = Vec::new();

        write_report(
            &mut out,
            &options(true),
            &proxy,
            "selftest.http.echo_url",
            &report(true),
        )
        .unwrap();

        let document: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(document["proxy"], proxy.to_string());
        assert_eq!(document["echo_from"], "selftest.http.echo_url");
        assert_eq!(document["passed"], true);
        assert_eq!(document["findings"][1]["check"], "ja4");
        assert_eq!(document["timing"]["connect_ms"], 320);
        assert_eq!(document["observed"]["source"], "peet");
    }
}
