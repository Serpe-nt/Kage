//! `kage persona list|show|selftest|regenerate|retire` and `kage selftest
//! browser --persona`, as the real binary against a Postgres container and
//! mock sidecars. The personas are made by a manager in this process, as
//! the server would; the binary then works on the same database.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

// The throwaway database, the provider and the mock sidecars of
// kage-persona's tests.
#[path = "../../kage-persona/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{Harness, MASTER_KEY, selftest_answer};
use kage_core::domain::{AuditOutcome, PersonaState, Render};
use serde_json::{Value, json};

fn shipped_policies() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The binary on the harness's database and sidecars.
struct Cli {
    env: Vec<(String, String)>,
    config: PathBuf,
}

impl Cli {
    fn new(harness: &Harness, dir: &Path) -> Self {
        let config = dir.join("kage.yaml");
        std::fs::write(
            &config,
            format!(
                "sidecar:\n  chromium_url: {}\n  firefox_url: {}\n\
                 engines:\n  chromium:\n    browser_major: 149\n  firefox:\n    browser_major: 151\n\
                 personas:\n  warmup:\n    enabled: false\n\
                 selftest:\n  browser:\n    pages:\n      - id: probe\n        kind: probe\n        \
                 url: https://fixture.kage.test/probe/\n",
                harness.chromium.url(),
                harness.firefox.url(),
            ),
        )
        .unwrap();
        let env = [
            ("KAGE__DATABASE__URL", harness.db.url.as_str()),
            ("KAGE__SIDECAR__TOKEN", "token"),
            ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
            ("KAGE__POLICIES__DIR", shipped_policies().to_str().unwrap()),
            ("RUST_LOG", "warn"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        Self { env, config }
    }

    /// Runs the binary to its end without blocking the runtime that serves
    /// the mock sidecars.
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

    /// Runs a command that must succeed and returns its stdout.
    async fn ok(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments).await;
        assert!(
            output.status.success(),
            "kage {arguments:?} failed:\n{}\n{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// Runs a command with `--json` and returns the document on its stdout.
    async fn json(&self, arguments: &[&str]) -> Value {
        let mut all = arguments.to_vec();
        all.push("--json");
        let stdout = self.ok(&all).await;
        let parsed = serde_json::from_str(&stdout);
        assert!(parsed.is_ok(), "stdout is not one JSON document:\n{stdout}");
        parsed.unwrap()
    }
}

#[tokio::test]
async fn the_persona_commands_inspect_test_replace_and_retire_personas() {
    let harness = Harness::with(149, 151, |_| {}).await;
    let (german_proxy, german) = harness.persona("DE").await;
    let (_, french) = harness.persona("FR").await;
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::new(&harness, dir.path());
    let german_id = german.id.to_string();
    let french_id = french.id.to_string();

    // `list`: the live personas and how they spread over the mix.
    let listed = cli.json(&["persona", "list"]).await;
    let personas = listed["personas"].as_array().unwrap();
    assert_eq!(personas.len(), 2);
    let shown = personas
        .iter()
        .find(|persona| persona["id"] == json!(german.id))
        .unwrap();
    assert_eq!(shown["proxy_id"], json!(german_proxy.id));
    assert_eq!(shown["state"], "active");
    assert_eq!(shown["engine"], german.engine.as_str());
    assert_eq!(shown["country"], "DE");
    assert_eq!(shown["timezone"], "Europe/Berlin");
    assert_eq!(shown["languages"], json!(german.languages));
    assert_eq!(shown["ready"], true);
    // The fingerprint is the sidecar's business and is not listed.
    assert!(shown.get("fingerprint").is_none());
    let counted: u64 = listed["distribution"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["count"].as_u64().unwrap())
        .sum();
    assert_eq!(counted, 2);
    let table = cli.ok(&["persona", "list"]).await;
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table.contains(&german_id) && table.contains(&french_id),
        "{table}"
    );
    assert!(table.contains("2 live: chromium/linux "), "{table}");
    assert_eq!(
        cli.json(&["persona", "list", "--state", "cooling"]).await["personas"],
        json!([])
    );

    // `show`: one persona, with its fingerprint only when asked for.
    let one = cli.json(&["persona", "show", &german_id]).await;
    assert_eq!(one["id"], json!(german.id));
    assert_eq!(one["seed"], json!(german.seed.to_string()));
    assert_eq!(one["open_sessions"], 0);
    assert_eq!(one["http_path_off"], Value::Null);
    assert_eq!(one["wreq_profile"], json!(german.wreq_profile));
    assert!(one.get("fingerprint").is_none());
    let whole = cli
        .json(&["persona", "show", &german_id, "--fingerprint"])
        .await;
    assert_eq!(whole["fingerprint"], german.fingerprint);
    let sheet = cli.ok(&["persona", "show", &german_id]).await;
    assert!(sheet.contains("time zone      Europe/Berlin"), "{sheet}");
    assert!(sheet.contains(&german.user_agent), "{sheet}");
    let nobody = "0199c2f0-0000-7000-8000-000000000001";
    let missing = cli.run(&["persona", "show", nobody]).await;
    assert!(!missing.status.success());
    assert!(
        text(&missing.stderr).contains(&format!("there is no persona {nobody}")),
        "{}",
        text(&missing.stderr)
    );

    // `selftest`: the persona's own context and the operator's own page.
    // No echo of the operator's is configured, so the http path is not
    // tested, and no third party is asked.
    let sidecar = harness.sidecar(german.engine);
    let tested = cli.json(&["persona", "selftest", &german_id]).await;
    assert_eq!(tested["passed"], true);
    assert_eq!(tested["third_party"], false);
    assert_eq!(tested["browser"]["verdict"], "passed");
    assert!(
        tested["http"]["skipped"]
            .as_str()
            .unwrap()
            .contains("selftest.http.echo_url is not set"),
        "{}",
        tested["http"]
    );
    assert_eq!(tested["persona"]["state"], "active");
    let asked = sidecar.with(|state| state.selftested.clone());
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0]["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|page| page["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["probe"]
    );
    assert_eq!(asked[0]["expected"]["timezone"], "Europe/Berlin");
    let opened = sidecar.with(|state| state.opened.clone());
    assert_eq!(opened[0]["fingerprint"], german.fingerprint);
    assert_eq!(opened[0]["profile_dir"], json!(german.profile_dir));
    // The context was handed the command's own forwarder under the
    // persona's credential, never the provider's proxy and its password
    // (security review G4).
    assert_eq!(opened[0]["proxy"]["username"], german_id.as_str());
    let server = opened[0]["proxy"]["server"].as_str().unwrap();
    assert!(server.starts_with("http://kage:"), "{server}");
    assert_ne!(server, "http://kage:8899");
    assert!(!opened[0].to_string().contains("secret-pass-91aa"));
    let runs = harness.store().selftest_runs();
    assert_eq!(runs.count(german.id, Render::Browser).await.unwrap(), 1);
    assert_eq!(runs.count(german.id, Render::Http).await.unwrap(), 0);

    // `kage selftest browser --persona --third-party`: the pages run by
    // others too, because someone asked.
    let third = cli
        .json(&[
            "selftest",
            "browser",
            "--persona",
            &german_id,
            "--third-party",
        ])
        .await;
    assert_eq!(third["third_party"], true);
    assert_eq!(third["http"], Value::Null);
    let asked = sidecar.with(|state| state.selftested.last().cloned().unwrap());
    assert_eq!(asked["pages"].as_array().unwrap().len(), 3);
    assert_eq!(runs.count(german.id, Render::Browser).await.unwrap(), 2);

    // A failing self-test: the command fails after showing what it found,
    // the persona cools, and the alert is on record.
    sidecar.with(|state| state.selftest = selftest_answer(false));
    let failed = cli.run(&["persona", "selftest", &german_id]).await;
    assert!(!failed.status.success());
    let report = text(&failed.stdout);
    assert!(
        report.contains("browser: failed, score 40.0 of 100"),
        "{report}"
    );
    assert!(report.contains("navigator.webdriver  FAIL"), "{report}");
    assert!(
        report.contains("the persona is cooling (selftest_failed)"),
        "{report}"
    );
    assert!(
        text(&failed.stderr).contains("the self-test of persona"),
        "{}",
        text(&failed.stderr)
    );
    assert_eq!(harness.reload(&german).await.state, PersonaState::Cooling);
    let events = harness.events_of(&german).await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "browser_selftest_failed"),
        "{:?}",
        events.iter().map(|event| &event.kind).collect::<Vec<_>>()
    );
    let cooling = cli.ok(&["persona", "list"]).await;
    assert!(cooling.contains("cooling (selftest_failed)"), "{cooling}");
    // A passing run ends the cooling.
    sidecar.with(|state| state.selftest = selftest_answer(true));
    let recovered = cli.ok(&["persona", "selftest", &german_id]).await;
    assert!(recovered.contains("the persona is active"), "{recovered}");
    let sheet = cli.ok(&["persona", "show", &german_id]).await;
    assert!(sheet.contains("SELF-TEST"), "{sheet}");
    assert!(sheet.contains("failed"), "{sheet}");

    // `regenerate`: a new persona behind the same proxy.
    let replaced = cli.json(&["persona", "regenerate", &german_id]).await;
    assert_ne!(replaced["id"], json!(german.id));
    assert_eq!(replaced["proxy_id"], json!(german_proxy.id));
    assert_eq!(replaced["state"], "active");
    let old = harness.reload(&german).await;
    assert_eq!(old.state, PersonaState::Retired);
    assert_eq!(old.retire_reason.as_deref(), Some("regenerated"));
    let again = cli.run(&["persona", "regenerate", &german_id]).await;
    assert!(!again.status.success());
    assert!(
        text(&again.stderr).contains("is retired"),
        "{}",
        text(&again.stderr)
    );

    // `retire`: by hand.
    let sentence = cli.ok(&["persona", "retire", &french_id]).await;
    assert!(
        sentence.contains(&format!("persona {french_id} is retired (operator)")),
        "{sentence}"
    );
    assert_eq!(
        harness.reload(&french).await.retire_reason.as_deref(),
        Some("operator")
    );
    let live = cli.json(&["persona", "list"]).await;
    assert_eq!(live["personas"].as_array().unwrap().len(), 1);
    let retired = cli.json(&["persona", "list", "--state", "retired"]).await;
    assert_eq!(retired["personas"].as_array().unwrap().len(), 2);
    let everyone = cli.json(&["persona", "list", "--all"]).await;
    assert_eq!(everyone["personas"].as_array().unwrap().len(), 3);

    // Every command that changed something left an audit event.
    let audit = harness.store().audit_events().list(None, 50).await.unwrap();
    let actions: Vec<(&str, AuditOutcome)> = audit
        .iter()
        .rev()
        .map(|event| (event.action.as_str(), event.outcome))
        .collect();
    assert_eq!(
        actions,
        [
            ("persona.selftest", AuditOutcome::Success),
            ("persona.selftest", AuditOutcome::Success),
            ("persona.selftest", AuditOutcome::Failure),
            ("persona.selftest", AuditOutcome::Success),
            ("persona.regenerate", AuditOutcome::Success),
            ("persona.regenerate", AuditOutcome::Failure),
            ("persona.retire", AuditOutcome::Success),
        ]
    );
    assert!(
        audit
            .iter()
            .all(|event| !event.details.to_string().contains("secret-pass"))
    );
}
