//! `kage config check`: validates the configuration and the policy files,
//! then prints the effective configuration with every secret redacted
//! (spec §17).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use kage_core::config::Config;
use kage_core::policy::PolicySet;

/// File used when `--config` is not given and it exists in the working
/// directory.
pub(crate) const DEFAULT_CONFIG_FILE: &str = "kage.yaml";

/// The configuration file to read: the one named on the command line, else
/// `kage.yaml` in the working directory when there is one, else none
/// (environment variables only).
pub(crate) fn config_file(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let default = PathBuf::from(DEFAULT_CONFIG_FILE);
        default.is_file().then_some(default)
    })
}

/// Loads the configuration from `file` and `env`.
pub(crate) fn load(
    file: Option<&Path>,
    env: impl IntoIterator<Item = (String, String)>,
) -> anyhow::Result<Config> {
    Config::load(file, env).context("configuration is invalid")
}

/// Validates the configuration and the policy directory it names, then
/// writes one JSON document to `out`: the effective configuration, secrets
/// redacted, and a summary of the policies.
pub(crate) fn check(
    file: Option<&Path>,
    env: impl IntoIterator<Item = (String, String)>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let config = load(file, env)?;
    let policies = PolicySet::load(&config.policies.dir).context("policies are invalid")?;

    let report = serde_json::json!({
        "config_file": file,
        "config": config.redacted(),
        "policies": {
            "dir": config.policies.dir,
            "domains": policies.domains.domains().len(),
            "pacing_profiles": policies.pacing.names().collect::<Vec<_>>(),
            "signature_rules": policies.signatures.rules().len(),
            "signature_hints": policies.signatures.hint_count(),
        },
    });
    serde_json::to_writer_pretty(&mut *out, &report).context("cannot write the report")?;
    writeln!(out).context("cannot write the report")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{DEFAULT_CONFIG_FILE, check, config_file};

    const DB_URL: &str = "postgres://kage:db-pass-4f9a@db.internal:5432/kage";
    const TOKEN: &str = "sidecar-token-77c1";
    const MASTER: &str = "master-key-0b3e";

    /// The policy files shipped in the repository.
    fn shipped_policies() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies")
    }

    fn env(policies: &Path, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL".to_owned(), DB_URL.to_owned()),
            ("KAGE__SIDECAR__TOKEN".to_owned(), TOKEN.to_owned()),
            ("KAGE__SECRETS__MASTER_KEY".to_owned(), MASTER.to_owned()),
            (
                "KAGE__POLICIES__DIR".to_owned(),
                policies.to_str().unwrap().to_owned(),
            ),
        ];
        pairs.extend(
            extra
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        pairs
    }

    fn run(file: Option<&Path>, env: Vec<(String, String)>) -> anyhow::Result<String> {
        let mut out = Vec::new();
        check(file, env, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn prints_the_effective_configuration_with_secrets_redacted() {
        let output = run(
            None,
            env(
                &shipped_policies(),
                &[
                    ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa"),
                    ("KAGE__REPUTATION__ABUSEIPDB_KEY", "abuse-key-93bd"),
                    ("KAGE__SERVER__BIND", "127.0.0.1:9000"),
                ],
            ),
        )
        .unwrap();

        for secret in [
            DB_URL,
            "db-pass-4f9a",
            TOKEN,
            MASTER,
            "ws-key-51aa",
            "abuse-key-93bd",
        ] {
            assert!(
                !output.contains(secret),
                "the output leaks {secret}:\n{output}"
            );
        }
        let report: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(report["config"]["database"]["url"], "[redacted]");
        assert_eq!(report["config"]["sidecar"]["token"], "[redacted]");
        assert_eq!(report["config"]["secrets"]["master_key"], "[redacted]");
        assert_eq!(report["config"]["webshare"]["api_key"], "[redacted]");
        assert_eq!(
            report["config"]["reputation"]["abuseipdb_key"],
            "[redacted]"
        );
        // Effective values: the override is in, the defaults are in.
        assert_eq!(report["config"]["server"]["bind"], "127.0.0.1:9000");
        assert_eq!(report["config"]["artifacts"]["retention_days"], 14);
        assert!(report["config_file"].is_null());
        assert!(output.ends_with("}\n"));
    }

    #[test]
    fn the_shipped_policies_validate_and_are_summarized() {
        let output = run(None, env(&shipped_policies(), &[])).unwrap();
        let report: serde_json::Value = serde_json::from_str(&output).unwrap();
        let policies = &report["policies"];

        assert_eq!(policies["domains"], 0);
        assert_eq!(
            policies["pacing_profiles"],
            serde_json::json!(["fast", "human"])
        );
        assert!(policies["signature_rules"].as_u64().unwrap() >= 25);
        assert!(policies["signature_hints"].as_u64().unwrap() >= 10);
    }

    #[test]
    fn reads_the_configuration_file_and_reports_it() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("kage.yaml");
        std::fs::write(&file, "artifacts:\n  retention_days: 3\n").unwrap();

        let output = run(Some(&file), env(&shipped_policies(), &[])).unwrap();
        let report: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(report["config"]["artifacts"]["retention_days"], 3);
        assert_eq!(report["config_file"], file.to_str().unwrap());
    }

    #[test]
    fn a_missing_secret_fails_and_names_the_variable() {
        let mut pairs = env(&shipped_policies(), &[]);
        pairs.retain(|(name, _)| name != "KAGE__SIDECAR__TOKEN");

        let error = run(None, pairs).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("configuration is invalid"), "{message}");
        assert!(message.contains("KAGE__SIDECAR__TOKEN"), "{message}");
        assert!(!message.contains("db-pass-4f9a"), "{message}");
    }

    #[test]
    fn an_invalid_policy_file_fails_and_names_the_file() {
        let dir = TempDir::new().unwrap();
        for name in [
            "domains.yaml",
            "pacing-profiles.yaml",
            "block-signatures.yaml",
        ] {
            std::fs::copy(shipped_policies().join(name), dir.path().join(name)).unwrap();
        }
        std::fs::write(
            dir.path().join("domains.yaml"),
            "defaults:\n  render: sideways\n",
        )
        .unwrap();

        let error = run(None, env(dir.path(), &[])).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("policies are invalid"), "{message}");
        assert!(message.contains("domains.yaml"), "{message}");

        let missing = run(None, env(&dir.path().join("nowhere"), &[])).unwrap_err();
        assert!(format!("{missing:#}").contains("cannot read"));
    }

    #[test]
    fn the_default_file_is_used_only_when_it_exists() {
        assert_eq!(
            config_file(Some(PathBuf::from("/etc/kage/custom.yaml"))),
            Some(PathBuf::from("/etc/kage/custom.yaml"))
        );
        // The tests run in the crate directory, which has no kage.yaml.
        assert!(!Path::new(DEFAULT_CONFIG_FILE).exists());
        assert_eq!(config_file(None), None);
    }
}
