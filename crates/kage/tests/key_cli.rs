//! `kage key create|list|revoke|rotate`, `kage openapi` and the API of
//! `kage serve`, as the real binary against a Postgres container: the first
//! key of a deployment is made at the command line, and the server takes
//! it at once.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const TOKEN: &str = "sidecar-token-77c1";
// 32 bytes as 64 hexadecimal characters.
const MASTER: &str = "0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e";

fn shipped_policies() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies")
}

/// The binary with an empty environment, so nothing from the machine that
/// runs the tests leaks into the configuration.
fn kage(env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kage"));
    command.env_clear();
    // Coverage runs collect profiles from the child process through this.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command.envs(env.iter().copied());
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(command: &mut Command) -> Output {
    command.output().expect("cannot run the kage binary")
}

/// Runs a command that must succeed and returns its stdout.
#[track_caller]
fn ok(command: &mut Command) -> String {
    let output = run(command);
    assert!(
        output.status.success(),
        "{}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    text(&output.stdout)
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

/// A server process that is killed when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A loopback port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One request to the server over a connection of its own: the status and
/// the body.
fn call(
    address: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    if let Some(token) = token {
        let _ = write!(request, "Authorization: Bearer {token}\r\n");
    }
    if let Some(body) = body {
        let _ = write!(
            request,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    request.push_str("\r\n");
    request.push_str(body.unwrap_or_default());
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {answer:?}"));
    let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
    // A body that was sent in chunks: the pieces between the size lines.
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut whole = String::new();
        let mut rest = body;
        while let Some((size, after)) = rest.split_once("\r\n") {
            let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
                break;
            };
            if size == 0 || after.len() < size {
                break;
            }
            whole.push_str(&after[..size]);
            rest = after[size..].trim_start_matches("\r\n");
        }
        whole
    } else {
        body.to_owned()
    };
    (status, body)
}

#[test]
fn the_document_of_the_api_is_printed_without_any_configuration() {
    // No secret, no file, no database: the document is the same everywhere.
    let printed = ok(kage(&[]).arg("openapi"));

    let document = json(&printed);
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["paths"]["/v1/fetch"]["post"]["operationId"],
        "fetch"
    );
    assert!(printed.ends_with("}\n"));
    // It is the committed one, which the server serves.
    let committed = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../kage-api/openapi.json"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    assert!(
        printed.replace("\r\n", "\n") == committed,
        "crates/kage-api/openapi.json is out of date: run `just openapi`"
    );
}

#[test]
fn a_key_command_that_is_not_one_is_refused_before_anything_is_read() {
    for arguments in [
        vec!["key", "create", "--name", "x"],
        vec!["key", "create", "--name", "x", "--scope", "root"],
        vec!["key", "create", "--scope", "fetch"],
        vec!["key", "revoke", "not-an-id"],
        vec!["key", "rotate"],
        vec!["key"],
    ] {
        let output = run(kage(&[]).args(&arguments));
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert_eq!(text(&output.stdout), "", "{arguments:?}");
    }
}

// One database followed from its first key to a server that takes it: each
// step needs the one before.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_key_is_made_at_the_command_line_and_the_server_takes_it() {
    let container = Postgres::default()
        .with_tag("17.11-trixie")
        .start()
        .await
        .expect("cannot start the Postgres test container; is Docker running?");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let host = container.get_host().await.unwrap();
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let dir = TempDir::new().unwrap();
    let artifacts = dir.path().join("artifacts");
    let policies = shipped_policies();
    let env = [
        ("KAGE__DATABASE__URL", database_url.as_str()),
        ("KAGE__SIDECAR__TOKEN", TOKEN),
        ("KAGE__SECRETS__MASTER_KEY", MASTER),
        ("KAGE__POLICIES__DIR", policies.to_str().unwrap()),
        ("KAGE__ARTIFACTS__DIR", artifacts.to_str().unwrap()),
        ("RUST_LOG", "warn"),
    ];
    ok(kage(&env).arg("migrate"));

    // A deployment starts without keys.
    let empty = ok(kage(&env).args(["key", "list"]));
    assert!(empty.contains("no API keys"), "{empty}");
    assert_eq!(
        json(&ok(kage(&env).args(["key", "list", "--json"]))),
        serde_json::json!({ "keys": [] })
    );

    // The first one, for an operator who also fetches. The token is on
    // stdout and nowhere else; the log goes to stderr.
    let created = run(kage(&env).args([
        "key",
        "create",
        "--name",
        "operator",
        "--scope",
        "admin",
        "--scope",
        "fetch",
        "--requests-per-minute",
        "30",
        "--daily-page-budget",
        "500",
        "--json",
    ]));
    assert!(created.status.success(), "{}", text(&created.stderr));
    let key = json(&text(&created.stdout));
    let id = key["id"].as_str().unwrap().to_owned();
    let token = key["token"].as_str().unwrap().to_owned();
    assert!(
        token.starts_with(&format!("kage_{}_", id.replace('-', ""))),
        "{token}"
    );
    assert_eq!(key["name"], "operator");
    assert_eq!(key["scopes"], serde_json::json!(["fetch", "admin"]));
    assert_eq!(key["requests_per_minute"], 30);
    assert_eq!(key["max_concurrent"], 4);
    assert_eq!(key["daily_page_budget"], 500);
    assert_eq!(key["active"], true);
    let secret = token.rsplit('_').next().unwrap().to_owned();
    assert!(!text(&created.stderr).contains(&secret));

    // In words, with what to do with it.
    let second = ok(kage(&env).args(["key", "create", "--name", "reader", "--scope", "fetch"]));
    assert!(second.contains("created key "), "{second}");
    assert!(second.contains("scopes: fetch"), "{second}");
    assert!(
        second.contains("limits: 60 requests a minute, 4 in flight, unlimited pages a day"),
        "{second}"
    );
    assert!(second.contains("token (shown once"), "{second}");
    assert!(second.contains("Authorization: Bearer <token>"), "{second}");
    let reader = second
        .lines()
        .find(|line| line.starts_with("kage_"))
        .unwrap()
        .to_owned();

    // Listed without tokens: Kage does not have them.
    let listed = ok(kage(&env).args(["key", "list"]));
    assert!(
        listed.contains("operator") && listed.contains("reader"),
        "{listed}"
    );
    assert!(listed.contains("fetch,admin"), "{listed}");
    assert!(
        !listed.contains(&secret) && !listed.contains("kage_"),
        "{listed}"
    );
    let listed = json(&ok(kage(&env).args(["key", "list", "--json"])));
    assert_eq!(listed["keys"].as_array().unwrap().len(), 2);
    assert!(listed["keys"][0].get("token").is_none());
    // A name that is not one, limits that cannot be.
    for arguments in [
        vec!["key", "create", "--name", "", "--scope", "fetch"],
        vec![
            "key",
            "create",
            "--name",
            "x",
            "--scope",
            "fetch",
            "--max-concurrent",
            "0",
        ],
    ] {
        let refused = run(kage(&env).args(&arguments));
        assert_eq!(refused.status.code(), Some(1), "{arguments:?}");
        assert_eq!(text(&refused.stdout), "");
    }

    // The server takes the key at once. It has no proxy pool: the API
    // answers, and says that nothing can be fetched.
    let address = format!("127.0.0.1:{}", free_port());
    let _server = Server(
        kage(&env)
            .args(["serve", "--bind", &address])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    eventually("the server answers /healthz", || {
        run(kage(&[]).args(["healthcheck", "--addr", &address]))
            .status
            .success()
    });
    let (status, body) = call(&address, "GET", "/v1/admin/keys", Some(&token), None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["keys"].as_array().unwrap().len(), 2);
    let (status, body) = call(&address, "GET", "/v1/admin/keys", None, None);
    assert_eq!(status, 401, "{body}");
    assert_eq!(json(&body)["code"], "missing_key");
    let (status, body) = call(&address, "GET", "/v1/admin/keys", Some(&reader), None);
    assert_eq!(status, 403, "{body}");
    assert_eq!(json(&body)["code"], "scope_missing");
    let (status, body) = call(
        &address,
        "POST",
        "/v1/fetch",
        Some(&token),
        Some(r#"{"url":"https://example.org/"}"#),
    );
    assert_eq!(status, 503, "{body}");
    assert_eq!(json(&body)["code"], "unavailable");
    let (status, body) = call(&address, "GET", "/readyz", None, None);
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        json(&body),
        serde_json::json!({
            "status": "not_ready",
            "checks": { "database": true, "persona": false, "sidecar": false },
        })
    );
    // The document it serves is the one the command prints.
    let (status, body) = call(&address, "GET", "/openapi.json", None, None);
    assert_eq!(status, 200);
    assert_eq!(json(&body), json(&ok(kage(&[]).arg("openapi"))));
    // And the exporter is installed: the API's own series are there.
    let (status, body) = call(&address, "GET", "/metrics", Some(&token), None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("kage_api_requests_total{"), "{body}");
    assert!(
        body.contains("kage_api_refusals_total{code=\"missing_key\"}"),
        "{body}"
    );
    assert!(
        body.contains("kage_persona_state{state=\"active\"} 0"),
        "{body}"
    );

    // Rotated at the command line: the running server refuses the old
    // token at once and takes the new one.
    let rotated = json(&ok(kage(&env).args(["key", "rotate", &id, "--json"])));
    let fresh = rotated["token"].as_str().unwrap().to_owned();
    assert_ne!(fresh, token);
    assert_eq!(rotated["id"], id.as_str());
    let (status, body) = call(&address, "GET", "/v1/admin/keys", Some(&token), None);
    assert_eq!(status, 401, "{body}");
    assert_eq!(json(&body)["code"], "invalid_key");
    let (status, _) = call(&address, "GET", "/v1/admin/keys", Some(&fresh), None);
    assert_eq!(status, 200);

    // Revoked: it stops authenticating at once; the row stays.
    let revoked = ok(kage(&env).args(["key", "revoke", &id]));
    assert!(revoked.contains("is disabled"), "{revoked}");
    let (status, body) = call(&address, "GET", "/v1/admin/keys", Some(&fresh), None);
    assert_eq!(status, 401, "{body}");
    assert_eq!(json(&body)["code"], "key_disabled");
    let again = ok(kage(&env).args(["key", "revoke", &id]));
    assert!(again.contains("was disabled already"), "{again}");
    let active = json(&ok(kage(&env).args(["key", "list", "--json"])));
    assert_eq!(active["keys"].as_array().unwrap().len(), 1);
    assert_eq!(active["keys"][0]["name"], "reader");
    let all = json(&ok(kage(&env).args(["key", "list", "--all", "--json"])));
    let disabled = all["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["id"] == id.as_str())
        .unwrap();
    assert_eq!(disabled["active"], false);
    assert!(disabled["disabled_at"].is_string());
    // A disabled key is not given a new secret, and a key that is not
    // there is not revoked.
    let refused = run(kage(&env).args(["key", "rotate", &id]));
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("is disabled"),
        "{}",
        text(&refused.stderr)
    );
    let missing = run(kage(&env).args(["key", "revoke", "0190a0b0-0000-7000-8000-000000000001"]));
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        text(&missing.stderr).contains("there is no API key"),
        "{}",
        text(&missing.stderr)
    );

    // What was done is in the audit trail: by the command line and, for
    // the calls to the admin API, by the key. Never a token or a hash.
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let events: Vec<(String, String, Option<String>, String, Value)> = sqlx::query_as(
        "SELECT action, actor, target, outcome, details FROM audit_events ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let by_cli: Vec<(&str, Option<&str>)> = events
        .iter()
        .filter(|event| event.1 == "cli")
        .map(|event| (event.0.as_str(), event.2.as_deref()))
        .collect();
    assert_eq!(by_cli.len(), 4, "{events:?}");
    assert_eq!(by_cli[0], ("key.create", Some(id.as_str())));
    assert_eq!(by_cli[1].0, "key.create");
    assert_eq!(by_cli[2], ("key.rotate", Some(id.as_str())));
    assert_eq!(by_cli[3], ("key.revoke", Some(id.as_str())));
    let by_key: Vec<(&str, &str)> = events
        .iter()
        .filter(|event| event.1 == "api_key")
        .map(|event| (event.0.as_str(), event.3.as_str()))
        .collect();
    assert!(
        by_key.contains(&("admin.key.list", "success")),
        "{by_key:?}"
    );
    assert!(by_key.contains(&("admin.key.list", "denied")), "{by_key:?}");
    let trail =
        serde_json::to_string(&events.iter().map(|event| &event.4).collect::<Vec<_>>()).unwrap();
    for token in [&token, &fresh, &reader] {
        assert!(!trail.contains(token.rsplit('_').next().unwrap()));
    }
    let hashes: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT secret_hash FROM api_keys")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(hashes.iter().all(|(hash,)| hash.len() == 32));
    pool.close().await;
}
