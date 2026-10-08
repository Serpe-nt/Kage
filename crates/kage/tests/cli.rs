//! The `kage` binary from the outside: `config check`, `migrate` and the
//! startup checks of `serve`, including a policy file that turns invalid
//! while the server runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const DB_URL: &str = "postgres://kage:db-pass-4f9a@db.internal:5432/kage";
const TOKEN: &str = "sidecar-token-77c1";
// 32 bytes as 64 hexadecimal characters.
const MASTER: &str = "0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e";

/// The policy files shipped in the repository.
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

fn secrets<'a>(database_url: &'a str, policies: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("KAGE__DATABASE__URL", database_url),
        ("KAGE__SIDECAR__TOKEN", TOKEN),
        ("KAGE__SECRETS__MASTER_KEY", MASTER),
        ("KAGE__POLICIES__DIR", policies),
    ]
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(command: &mut Command) -> Output {
    command.output().expect("cannot run the kage binary")
}

#[test]
fn config_check_prints_the_effective_configuration_without_secrets() {
    let policies = shipped_policies();
    let output = run(kage(&secrets(DB_URL, policies.to_str().unwrap())).args(["config", "check"]));

    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    for secret in [DB_URL, "db-pass-4f9a", TOKEN, MASTER] {
        assert!(!stdout.contains(secret), "stdout leaks {secret}");
        assert!(!stderr.contains(secret), "stderr leaks {secret}");
    }
    // stdout is exactly one JSON document.
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["config"]["database"]["url"], "[redacted]");
    assert_eq!(report["config"]["sidecar"]["token"], "[redacted]");
    assert_eq!(report["config"]["secrets"]["master_key"], "[redacted]");
    assert_eq!(report["config"]["server"]["bind"], "0.0.0.0:8080");
    assert_eq!(
        report["policies"]["pacing_profiles"],
        serde_json::json!(["fast", "human"])
    );
}

#[test]
fn config_check_fails_when_a_required_secret_is_missing() {
    let policies = shipped_policies();
    let mut env = secrets(DB_URL, policies.to_str().unwrap());
    env.retain(|(name, _)| *name != "KAGE__SECRETS__MASTER_KEY");

    let output = run(kage(&env).args(["config", "check"]));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("missing required secret(s): KAGE__SECRETS__MASTER_KEY"),
        "{stderr}"
    );
    assert!(!stderr.contains("db-pass-4f9a"), "{stderr}");
}

#[test]
fn config_check_refuses_a_secret_in_the_configuration_file() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("kage.yaml");
    std::fs::write(&file, "sidecar:\n  token: leaked-in-file\n").unwrap();
    let policies = shipped_policies();

    let output = run(kage(&secrets(DB_URL, policies.to_str().unwrap()))
        .args(["config", "check", "--config"])
        .arg(&file));

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("sidecar.token is a secret"), "{stderr}");
    assert!(stderr.contains("KAGE__SIDECAR__TOKEN"), "{stderr}");
    assert!(!stderr.contains("leaked-in-file"), "{stderr}");
}

#[test]
fn config_check_reads_secrets_from_files_and_the_file_named_by_the_environment() {
    let dir = TempDir::new().unwrap();
    let token_file = dir.path().join("sidecar_token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let config_file = dir.path().join("custom.yaml");
    std::fs::write(&config_file, "server:\n  bind: 127.0.0.1:9100\n").unwrap();
    let policies = shipped_policies();
    let env = [
        ("KAGE__DATABASE__URL", DB_URL),
        ("KAGE__SIDECAR__TOKEN_FILE", token_file.to_str().unwrap()),
        ("KAGE__SECRETS__MASTER_KEY", MASTER),
        ("KAGE__POLICIES__DIR", policies.to_str().unwrap()),
        ("KAGE_CONFIG", config_file.to_str().unwrap()),
    ];

    let output = run(kage(&env).args(["config", "check"]));

    assert!(output.status.success(), "{}", text(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["config"]["server"]["bind"], "127.0.0.1:9100");
    assert_eq!(report["config"]["sidecar"]["token"], "[redacted]");
    assert_eq!(report["config_file"], config_file.to_str().unwrap());
}

#[test]
fn config_check_fails_on_an_invalid_policy_file() {
    let dir = TempDir::new().unwrap();
    for name in [
        "domains.yaml",
        "pacing-profiles.yaml",
        "block-signatures.yaml",
    ] {
        std::fs::copy(shipped_policies().join(name), dir.path().join(name)).unwrap();
    }
    std::fs::write(dir.path().join("pacing-profiles.yaml"), "fast: {}\n").unwrap();

    let output =
        run(kage(&secrets(DB_URL, dir.path().to_str().unwrap())).args(["config", "check"]));

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("policies are invalid"), "{stderr}");
    assert!(stderr.contains("pacing-profiles.yaml"), "{stderr}");
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

// One database and one server followed from empty to serving: each step needs the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn migrate_then_serve_and_survive_a_policy_file_turning_invalid() {
    let container = Postgres::default()
        .with_tag("17.11-trixie")
        .start()
        .await
        .expect("cannot start the Postgres test container; is Docker running?");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let host = container.get_host().await.unwrap();
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let dir = TempDir::new().unwrap();
    let policies = dir.path().join("policies");
    std::fs::create_dir(&policies).unwrap();
    for name in [
        "domains.yaml",
        "pacing-profiles.yaml",
        "block-signatures.yaml",
    ] {
        std::fs::copy(shipped_policies().join(name), policies.join(name)).unwrap();
    }
    let artifacts = dir.path().join("artifacts");
    let mut env = secrets(&database_url, policies.to_str().unwrap());
    env.push(("KAGE__ARTIFACTS__DIR", artifacts.to_str().unwrap()));
    env.push(("RUST_LOG", "info"));

    // An empty database: the check fails and the server refuses to start.
    let check = run(kage(&env).args(["migrate", "--check"]));
    assert_eq!(check.status.code(), Some(1));
    assert!(
        text(&check.stderr).contains("pending migrations"),
        "{}",
        text(&check.stderr)
    );
    let refused = run(kage(&env).args(["serve", "--bind", "127.0.0.1:0"]));
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("run `kage migrate`"),
        "{}",
        text(&refused.stderr)
    );

    // Migrating twice is the same as migrating once.
    for _ in 0..2 {
        let migrated = run(kage(&env).arg("migrate"));
        assert!(migrated.status.success(), "{}", text(&migrated.stderr));
    }
    assert!(
        run(kage(&env).args(["migrate", "--check"]))
            .status
            .success()
    );

    // An invalid policy file at startup is fatal: there is no last good one.
    std::fs::write(
        policies.join("domains.yaml"),
        "defaults:\n  render: sideways\n",
    )
    .unwrap();
    let invalid = run(kage(&env).args(["serve", "--bind", "127.0.0.1:0"]));
    assert_eq!(invalid.status.code(), Some(1));
    assert!(
        text(&invalid.stderr).contains("policies are invalid"),
        "{}",
        text(&invalid.stderr)
    );
    std::fs::copy(
        shipped_policies().join("domains.yaml"),
        policies.join("domains.yaml"),
    )
    .unwrap();

    // Now it starts and answers.
    let address = format!("127.0.0.1:{}", free_port());
    let _server = Server(
        kage(&env)
            .args(["serve", "--bind", &address])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let healthy = || {
        run(kage(&[]).args(["healthcheck", "--addr", &address]))
            .status
            .success()
    };
    eventually("the server answers /healthz", healthy);

    // A policy file turns invalid while the server runs: it keeps serving,
    // and the alert is stored as a health event.
    std::fs::write(policies.join("domains.yaml"), "defaults:\n  retries: 99\n").unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let mut stored: Option<(String, String, serde_json::Value)> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while stored.is_none() {
        assert!(
            Instant::now() < deadline,
            "no policy_reload_failed event was stored"
        );
        stored = sqlx::query_as(
            "SELECT subject_kind, severity, details FROM health_events
             WHERE kind = 'policy_reload_failed' ORDER BY id LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (subject_kind, severity, details) = stored.unwrap();
    assert_eq!(subject_kind, "policy");
    assert_eq!(severity, "critical");
    assert!(
        details["details"]["file"]
            .as_str()
            .unwrap()
            .ends_with("domains.yaml"),
        "{details}"
    );
    assert!(
        details["details"]["error"]
            .as_str()
            .unwrap()
            .contains("retries"),
        "{details}"
    );
    assert!(
        healthy(),
        "the server stopped answering after the invalid reload"
    );

    // The startup sweep of the retention job created nothing and removed
    // nothing, and it did not need the artifact directory to exist.
    let artifact_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(artifact_rows, 0);
}
