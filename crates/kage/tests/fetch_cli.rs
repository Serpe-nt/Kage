//! `kage fetch`, as the real binary against a Postgres container, mock
//! sidecars, stand-in provider proxies and a site behind them. The
//! personas are made by a manager in this process, as the server would;
//! the binary then works on the same database, with a forwarder and a
//! scheduler of its own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

// The lab of kage-scheduler's tests: the throwaway database, the provider
// proxies, the mock sidecars and the site.
#[path = "../../kage-scheduler/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Utc};
use common::lab::{FAST, Lab, MASTER_KEY, SITE, article};
use kage_core::domain::{AuditOutcome, Exit};
use kage_core::ids::RequestId;
use kage_core::outcome::Outcome;
use kage_proxy::machine::{ExitObservation, ProbeReport};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The binary on the lab's database, sidecars and policies.
struct Cli {
    env: Vec<(String, String)>,
    config: PathBuf,
}

impl Cli {
    fn new(lab: &Lab, dir: &Path) -> Self {
        let config = dir.join("kage.yaml");
        std::fs::write(
            &config,
            format!(
                "sidecar:\n  chromium_url: {}\n  firefox_url: {}\n\
                 engines:\n  chromium:\n    browser_major: 149\n  firefox:\n    browser_major: 156\n\
                 personas:\n  warmup:\n    enabled: false\n\
                 scheduler:\n  retry_pause: {{ min: 50ms, median: 100ms, p95: 200ms }}\n  \
                 visit_start_spacing: {{ min: 10ms, median: 20ms, p95: 40ms }}\n\
                 forwarder:\n  bind: 127.0.0.1:8899\n\
                 artifacts:\n  dir: {}\n",
                lab.chromium.url(),
                lab.firefox.url(),
                // What a fetch keeps for the cache goes below the test's
                // own directory.
                dir.join("artifacts").display(),
            ),
        )
        .unwrap();
        let env = [
            ("KAGE__DATABASE__URL", lab.db.url.as_str()),
            ("KAGE__SIDECAR__TOKEN", "token"),
            ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
            ("KAGE__POLICIES__DIR", lab.policy_dir().to_str().unwrap()),
            ("RUST_LOG", "warn"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        Self { env, config }
    }

    /// Runs the binary to its end without blocking the runtime that serves
    /// the mock sidecars, the proxies and the site.
    async fn run(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kage"));
        command.env_clear();
        // Coverage runs collect profiles from the child process through this.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command.envs(self.env.iter().map(|(name, value)| (name, value)));
        command.arg("--config").arg(&self.config).args(arguments);
        tokio::task::spawn_blocking(move || command.output().expect("cannot run the kage binary"))
            .await
            .unwrap()
    }

    /// Runs a fetch with `--json` that must succeed, and returns the
    /// document on its stdout.
    async fn json(&self, arguments: &[&str]) -> Value {
        let mut all = vec!["fetch"];
        all.extend_from_slice(arguments);
        all.push("--json");
        let output = self.run(&all).await;
        assert!(
            output.status.success(),
            "kage {all:?} failed:\n{}\n{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        let stdout = text(&output.stdout);
        let parsed = serde_json::from_str(&stdout);
        assert!(parsed.is_ok(), "stdout is not one JSON document:\n{stdout}");
        parsed.unwrap()
    }
}

#[tokio::test]
async fn fetch_runs_one_request_through_the_whole_pipeline() {
    let mut lab = Lab::start(2, 149, FAST).await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("The shop")),
        )
        .mount(&lab.site)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::new(&lab, dir.path());
    let url = format!("http://{SITE}/page");
    let epoch = DateTime::<Utc>::UNIX_EPOCH;

    // Level 1: one attempt on the http path, by a persona of the pool,
    // through the command's own forwarder.
    let fetched = cli.json(&[&url, "--level", "1"]).await;
    assert_eq!(fetched["outcome"], "ok", "{fetched}");
    assert_eq!(fetched["status"], 200);
    assert_eq!(fetched["final_url"], url);
    let attempts = fetched["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["render"], "http");
    assert_eq!(attempts[0]["exit"], Value::Null);
    assert_eq!(attempts[0]["outcome"], "ok");
    assert_eq!(attempts[0]["seed"].as_str().unwrap().len(), 64);
    assert_eq!(fetched["pacing"]["applied"], "fast");
    assert_eq!(fetched["document_bytes"], article("The shop").len());
    let persona = lab
        .personas
        .iter()
        .find(|persona| json!(persona.id) == attempts[0]["persona_id"])
        .expect("a persona of the pool made the attempt")
        .clone();
    assert_eq!(fetched["persona"]["id"], json!(persona.id));
    // The site saw the persona, through the persona's own proxy.
    let received = lab.received("/page").await;
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].headers.get("user-agent").unwrap(),
        persona.user_agent.as_str()
    );
    assert_eq!(lab.proxy_of(&persona).plain().len(), 1);
    // What the command's forwarder relayed is what the provider's proxy
    // carried, and it is written to the store when the command ends.
    let relayed = (
        fetched["relayed_bytes"]["up"].as_u64().unwrap(),
        fetched["relayed_bytes"]["down"].as_u64().unwrap(),
    );
    assert_eq!(relayed, lab.proxy_of(&persona).bytes());
    let total = lab
        .store()
        .bandwidth()
        .total_since(epoch, Exit::Pool)
        .await
        .unwrap();
    assert_eq!((total.bytes_up, total.bytes_down), relayed);
    // The request, its attempt and its result are in the store, and the
    // command is in the audit log.
    let request: RequestId = serde_json::from_value(fetched["request_id"].clone()).unwrap();
    let stored = lab.store().requests().get(request).await.unwrap().unwrap();
    assert_eq!(stored.outcome, Some(Outcome::Ok));
    assert_eq!(stored.spec["source"], "cli");
    assert_eq!(stored.api_key_id, None);
    assert_eq!(
        lab.store()
            .attempts()
            .list_for_request(request)
            .await
            .unwrap()
            .len(),
        1
    );
    let audited = lab.store().audit_events().list(None, 10).await.unwrap();
    let event = audited
        .iter()
        .find(|event| event.action == "fetch")
        .expect("the fetch is audited");
    assert_eq!(event.outcome, AuditOutcome::Success);
    assert_eq!(event.target.as_deref(), Some(url.as_str()));

    // In the browser, by the persona asked for: the page is loaded with
    // the behavior of a person, the document is written where asked, and
    // the cookies the browser holds go to the persona's http jar.
    let pinned = lab.personas[1].clone();
    let document = dir.path().join("page.html");
    let rendered = cli
        .json(&[
            &url,
            "--level",
            "3",
            "--persona",
            &pinned.id.to_string(),
            "--timeout",
            "40s",
            "--output",
            document.to_str().unwrap(),
        ])
        .await;
    assert_eq!(rendered["outcome"], "ok", "{rendered}");
    assert_eq!(rendered["attempts"][0]["render"], "browser");
    assert_eq!(rendered["attempts"][0]["persona_id"], json!(pinned.id));
    assert_eq!(rendered["handoff"]["cookies_to_browser"], 0);
    assert_eq!(rendered["handoff"]["cookies_to_http"], 0);
    assert!(rendered["behavior"]["dwell_ms"].is_u64(), "{rendered}");
    assert_eq!(
        std::fs::read_to_string(&document).unwrap(),
        article("A page")
    );
    let (opened, navigated, closed) = lab.chromium.with(|sidecar| {
        (
            sidecar.opened.clone(),
            sidecar.navigated.clone(),
            sidecar.closed.len(),
        )
    });
    // The sidecar was handed the command's forwarder under the persona's
    // credential, and asked for a human visit with the attempt's seed.
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["proxy"]["username"], pinned.id.to_string());
    let server = opened[0]["proxy"]["server"].as_str().unwrap();
    assert!(server.starts_with("http://kage:"), "{server}");
    assert_ne!(server, "http://kage:8899");
    assert_eq!(navigated.len(), 1);
    assert_eq!(navigated[0]["timeout_ms"], 40_000);
    assert_eq!(navigated[0]["behavior"]["dwell"], "human");
    assert_eq!(
        navigated[0]["behavior"]["seed"],
        rendered["attempts"][0]["seed"]
    );
    // The command closes the browser it opened before it ends.
    assert_eq!(closed, 1);

    // In sentences, for a person.
    let output = cli.run(&["fetch", &url, "--level", "1"]).await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sentences = text(&output.stdout);
    assert!(
        sentences.starts_with(&format!("ok  200  {url}\n")),
        "{sentences}"
    );
    assert!(sentences.contains("attempts:\n  1  http "), "{sentences}");
    assert!(sentences.contains("pacing: fast, waited "), "{sentences}");

    // A page that is not fetched: what was found is shown, and the
    // command fails.
    lab.chromium.with(|sidecar| {
        sidecar.next_pages.push_back(json!({
            "status": 403,
            "dom_html": "<html><body><h1>Sorry, you have been blocked</h1>\
                         <p>Cloudflare Ray ID: 8f1</p></body></html>",
        }));
    });
    let blocked = cli
        .run(&["fetch", &url, "--level", "3", "--retries", "0"])
        .await;
    assert!(!blocked.status.success());
    let shown = text(&blocked.stdout);
    assert!(shown.starts_with("forbidden  403  "), "{shown}");
    assert!(shown.contains("block: cloudflare waf_block"), "{shown}");
    assert!(
        text(&blocked.stderr).contains(&format!("{url} was not fetched: forbidden")),
        "{}",
        text(&blocked.stderr)
    );

    // Nothing is fetched while a leak is on record: the command says so
    // without an attempt.
    let report = ProbeReport {
        at: lab.now(),
        exit: Ok(ExitObservation {
            ip: "192.0.2.77".parse().unwrap(),
            latency_ms: 3,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(true),
        geo: None,
    };
    lab.pool
        .record_probe(lab.personas[0].proxy_id, &report)
        .await
        .unwrap();
    let refused = cli.run(&["fetch", &url, "--level", "1", "--json"]).await;
    assert!(!refused.status.success());
    let document: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(document["outcome"], "provider_error");
    assert_eq!(document["reason"], "egress_unsafe");
    assert_eq!(document["attempts"], json!([]));
    assert!(
        text(&refused.stderr).contains("was not fetched: egress_unsafe"),
        "{}",
        text(&refused.stderr)
    );

    // What cannot be accepted is an error before anything starts.
    let unknown = cli
        .run(&["fetch", &url, "--level", "1", "--pacing", "warp"])
        .await;
    assert!(!unknown.status.success());
    assert!(
        text(&unknown.stderr).contains("the request could not be accepted"),
        "{}",
        text(&unknown.stderr)
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn fetch_at_level_zero_is_a_plain_client_that_names_itself() {
    let mut lab = Lab::start(2, 149, FAST).await;
    Mock::given(method("GET"))
        .and(path("/plain"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("Plain")),
        )
        .mount(&lab.site)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::new(&lab, dir.path());
    let url = format!("http://{SITE}/plain");

    // Nobody's: no persona, the site's plain exit, and a client that says
    // it is Kage.
    let fetched = cli.json(&[&url, "--level", "0"]).await;
    assert_eq!(fetched["outcome"], "ok", "{fetched}");
    assert_eq!(fetched["persona"], Value::Null);
    let attempts = fetched["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["exit"], "pool");
    assert_eq!(attempts[0]["render"], "http");
    assert_eq!(attempts[0]["persona_id"], Value::Null);
    assert_eq!(attempts[0]["engine"], Value::Null);
    let received = lab.received("/plain").await;
    assert_eq!(received.len(), 1);
    let agent = received[0]
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(agent.starts_with("kage/"), "{agent}");
    let request: RequestId = serde_json::from_value(fetched["request_id"].clone()).unwrap();
    let stored = lab.store().requests().get(request).await.unwrap().unwrap();
    assert_eq!(stored.spec["level"], 0);

    // A plain client has no persona to name.
    let persona = lab.personas[0].id.to_string();
    let refused = cli
        .run(&["fetch", &url, "--level", "0", "--persona", &persona])
        .await;
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("--persona is not taken at level 0"),
        "{}",
        text(&refused.stderr)
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn fetch_returns_what_the_page_says_and_the_cache_answers_the_second_time() {
    let mut lab = Lab::start(2, 149, FAST).await;
    Mock::given(method("GET"))
        .and(path("/read"))
        .respond_with(
            // The type is set with the body: `set_body_string` after a
            // header would answer `text/plain`, and what is no HTML
            // document has no extraction.
            ResponseTemplate::new(200)
                .set_body_raw(article("The reading room"), "text/html; charset=utf-8"),
        )
        .mount(&lab.site)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::new(&lab, dir.path());
    let url = format!("http://{SITE}/read");
    let page = article("The reading room");

    // Fetched, read and kept: what the page says is in the answer, and
    // the site was asked once.
    let first_file = dir.path().join("first.html");
    let asked = [
        url.as_str(),
        "--level",
        "1",
        "--extract",
        "--cache-ttl",
        "10m",
        "--output",
        first_file.to_str().unwrap(),
    ];
    let first = cli.json(&asked).await;
    assert_eq!(first["outcome"], "ok", "{first}");
    assert_eq!(first["cached"], false);
    assert_eq!(first["cache_ttl_s"], 600);
    assert_eq!(first["extract"]["title"], "The reading room", "{first}");
    assert_eq!(first["extract"]["thin_content"], false);
    assert!(
        first["extract"]["word_count"].as_u64().unwrap() >= 60 * 14,
        "{first}"
    );
    assert!(
        first["extract"]["markdown"]
            .as_str()
            .unwrap()
            .contains("The council met on Tuesday"),
        "{first}"
    );
    // Nobody asks this site for its robots.txt, and the answer says so.
    assert_eq!(
        first["robots"],
        json!({ "fetched": false, "allowed": true, "matched_rule": null, "crawl_delay": null })
    );
    assert_eq!(std::fs::read_to_string(&first_file).unwrap(), page);
    assert_eq!(lab.received("/read").await.len(), 1);

    // The same command again is answered from the cache: no attempt,
    // nothing relayed, nothing asked of the site, and the document is the
    // one that was stored.
    let again_file = dir.path().join("again.html");
    let again = [
        url.as_str(),
        "--level",
        "1",
        "--extract",
        "--cache-ttl",
        "10m",
        "--output",
        again_file.to_str().unwrap(),
    ];
    let second = cli.json(&again).await;
    assert_eq!(second["outcome"], "ok", "{second}");
    assert_eq!(second["cached"], true);
    assert!(second["cached_at"].is_string(), "{second}");
    assert_eq!(second["attempts"], json!([]));
    assert_eq!(second["status"], 200);
    assert_eq!(second["extract"], first["extract"]);
    assert_eq!(second["document_bytes"], page.len());
    assert_eq!(second["relayed_bytes"], json!({ "up": 0, "down": 0 }));
    assert_ne!(second["request_id"], first["request_id"]);
    assert_eq!(std::fs::read_to_string(&again_file).unwrap(), page);
    assert_eq!(lab.received("/read").await.len(), 1);

    // In sentences, for a person.
    let output = cli
        .run(&[
            "fetch",
            &url,
            "--level",
            "1",
            "--extract",
            "--cache-ttl",
            "10m",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sentences = text(&output.stdout);
    assert!(
        sentences.contains("cache: answered from the cache, fetched "),
        "{sentences}"
    );
    assert!(
        sentences.contains("extract: \"The reading room\", "),
        "{sentences}"
    );
    assert!(!sentences.contains("attempts:"), "{sentences}");
    assert_eq!(lab.received("/read").await.len(), 1);

    // A result with other parts is another entry: without the extraction
    // the page is fetched.
    let other = cli
        .json(&[&url, "--level", "1", "--cache-ttl", "10m"])
        .await;
    assert_eq!(other["cached"], false);
    assert_eq!(other["extract"], Value::Null);
    assert_eq!(lab.received("/read").await.len(), 2);

    // What the site's policy does not allow is refused before anything
    // starts: nobody asks this site for its robots.txt, and results are
    // not served for longer than a day.
    for (flag, value, said) in [
        ("--robots", "enforce", "robots_ignored"),
        ("--cache-ttl", "2d", "--cache-ttl is at most 1d"),
    ] {
        let refused = cli.run(&["fetch", &url, "--level", "1", flag, value]).await;
        assert!(!refused.status.success());
        assert!(
            text(&refused.stderr).contains(said),
            "{}",
            text(&refused.stderr)
        );
        assert_eq!(text(&refused.stdout), "");
    }
    assert_eq!(lab.received("/read").await.len(), 2);
    lab.shut_down().await;
}
