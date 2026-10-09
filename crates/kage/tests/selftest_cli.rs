//! `kage selftest http`, `kage selftest browser` and `kage echo serve`, as
//! the real binary against a Postgres container, a local proxy, the
//! fingerprint echo and a stand-in for the browser sidecar.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// Postgres and the master key of kage-proxy's tests.
#[path = "../../kage-proxy/tests/common/mod.rs"]
mod pool_support;

// The proxy with routes, the echo and its certificates of kage-http's tests.
#[path = "../../kage-http/tests/common/mod.rs"]
mod http_support;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use http_support::{
    ECHO_H1_HOST, ECHO_HOST, PROXY_PASS, PROXY_USER, TestCertificates, TestEcho, TestProxy,
};
use pool_support::{MASTER_KEY, TestDb};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn shipped_policies() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies")
}

/// The binary with an empty environment plus `env`.
fn kage(env: &[(String, String)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kage"));
    command.env_clear();
    // Coverage runs collect profiles from the child process through this.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command.envs(env.iter().map(|(name, value)| (name, value)));
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs the binary to its end without blocking the runtime that serves the
/// proxy and the echo.
async fn run(env: &[(String, String)], arguments: &[&str]) -> Output {
    let mut command = kage(env);
    command.args(arguments);
    tokio::task::spawn_blocking(move || command.output().expect("cannot run the kage binary"))
        .await
        .unwrap()
}

async fn ok(env: &[(String, String)], arguments: &[&str]) -> String {
    let output = run(env, arguments).await;
    assert!(
        output.status.success(),
        "kage {arguments:?} failed:\n{}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    text(&output.stdout)
}

async fn count(db: &TestDb, table: &str, filter: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table} WHERE {filter}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(db.store.pool())
        .await
        .unwrap()
}

/// The environment of a Kage whose pool is the one local proxy and whose
/// own echo is reachable through it.
fn environment(db: &TestDb, proxy: &TestProxy, ca_file: &Path) -> Vec<(String, String)> {
    [
        ("KAGE__DATABASE__URL", db.url.clone()),
        ("KAGE__SIDECAR__TOKEN", "sidecar-token-77c1".to_owned()),
        ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY.to_owned()),
        (
            "KAGE__POLICIES__DIR",
            shipped_policies().to_str().unwrap().to_owned(),
        ),
        ("RUST_LOG", "warn".to_owned()),
        ("KAGE__PROXIES__PROVIDER", "static".to_owned()),
        (
            "KAGE__PROXIES__STATIC_LIST",
            format!(
                "{}:{}:{PROXY_USER}:{PROXY_PASS}:DE",
                proxy.addr.ip(),
                proxy.addr.port()
            ),
        ),
        (
            "KAGE__SELFTEST__HTTP__ECHO_URL",
            format!("https://{ECHO_HOST}/"),
        ),
        (
            "KAGE__SELFTEST__HTTP__ECHO_CA_FILE",
            ca_file.to_str().unwrap().to_owned(),
        ),
        ("KAGE__SELFTEST__HTTP__TIMEOUT", "20s".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// Adopts the static proxy and returns its id.
async fn adopt_proxy(env: &[(String, String)]) -> String {
    let listed: Value =
        serde_json::from_str(&ok(env, &["proxy", "list", "--sync", "--json"]).await).unwrap();
    listed["proxies"][0]["id"].as_str().unwrap().to_owned()
}

// One pool and one echo for every way the command can end: each run needs
// the setup before it.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn the_self_test_passes_for_chrome_and_firefox_and_says_when_it_cannot_judge() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let echo = TestEcho::start().await;
    echo.route_through(&proxy);
    let dir = TempDir::new().unwrap();
    let ca_file = dir.path().join("echo-ca.pem");
    std::fs::write(&ca_file, &echo.ca_pem).unwrap();
    let env = environment(&db, &proxy, &ca_file);
    let id = adopt_proxy(&env).await;

    // A table for a person.
    let table = ok(
        &env,
        &[
            "selftest",
            "http",
            "--proxy",
            &id,
            "--profile",
            "chrome_149",
        ],
    )
    .await;
    assert!(
        table.starts_with(&format!(
            "chrome_149 on windows through proxy {id}, asked https://echo.kage.test/ \
             (selftest.http.echo_url)"
        )),
        "{table}"
    );
    for check in [
        "http_version",
        "ja4",
        "akamai",
        "header_order",
        "user_agent",
    ] {
        assert!(
            table
                .lines()
                .any(|line| line.starts_with(check) && line.contains(" ok ")),
            "{check}:\n{table}"
        );
    }
    assert!(
        table.contains("t13d1516h2_8daaf6152771_d8a2da3f94cd"),
        "{table}"
    );
    assert!(table.contains("timing: tunnel "), "{table}");
    assert!(
        table.contains("the echo saw the request come from 127.0.0.1"),
        "{table}"
    );
    assert!(
        table
            .trim_end()
            .ends_with("passed: the fingerprint is the recorded one")
    );
    // It left through the proxy, as one tunnel to the echo's name.
    assert_eq!(proxy.tunnels().len(), 1);
    assert_eq!(proxy.tunnels()[0].target, "echo.kage.test:443");

    // One JSON document for a script.
    let stdout = ok(
        &env,
        &[
            "selftest",
            "http",
            "--proxy",
            &id,
            "--profile",
            "firefox_151",
            "--platform",
            "macos",
            "--json",
        ],
    )
    .await;
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["profile"], "firefox_151");
    assert_eq!(report["os"], "macos");
    assert_eq!(report["proxy"], id);
    assert_eq!(report["echo_from"], "selftest.http.echo_url");
    assert_eq!(report["observed"]["source"], "kage");
    assert_eq!(
        report["observed"]["ja4"],
        "t13d1717h2_5b57614c22b0_3cbfd9057e0d"
    );
    assert!(report["timing"]["connect_ms"].is_u64() && report["timing"]["tls_ms"].is_u64());
    assert_eq!(report["findings"].as_array().unwrap().len(), 5);

    // Both runs are in the audit trail, and neither raised an alert.
    assert_eq!(
        count(
            &db,
            "audit_events",
            "action = 'selftest.http' AND outcome = 'success'"
        )
        .await,
        2
    );
    assert_eq!(
        count(&db, "health_events", "kind = 'http_selftest_failed'").await,
        0
    );

    // What cannot be judged or built fails with the reason.
    for (arguments, expect) in [
        (
            vec!["--proxy", id.as_str(), "--profile", "chrome_120"],
            "no recorded fingerprint values for chrome_120 on windows",
        ),
        (
            vec![
                "--proxy",
                id.as_str(),
                "--profile",
                "firefox_151",
                "--platform",
                "linux",
            ],
            "needs an explicit User-Agent",
        ),
        (
            vec!["--proxy", id.as_str(), "--profile", "chrome_9000"],
            "unknown emulation profile",
        ),
        (
            vec![
                "--proxy",
                id.as_str(),
                "--profile",
                "chrome_149",
                "--platform",
                "amiga",
            ],
            "unknown platform",
        ),
        (
            vec![
                "--proxy",
                id.as_str(),
                "--profile",
                "chrome_149",
                "--echo",
                "http://plain.test/",
            ],
            "must be an https URL",
        ),
        (
            vec![
                "--proxy",
                "0199c2f0-0000-7000-8000-000000000001",
                "--profile",
                "chrome_149",
            ],
            "there is no proxy",
        ),
        (
            vec![
                "--proxy",
                id.as_str(),
                "--profile",
                "chrome_149",
                "--echo",
                "https://gone.test/",
            ],
            "could not be reached",
        ),
    ] {
        let mut all = vec!["selftest", "http"];
        all.extend(arguments);
        let output = run(&env, &all).await;
        assert!(!output.status.success(), "{all:?} succeeded");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(expect), "{all:?}: {stderr}");
        // The proxy's login is in no output.
        assert!(!stderr.contains(PROXY_PASS) && !text(&output.stdout).contains(PROXY_PASS));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fingerprint_that_differs_fails_the_command_and_raises_an_alert() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let echo = TestEcho::start().await;
    echo.route_through(&proxy);
    let dir = TempDir::new().unwrap();
    let ca_file = dir.path().join("echo-ca.pem");
    std::fs::write(&ca_file, &echo.ca_pem).unwrap();
    let env = environment(&db, &proxy, &ca_file);
    let id = adopt_proxy(&env).await;

    // An echo that only offers HTTP/1.1: the request cannot look as
    // recorded, which was recorded over HTTP/2.
    let h1 = format!("https://{ECHO_H1_HOST}/");
    let output = run(
        &env,
        &[
            "selftest",
            "http",
            "--proxy",
            &id,
            "--profile",
            "chrome_149",
            "--echo",
            &h1,
        ],
    )
    .await;

    assert!(!output.status.success());
    let stdout = text(&output.stdout);
    // The findings are shown before the command fails.
    assert!(stdout.contains("http_version  DIFFERS"), "{stdout}");
    assert!(stdout.contains("recorded: h2"), "{stdout}");
    assert!(stdout.contains("observed: http/1.1"), "{stdout}");
    // The handshake is the same whatever the server then picks.
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("ja4") && line.contains(" ok ")),
        "{stdout}"
    );
    assert!(
        stdout.contains("FAILED: the fingerprint differs"),
        "{stdout}"
    );
    assert!(
        text(&output.stderr).contains("the fingerprint differs from the recorded one"),
        "{}",
        text(&output.stderr)
    );
    assert_eq!(
        count(
            &db,
            "audit_events",
            "action = 'selftest.http' AND outcome = 'failure'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db,
            "health_events",
            "kind = 'http_selftest_failed' AND subject_kind = 'proxy' AND severity = 'warning'"
        )
        .await,
        1
    );
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `kage echo serve` as its own process, asked by `kage selftest http`.
#[tokio::test(flavor = "multi_thread")]
async fn the_echo_command_serves_what_the_self_test_command_asks() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let certificates = TestCertificates::generate();
    let dir = TempDir::new().unwrap();
    let (ca_file, cert_file, key_file) = (
        dir.path().join("ca.pem"),
        dir.path().join("cert.pem"),
        dir.path().join("key.pem"),
    );
    std::fs::write(&ca_file, &certificates.ca_pem).unwrap();
    std::fs::write(&cert_file, &certificates.cert_pem).unwrap();
    std::fs::write(&key_file, &certificates.key_pem).unwrap();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");

    // The echo needs neither a database nor a secret.
    let mut command = kage(&[("RUST_LOG".to_owned(), "info".to_owned())]);
    command
        .args(["echo", "serve", "--bind", &address, "--cert"])
        .arg(&cert_file)
        .arg("--key")
        .arg(&key_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let server = Server {
        child: command.spawn().expect("cannot start kage echo serve"),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(&address).is_err() {
        assert!(
            Instant::now() < deadline,
            "the echo did not start listening"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proxy.route(&format!("{ECHO_HOST}:443"), address.parse().unwrap());

    let env = environment(&db, &proxy, &ca_file);
    let id = adopt_proxy(&env).await;
    let stdout = ok(
        &env,
        &[
            "selftest",
            "http",
            "--proxy",
            &id,
            "--profile",
            "chrome_149",
            "--json",
        ],
    )
    .await;
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["passed"], true, "{stdout}");
    assert_eq!(report["observed"]["source"], "kage");
    assert_eq!(
        report["observed"]["akamai"],
        "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p"
    );
    drop(server);

    // Without its files it says which one is missing.
    let output = run(
        &[],
        &[
            "echo",
            "serve",
            "--bind",
            &address,
            "--cert",
            "/nonexistent.pem",
            "--key",
            "/k.pem",
        ],
    )
    .await;
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("cannot read the certificate"));
}

/// The contract's example of a schema, as the sidecar would send it.
fn contract_example(schema: &str) -> Value {
    let examples: Value = serde_json::from_str(kage_browser::contract::EXAMPLES_JSON).unwrap();
    examples[schema][0].clone()
}

const CONTEXT: &str = "0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f";

/// A stand-in for the Chromium sidecar: it answers one self-test
/// conversation with the contract's examples and `report`, and only to
/// calls that carry the shared secret. `context` is what the request that
/// opens the context must say.
async fn sidecar(context: Value, report: Value) -> MockServer {
    let server = MockServer::start().await;
    let authorized = || header("x-kage-sidecar-token", "sidecar-token-77c1");
    Mock::given(method("POST"))
        .and(path("/v1/personas/generate"))
        .and(authorized())
        .and(body_partial_json(json!({ "claimed_os": "linux" })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(contract_example("GeneratePersonaResponse")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/contexts"))
        .and(authorized())
        .and(body_partial_json(context))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(contract_example("OpenContextResponse")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/selftest")))
        .and(authorized())
        // The third-party pages of the configuration, and the capture
        // recorded for the browser the sidecar says it runs.
        .and(body_partial_json(json!({
            "pages": [
                { "id": "sannysoft", "kind": "sannysoft", "url": "https://bot.sannysoft.com/" },
                {
                    "id": "creepjs",
                    "kind": "creepjs",
                    "url": "https://abrahamjuliot.github.io/creepjs/",
                },
            ],
            "expected": {
                "claimed_os": "linux",
                "browser_major": 155,
                "timezone": "Europe/Berlin",
            },
            "echo": { "ja4": "t13d1517h2_8daaf6152771_cb7bf5808d99" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(report))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/contexts/{CONTEXT}")))
        .and(authorized())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_self_test_opens_a_context_through_the_proxy_and_closes_it_again() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let dir = TempDir::new().unwrap();
    let mut env = environment(&db, &proxy, &dir.path().join("unused.pem"));
    env.retain(|(name, _)| !name.starts_with("KAGE__SELFTEST__HTTP__ECHO"));
    let id = adopt_proxy(&env).await;
    let arguments = [
        "selftest",
        "browser",
        "--proxy",
        id.as_str(),
        "--timezone",
        "Europe/Berlin",
        "--language",
        "de-DE",
        "--language",
        "de",
    ];
    // The proxy with its credentials, the proxy's own profile directory and
    // the identity that was asked for.
    let context = json!({
        "persona_id": id,
        "profile_dir": format!("selftest/{id}"),
        "proxy": {
            "server": format!("http://{}:{}", proxy.addr.ip(), proxy.addr.port()),
            "username": PROXY_USER,
            "password": PROXY_PASS,
        },
        "locale": "de-DE",
        "languages": ["de-DE", "de"],
        "timezone": "Europe/Berlin",
    });

    // Without --third-party there is nothing to load, and the sidecar is
    // not asked at all: nothing listens at its default address here.
    let output = run(&env, &arguments).await;
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("--third-party"));

    // A passing report: one JSON document, and the context is closed.
    let passing = sidecar(context.clone(), contract_example("SelftestResponse")).await;
    env.push(("KAGE__SIDECAR__CHROMIUM_URL".to_owned(), passing.uri()));
    let with_pages = [&arguments[..], &["--third-party"]].concat();
    // With a pause before the pages: the context stays open that long.
    let started = Instant::now();
    let report: Value = serde_json::from_str(
        &ok(
            &env,
            &[&with_pages[..], &["--json", "--idle-before", "2s"]].concat(),
        )
        .await,
    )
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(report["engine"], "chromium");
    assert_eq!(report["proxy"], id.as_str());
    assert_eq!(report["persona"]["browser_major"], 155);
    assert_eq!(report["passed"], true);
    assert_eq!(report["findings"].as_array().unwrap().len(), 2);
    // The stand-in persona of a proxy: its seed is the proxy's id, twice.
    let seed = id.replace('-', "");
    assert_eq!(report["seed"], format!("{seed}{seed}"));
    passing.verify().await;
    drop(passing);

    // A failing report still prints what was found, as a table, and the
    // command fails. The context is closed all the same.
    let mut failed = contract_example("SelftestResponse");
    failed["passed"] = Value::Bool(false);
    failed["score"] = json!(40);
    failed["findings"][0]["status"] = json!("fail");
    failed["findings"][0]["observed"] = json!("true");
    let failing = sidecar(context, failed).await;
    env.pop();
    env.push(("KAGE__SIDECAR__CHROMIUM_URL".to_owned(), failing.uri()));
    let output = run(&env, &with_pages).await;
    assert!(!output.status.success());
    let table = text(&output.stdout);
    assert!(table.contains("navigator.webdriver"), "{table}");
    assert!(table.contains("FAIL"), "{table}");
    assert!(table.contains("score 40.0 of 100"), "{table}");
    assert!(text(&output.stderr).contains("the browser self-test failed"));
    failing.verify().await;

    // Both runs are in the audit log with their outcome, and the proxy's
    // password is in none of the output.
    for (outcome, expected) in [("success", 1), ("failure", 1)] {
        let filter = format!("action = 'selftest.browser' AND outcome = '{outcome}'");
        assert_eq!(
            count(&db, "audit_events", &filter).await,
            expected,
            "{outcome}"
        );
    }
    assert!(!table.contains(PROXY_PASS));
    assert!(!report.to_string().contains(PROXY_PASS));
}
