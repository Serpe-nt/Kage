//! `kage proxy list|health|replace|egress` and the pool inside `kage serve`,
//! as the real binary against a Postgres container, a mock of the Webshare
//! API, a local echo and local proxies.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// The mock Webshare API, the echo and the test proxy of kage-proxy's tests.
#[path = "../../kage-proxy/tests/common/mod.rs"]
mod common;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{
    Echo, GeoFixture, MASTER_KEY, MockWebshare, PROXY_PASS, PROXY_USER, TestDb, TestProxy,
    WEBSHARE_TOKEN, loopback,
};
use serde_json::Value;

const TOKEN: &str = "sidecar-token-77c1";

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
/// mock API, the echo and the proxies.
async fn run(env: &[(String, String)], arguments: &[&str]) -> Output {
    let mut command = kage(env);
    command.args(arguments);
    tokio::task::spawn_blocking(move || command.output().expect("cannot run the kage binary"))
        .await
        .unwrap()
}

/// Runs a command that must succeed and returns its stdout.
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

/// Runs a command with `--json` and returns the document on its stdout.
async fn json(env: &[(String, String)], arguments: &[&str]) -> Value {
    let mut all = arguments.to_vec();
    all.push("--json");
    let stdout = ok(env, &all).await;
    let parsed = serde_json::from_str(&stdout);
    assert!(parsed.is_ok(), "stdout is not one JSON document:\n{stdout}");
    parsed.unwrap()
}

fn base_env(db: &TestDb) -> Vec<(String, String)> {
    [
        ("KAGE__DATABASE__URL", db.url.as_str()),
        ("KAGE__SIDECAR__TOKEN", TOKEN),
        ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
        ("KAGE__POLICIES__DIR", shipped_policies().to_str().unwrap()),
        ("RUST_LOG", "warn"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect()
}

/// Points the health checks at the local echo.
fn echo_env(echo: &Echo) -> Vec<(String, String)> {
    vec![
        (
            "KAGE__PROXIES__HEALTH__ECHO_URL".to_owned(),
            echo.url("/ip"),
        ),
        (
            "KAGE__PROXIES__HEALTH__CONFIRM_URL".to_owned(),
            echo.url("/confirm"),
        ),
        (
            "KAGE__PROXIES__HEALTH__GEO_URL".to_owned(),
            echo.url("/json"),
        ),
        (
            "KAGE__PROXIES__HEALTH__HOST_IP_URL".to_owned(),
            echo.url("/host"),
        ),
        ("KAGE__PROXIES__HEALTH__TIMEOUT".to_owned(), "5s".to_owned()),
    ]
}

/// The number of rows in `table` that match `filter`. Both are written in
/// this file, never taken from outside.
async fn count(db: &TestDb, table: &str, filter: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table} WHERE {filter}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(db.store.pool())
        .await
        .unwrap()
}

// One pool followed from empty to replaced: each step needs the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_commands_take_a_pool_from_empty_to_replaced() {
    let db = TestDb::migrated().await;
    let mock = MockWebshare::start().await;
    let echo = Echo::start().await;
    let first = TestProxy::with_exit(2).await;
    let second = TestProxy::with_exit(3).await;
    echo.set_geo(loopback(2), GeoFixture::berlin());
    echo.set_geo(loopback(3), GeoFixture::berlin());
    mock.with(|state| {
        state.add_local_proxy(&first, "DE");
        state.add_local_proxy(&second, "DE");
        // The provider finishes a replacement at the first status read.
        state.polls_to_complete = 0;
    });
    let mut env = base_env(&db);
    env.extend(echo_env(&echo));
    env.push((
        "KAGE__WEBSHARE__API_KEY".to_owned(),
        WEBSHARE_TOKEN.to_owned(),
    ));
    env.push(("KAGE__WEBSHARE__BASE_URL".to_owned(), mock.base_url()));
    env.push((
        "KAGE__PROXIES__REPLACEMENTS_PER_MONTH".to_owned(),
        "1".to_owned(),
    ));

    // Nothing is known before the first sync.
    let empty = json(&env, &["proxy", "list"]).await;
    assert_eq!(empty["proxies"], serde_json::json!([]));
    assert_eq!(empty["summary"]["live"], 0);
    assert!(empty["account"].is_null());
    mock.with(|state| assert_eq!(state.seen.len(), 0));

    // The sync adopts the provider's list.
    let synced = json(&env, &["proxy", "list", "--sync"]).await;
    let proxies = synced["proxies"].as_array().unwrap();
    assert_eq!(proxies.len(), 2);
    for proxy in proxies {
        assert_eq!(proxy["state"], "new");
        assert_eq!(proxy["provider"], "webshare");
        assert_eq!(proxy["country"], "DE");
        assert!(proxy["last_health"].is_null());
        // No credential, not even the user name, unless it is asked for.
        assert!(proxy.get("username").is_none() && proxy.get("password").is_none());
    }
    assert_eq!(synced["account"]["replacements_available"], 10);
    assert_eq!(synced["summary"]["replacements_per_month"], 1);
    let ids: Vec<String> = proxies
        .iter()
        .map(|proxy| proxy["id"].as_str().unwrap().to_owned())
        .collect();

    // Health: every proxy with latency, exit address and country.
    let health = json(&env, &["proxy", "health"]).await;
    let results = health["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for (result, exit) in results.iter().zip(["127.0.0.2", "127.0.0.3"]) {
        assert_eq!(result["verdict"], "ok");
        assert_eq!(result["state"], "healthy");
        assert_eq!(result["last_health"]["exit_ip"], exit);
        assert!(result["last_health"]["latency_ms"].as_u64().is_some());
        assert_eq!(result["last_health"]["leak_detected"], false);
        assert_eq!(result["observed_country"], "DE");
        assert_eq!(result["last_health"]["geo"]["timezone"], "Europe/Berlin");
        assert_eq!(result["asn"], "AS3320");
    }
    assert_eq!(health["host_address_known"], true);
    assert_eq!(health["summary"]["usable"], 2);

    // The same as tables, for a person.
    let table = ok(&env, &["proxy", "health", "--no-sync"]).await;
    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("ID "), "{table}");
    for column in ["RESULT", "LATENCY", "EXIT IP", "COUNTRY", "STATE"] {
        assert!(lines[0].contains(column), "{table}");
    }
    assert!(
        lines[1].contains("ok") && lines[1].contains("127.0.0.2"),
        "{table}"
    );
    assert!(
        lines[2].contains("healthy") && lines[2].contains("DE"),
        "{table}"
    );
    assert!(
        lines[3].starts_with("2 checked; 2 live, 2 usable (2 healthy)"),
        "{table}"
    );
    let listed = ok(&env, &["proxy", "list"]).await;
    assert!(
        listed.contains("CHECKED (UTC)") && !listed.contains("USERNAME"),
        "{listed}"
    );
    assert!(
        listed.contains("replacements this month: 0 of 1; egress: safe"),
        "{listed}"
    );
    assert!(
        !listed.contains(PROXY_USER) && !listed.contains(PROXY_PASS),
        "{listed}"
    );

    // The user name is shown only on request, and the request is audited.
    let revealed = json(&env, &["proxy", "list", "--reveal-username"]).await;
    for proxy in revealed["proxies"].as_array().unwrap() {
        assert_eq!(proxy["username"], PROXY_USER);
        assert!(proxy.get("password").is_none());
    }
    assert!(
        ok(&env, &["proxy", "list", "--reveal-username"])
            .await
            .contains("USERNAME")
    );
    assert_eq!(
        count(
            &db,
            "audit_events",
            "action = 'proxy.reveal_username' AND actor = 'cli'"
        )
        .await,
        2
    );
    // At rest it is ciphertext, like the password.
    let stored: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as("SELECT username, password FROM proxies")
        .fetch_all(db.store.pool())
        .await
        .unwrap();
    for (username, password) in &stored {
        let holds = |bytes: &[u8], needle: &str| {
            bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
        };
        assert!(!holds(username, PROXY_USER) && !holds(password, PROXY_PASS));
    }

    // Replace the first proxy: the provider does it, the new address is
    // adopted, and the command says so.
    let replaced = json(&env, &["proxy", "replace", &ids[0]]).await;
    assert_eq!(replaced["requested"], true);
    assert_eq!(replaced["finished"], true);
    assert_eq!(replaced["proxy"]["state"], "retired");
    assert_eq!(replaced["proxy"]["replacement"]["reason"], "operator");
    assert_eq!(replaced["proxy"]["replacement"]["ticket"], "98315");
    assert!(
        replaced["message"]
            .as_str()
            .unwrap()
            .contains("the new address is in the pool")
    );
    assert_eq!(replaced["summary"]["replacements_this_month"], 1);
    mock.with(|state| {
        assert_eq!(state.count(Some("POST"), "/api/v3/proxy/replace/"), 1);
        assert_eq!(state.replacements_used, 1);
    });
    let all = json(&env, &["proxy", "list", "--all"]).await;
    let states: Vec<&str> = all["proxies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|proxy| proxy["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["retired", "healthy", "new"]);
    assert_eq!(
        json(&env, &["proxy", "list"]).await["proxies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let retired = json(&env, &["proxy", "list", "--state", "retired"]).await;
    assert_eq!(retired["proxies"][0]["id"], ids[0].as_str());

    // The budget of one is spent: the next request is refused, nothing is
    // sent to the provider, and the command fails.
    let refused = run(&env, &["proxy", "replace", &ids[1]]).await;
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stdout)
            .contains("refused: this month's replacement budget is used up (1 of 1)"),
        "{}",
        text(&refused.stdout)
    );
    assert!(text(&refused.stderr).contains("the proxy was not replaced"));
    mock.with(|state| assert_eq!(state.count(Some("POST"), "/api/v3/proxy/replace/"), 1));
    assert_eq!(
        count(
            &db,
            "audit_events",
            "action = 'proxy.replace' AND outcome = 'denied'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db,
            "audit_events",
            "action = 'proxy.replace' AND outcome = 'success'"
        )
        .await,
        1
    );
    // The refusal is an alert, stored for the operator.
    assert_eq!(
        count(
            &db,
            "health_events",
            "kind = 'replacement_budget_exhausted' AND subject_kind = 'proxy'"
        )
        .await,
        1
    );
    // A proxy that does not exist.
    let unknown = run(
        &env,
        &["proxy", "replace", "0199c2f0-0000-7000-8000-000000000001"],
    )
    .await;
    assert_eq!(unknown.status.code(), Some(1));
    assert!(text(&unknown.stderr).contains("there is no proxy 0199c2f0"));

    // The second proxy starts leaving through the host's own address.
    assert_eq!(
        json(&env, &["proxy", "egress"]).await["egress_unsafe"],
        false
    );
    second.set_exit(loopback(1));
    let leaking = ok(&env, &["proxy", "health", "--no-sync", "--id", &ids[1]]).await;
    assert!(
        leaking.contains("LEAK") && leaking.contains("backoff"),
        "{leaking}"
    );
    assert!(leaking.contains("egress: UNSAFE"), "{leaking}");
    let flag = ok(&env, &["proxy", "egress"]).await;
    assert!(
        flag.contains(&format!("leak on record: proxy {}", ids[1])),
        "{flag}"
    );
    assert!(flag.contains("egress: UNSAFE"), "{flag}");
    assert_eq!(
        count(
            &db,
            "health_events",
            "kind = 'egress_unsafe' AND severity = 'critical'"
        )
        .await,
        1
    );
    // The operator fixes the cause and clears the flag.
    second.set_exit(loopback(3));
    let cleared = json(&env, &["proxy", "egress", "--clear"]).await;
    assert_eq!(cleared["egress_unsafe"], false);
    assert_eq!(cleared["cleared"], serde_json::json!([ids[1]]));
    assert!(
        ok(&env, &["proxy", "egress"])
            .await
            .contains("egress: safe")
    );
    assert_eq!(
        count(&db, "audit_events", "action = 'proxy.egress_clear'").await,
        1
    );
    // And the proxy is healthy again at its next check.
    let back = json(&env, &["proxy", "health", "--no-sync", "--id", &ids[1]]).await;
    assert_eq!(back["results"][0]["state"], "healthy");

    // Nothing the commands printed or logged held the password.
    let last = run(&env, &["proxy", "list", "--all", "--reveal-username"]).await;
    assert!(!text(&last.stdout).contains(PROXY_PASS));
    assert!(!text(&last.stderr).contains(PROXY_PASS));
    // Every command that changed something is in the audit trail.
    for action in [
        "proxy.sync",
        "proxy.health",
        "proxy.replace",
        "proxy.egress_clear",
    ] {
        let filter = format!("action = '{action}' AND actor = 'cli'");
        assert!(count(&db, "audit_events", &filter).await >= 1, "{action}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_provider_the_commands_say_what_is_missing() {
    let db = TestDb::migrated().await;
    let env = base_env(&db);

    let listed = ok(&env, &["proxy", "list", "--sync"]).await;
    assert!(listed.contains("0 live, 0 usable (no proxies)"), "{listed}");
    let checked = ok(&env, &["proxy", "health"]).await;
    assert!(checked.contains("0 checked"), "{checked}");
    assert!(
        ok(&env, &["proxy", "egress"])
            .await
            .contains("egress: safe")
    );

    let replace = run(
        &env,
        &["proxy", "replace", "0199c2f0-0000-7000-8000-000000000001"],
    )
    .await;
    assert_eq!(replace.status.code(), Some(1));
    let stderr = text(&replace.stderr);
    assert!(
        stderr.contains("no proxy provider is configured"),
        "{stderr}"
    );
    assert!(stderr.contains("KAGE__WEBSHARE__API_KEY"), "{stderr}");

    // A proxy id that does not exist.
    let health = run(
        &env,
        &[
            "proxy",
            "health",
            "--id",
            "0199c2f0-0000-7000-8000-000000000001",
        ],
    )
    .await;
    assert_eq!(health.status.code(), Some(1));
    assert!(text(&health.stderr).contains("there is no proxy"));

    // An emulation profile this build does not ship is found before any
    // probe is sent.
    let mut wrong_profile = env.clone();
    wrong_profile.push((
        "KAGE__PROXIES__HEALTH__PROFILE".to_owned(),
        "chrome_1".to_owned(),
    ));
    let health = run(&wrong_profile, &["proxy", "health"]).await;
    assert_eq!(health.status.code(), Some(1));
    assert!(text(&health.stderr).contains("chrome_1"));

    // A database that an older version of kage migrated.
    let behind = TestDb::migrated().await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= 8")
        .execute(behind.store.pool())
        .await
        .unwrap();
    let stale = run(&base_env(&behind), &["proxy", "list"]).await;
    assert_eq!(stale.status.code(), Some(1));
    assert!(
        text(&stale.stderr).contains("run `kage migrate`"),
        "{}",
        text(&stale.stderr)
    );
}

/// A server process that is killed when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `kage serve`; the server stops when the value is dropped.
fn serve(env: &[(String, String)], address: &str) -> Server {
    Server(
        kage(env)
            .args(["serve", "--bind", address])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_syncs_the_pool_and_checks_its_health_by_itself() {
    let db = TestDb::migrated().await;
    let echo = Echo::start().await;
    let proxy = TestProxy::with_exit(2).await;
    echo.set_geo(loopback(2), GeoFixture::vienna());
    let dir = tempfile::TempDir::new().unwrap();
    let mut env = base_env(&db);
    env.extend(echo_env(&echo));
    // A static list: the provider is chosen by what is configured.
    env.push(("KAGE__PROXIES__STATIC_LIST".to_owned(), proxy.line("AT")));
    env.push((
        "KAGE__PROXIES__HEALTH__INTERVAL".to_owned(),
        "2s".to_owned(),
    ));
    env.push((
        "KAGE__PROXIES__HEALTH__STARTUP_SPREAD".to_owned(),
        "0s".to_owned(),
    ));
    env.push((
        "KAGE__ARTIFACTS__DIR".to_owned(),
        dir.path().join("artifacts").to_str().unwrap().to_owned(),
    ));
    let address = format!("127.0.0.1:{}", free_port());
    let server = serve(&env, &address);

    // The pool adopts the proxy and the first probe makes it healthy.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state: Option<String> = sqlx::query_scalar("SELECT state FROM proxies")
            .fetch_optional(db.store.pool())
            .await
            .unwrap();
        if state.as_deref() == Some("healthy") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the proxy never became healthy; it is {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (provider, country, asn, health): (String, String, String, Value) =
        sqlx::query_as("SELECT provider, country, asn, last_health FROM proxies")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(provider, "static");
    assert_eq!(country, "AT");
    assert_eq!(asn, "AS8447");
    assert_eq!(health["exit_ip"], "127.0.0.2");
    assert_eq!(health["leak_detected"], false);
    assert_eq!(health["geo"]["timezone"], "Europe/Vienna");
    // Each event is written with what it records (F65); the test still
    // waits for them rather than for the state, so that it says which one
    // is missing if one ever is.
    for kind in ["proxy_added", "proxy_state_changed", "proxy_geo_observed"] {
        let filter = format!("kind = '{kind}'");
        let deadline = Instant::now() + Duration::from_secs(30);
        while count(&db, "health_events", &filter).await == 0 {
            assert!(Instant::now() < deadline, "no {kind} event");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(count(&db, "health_events", &filter).await, 1, "{kind}");
    }
    // The probes keep coming, every two seconds give or take a quarter.
    let probes = echo.count("/ip");
    let deadline = Instant::now() + Duration::from_secs(30);
    while echo.count("/ip") < probes + 2 {
        assert!(Instant::now() < deadline, "the proxy was not probed again");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The geo lookup was sent once, not with every probe.
    assert_eq!(echo.count("/json"), 1);

    // The server answers while the pool works, and a CLI command next to it
    // sees the same pool.
    let healthz = run(&[], &["healthcheck", "--addr", &address]).await;
    assert!(healthz.status.success());
    let listed = json(&env, &["proxy", "list"]).await;
    assert_eq!(listed["proxies"][0]["state"], "healthy");
    assert_eq!(listed["proxies"][0]["provider"], "static");

    // The list goes missing from the configuration. The proxy is still in
    // the pool, so the next server still checks it: a leak must not go
    // unseen because a provider is no longer configured.
    drop(server);
    env.retain(|(name, _)| name != "KAGE__PROXIES__STATIC_LIST");
    let probes = echo.count("/ip");
    let address = format!("127.0.0.1:{}", free_port());
    let _server = serve(&env, &address);
    let deadline = Instant::now() + Duration::from_secs(60);
    while echo.count("/ip") < probes + 2 {
        assert!(
            Instant::now() < deadline,
            "the stored proxy was not probed without a provider"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let listed = json(&env, &["proxy", "list"]).await;
    assert_eq!(listed["proxies"][0]["state"], "healthy");
}
