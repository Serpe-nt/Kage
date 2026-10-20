//! `kage config check`: validates the configuration and the policy files,
//! then prints the effective configuration with every secret redacted
//! (spec §17).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use kage_core::config::{Config, EgressMode, ResolvedProvider, StaticProxy};
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

/// Validates the configuration, the policy directory it names and the
/// proxy pool's settings, then writes one JSON document to `out`: the
/// effective configuration, secrets redacted, a summary of the policies,
/// which proxy provider is in force, and where requests leave from with
/// what deserves a second look (`egress.warnings`). A configuration that
/// would use the pool and the host's own address for one site by turns is
/// refused before (spec §25).
pub(crate) fn check(
    file: Option<&Path>,
    env: impl IntoIterator<Item = (String, String)>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let config = load(file, env)?;
    let policies = PolicySet::load(&config.policies.dir).context("policies are invalid")?;
    // What only the proxy crate can judge, such as whether the probe's
    // emulation profile exists in this build.
    kage_proxy::check_config(&config)
        .map_err(|problem| anyhow::anyhow!(problem))
        .context("configuration is invalid")?;
    // And what only the http path can judge: that an engine's configured
    // browser major has an emulation profile.
    kage_http::check_config(&config)
        .map_err(|problem| anyhow::anyhow!(problem))
        .context("configuration is invalid")?;
    let static_proxies = match &config.proxies.static_list {
        Some(list) => StaticProxy::parse_list(list.expose())
            .context("configuration is invalid")?
            .len(),
        None => 0,
    };

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
        "proxies": {
            // What `auto` resolved to.
            "provider": config.proxy_provider(),
            "static_proxies": static_proxies,
            "reputation": config.reputation.abuseipdb_key.is_some(),
        },
        // What the http path can present as in this build.
        "http": kage_http::describe(&config),
        // What the persona manager will do by itself.
        "personas": kage_persona::describe(&config),
        // The scheduler's caps, the bandwidth budget and the forwarder.
        "scheduler": kage_scheduler::describe(&config),
        // Where requests leave from (spec §25).
        "egress": egress(&config),
    });
    serde_json::to_writer_pretty(&mut *out, &report).context("cannot write the report")?;
    writeln!(out).context("cannot write the report")?;
    Ok(())
}

/// Where requests leave from, and what deserves a second look.
fn egress(config: &Config) -> serde_json::Value {
    let egress = &config.egress;
    let direct = egress.mode == EgressMode::Direct;
    let mut warnings: Vec<String> = Vec::new();
    if direct && config.proxy_provider() != ResolvedProvider::None {
        warnings.push(
            "egress.mode is direct and a proxy provider is configured: the pool is idle, neither \
             synced, probed nor used"
                .to_owned(),
        );
    }
    if direct && egress.direct.country.is_none() && config.proxies.health.geo_url.is_none() {
        warnings.push(
            "egress.mode is direct with neither egress.direct.country nor \
             proxies.health.geo_url: no host persona can be made, and only level 0 works"
                .to_owned(),
        );
    }
    if !egress.direct.private_names.is_empty() {
        warnings.push(format!(
            "egress.direct.private_names lets {} name(s) resolve to a private or loopback \
             address on the host's own connection: {}",
            egress.direct.private_names.len(),
            egress.direct.private_names.join(", ")
        ));
    }
    serde_json::json!({
        "mode": egress.mode,
        "plain": egress.plain_exit(),
        "plain_user_agent": egress.plain_user_agent(),
        "host_personas": if direct { egress.direct.personas } else { 0 },
        "level_one": kage_http::level_one_available(config),
        "warnings": warnings,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{DEFAULT_CONFIG_FILE, check, config_file};

    const DB_URL: &str = "postgres://kage:db-pass-4f9a@db.internal:5432/kage";
    const TOKEN: &str = "sidecar-token-77c1";
    // 32 bytes as 64 hexadecimal characters.
    const MASTER: &str = "0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e";

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

        // The detection pages, each with the floor of a person (spec §25).
        assert_eq!(policies["domains"], 7);
        assert_eq!(
            policies["pacing_profiles"],
            serde_json::json!(["fast", "human"])
        );
        assert!(policies["signature_rules"].as_u64().unwrap() >= 25);
        assert!(policies["signature_hints"].as_u64().unwrap() >= 10);
    }

    #[test]
    fn the_proxy_provider_in_force_is_reported() {
        let report = |extra: &[(&str, &str)]| -> serde_json::Value {
            serde_json::from_str(&run(None, env(&shipped_policies(), extra)).unwrap()).unwrap()
        };

        // Nothing configured: no provider, and the pool's defaults are shown.
        let none = report(&[]);
        assert_eq!(none["proxies"]["provider"], "none");
        assert_eq!(none["proxies"]["static_proxies"], 0);
        assert_eq!(none["proxies"]["reputation"], false);
        assert_eq!(none["config"]["proxies"]["provider"], "auto");
        assert_eq!(none["config"]["proxies"]["replacements_per_month"], 5);
        assert_eq!(none["config"]["proxies"]["health"]["profile"], "chrome_149");
        assert_eq!(
            none["config"]["webshare"]["base_url"],
            "https://proxy.webshare.io/api/"
        );

        let webshare = report(&[
            ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa"),
            ("KAGE__REPUTATION__ABUSEIPDB_KEY", "abuse-key-93bd"),
        ]);
        assert_eq!(webshare["proxies"]["provider"], "webshare");
        assert_eq!(webshare["proxies"]["reputation"], true);

        // A static list is counted, and never printed.
        let output = run(
            None,
            env(
                &shipped_policies(),
                &[(
                    "KAGE__PROXIES__STATIC_LIST",
                    "198.51.100.10:8080:user-c41d:pass-77aa\n198.51.100.11:8080:user-c41d:pass-77aa",
                )],
            ),
        )
        .unwrap();
        assert!(!output.contains("pass-77aa") && !output.contains("user-c41d"));
        let fixed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(fixed["proxies"]["provider"], "static");
        assert_eq!(fixed["proxies"]["static_proxies"], 2);
        assert_eq!(fixed["config"]["proxies"]["static_list"], "[redacted]");
    }

    #[test]
    fn where_requests_leave_from_is_reported_and_a_crossing_is_refused() {
        let report = |file: Option<&Path>, extra: &[(&str, &str)]| -> serde_json::Value {
            serde_json::from_str(&run(file, env(&shipped_policies(), extra)).unwrap()).unwrap()
        };

        // The pool, as in 1.x: level 0 through it, nothing to look at.
        let pooled = report(None, &[]);
        assert_eq!(pooled["egress"]["mode"], "proxies");
        assert_eq!(pooled["egress"]["plain"], "pool");
        assert_eq!(pooled["egress"]["host_personas"], 0);
        assert_eq!(pooled["egress"]["level_one"], false);
        assert_eq!(pooled["egress"]["warnings"], serde_json::json!([]));
        assert!(
            pooled["egress"]["plain_user_agent"]
                .as_str()
                .unwrap()
                .starts_with("kage/")
        );

        // The host's own connection, with an idle pool and private names.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("kage.yaml");
        std::fs::write(
            &file,
            "egress:\n  mode: direct\n  direct:\n    personas: 2\n    private_names: [fixture.kage.test]\n",
        )
        .unwrap();
        let direct = report(
            Some(&file),
            &[
                ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa"),
                ("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "149"),
            ],
        );
        assert_eq!(direct["egress"]["mode"], "direct");
        assert_eq!(direct["egress"]["plain"], "host");
        assert_eq!(direct["egress"]["host_personas"], 2);
        assert_eq!(direct["egress"]["level_one"], true);
        let warnings = direct["egress"]["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].as_str().unwrap().contains("the pool is idle"));
        assert!(warnings[1].as_str().unwrap().contains("fixture.kage.test"));

        // Neither a declared country nor a lookup: only level 0.
        let blind_file = dir.path().join("blind.yaml");
        std::fs::write(
            &blind_file,
            "egress:\n  mode: direct\nproxies:\n  health:\n    geo_url: null\n",
        )
        .unwrap();
        let blind = report(Some(&blind_file), &[]);
        let warnings = blind["egress"]["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("only level 0 works")),
            "{warnings:?}"
        );

        // Both crossings are refused.
        for (mode, plain, said) in [
            ("proxies", "host", "egress.plain host is refused"),
            ("direct", "pool", "egress.plain pool is refused"),
        ] {
            let message = format!(
                "{:#}",
                run(
                    None,
                    env(
                        &shipped_policies(),
                        &[("KAGE__EGRESS__MODE", mode), ("KAGE__EGRESS__PLAIN", plain)],
                    ),
                )
                .unwrap_err()
            );
            assert!(message.contains(said), "{message}");
        }
    }

    #[test]
    fn a_probe_profile_this_build_does_not_ship_fails_the_check() {
        let error = run(
            None,
            env(
                &shipped_policies(),
                &[("KAGE__PROXIES__HEALTH__PROFILE", "chrome_1")],
            ),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("configuration is invalid"), "{message}");
        assert!(message.contains("proxies.health.profile"), "{message}");
        assert!(message.contains("chrome_149"), "{message}");

        // A master key that is not 32 bytes of hex fails earlier, and is
        // not echoed.
        let mut pairs = env(&shipped_policies(), &[]);
        pairs.retain(|(name, _)| name != "KAGE__SECRETS__MASTER_KEY");
        pairs.push((
            "KAGE__SECRETS__MASTER_KEY".to_owned(),
            "not-a-key-5a5a".to_owned(),
        ));
        let message = format!("{:#}", run(None, pairs).unwrap_err());
        assert!(message.contains("64 hexadecimal characters"), "{message}");
        assert!(!message.contains("not-a-key-5a5a"), "{message}");
    }

    #[test]
    fn the_http_path_reports_its_profiles_and_refuses_an_engine_it_cannot_serve() {
        let report: serde_json::Value =
            serde_json::from_str(&run(None, env(&shipped_policies(), &[])).unwrap()).unwrap();
        let http = &report["http"];
        assert_eq!(http["exact_profiles"]["chromium"][7], 149);
        assert_eq!(http["exact_profiles"]["firefox"][7], 151);
        assert_eq!(http["verified_majors"], serde_json::json!([]));
        assert!(http["engines"]["chromium"]["browser_major"].is_null());
        assert_eq!(http["fingerprint_fixtures"], 48);
        // The defaults of the new sections are part of the effective
        // configuration.
        assert_eq!(report["config"]["http"]["max_redirects"], 10);
        assert_eq!(report["config"]["http"]["fetch_subresources"], false);
        assert_eq!(
            report["config"]["selftest"]["http"]["third_party_url"],
            "https://tls.peet.ws/api/all"
        );

        // An engine whose browser has an exact profile.
        let served: serde_json::Value = serde_json::from_str(
            &run(
                None,
                env(
                    &shipped_policies(),
                    &[("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "149")],
                ),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            served["http"]["engines"]["chromium"]["profile"],
            "chrome_149"
        );

        // The engines of this release, Chrome 155 and Camoufox 156: a
        // capture of each proved that no profile fits, so the check accepts
        // them and says that their personas are browser-only.
        let released: serde_json::Value = serde_json::from_str(
            &run(
                None,
                env(
                    &shipped_policies(),
                    &[
                        ("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "155"),
                        ("KAGE__ENGINES__FIREFOX__BROWSER_MAJOR", "156"),
                    ],
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let http = &released["http"];
        assert_eq!(http["engines"]["firefox"]["browser_major"], 156);
        assert!(http["engines"]["firefox"]["profile"].is_null());
        assert!(http["engines"]["chromium"]["profile"].is_null());
        assert_eq!(
            http["browser_only_majors"],
            serde_json::json!(["chromium-155", "firefox-156"])
        );
        assert_eq!(http["browser_captures"], 2);

        // An engine upgrade past the captures must not pass unnoticed.
        let error = run(
            None,
            env(
                &shipped_policies(),
                &[("KAGE__ENGINES__FIREFOX__BROWSER_MAJOR", "153")],
            ),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("configuration is invalid"), "{message}");
        assert!(
            message.contains("engines.firefox.browser_major"),
            "{message}"
        );
        assert!(message.contains("firefox 153"), "{message}");

        // Firefox on Linux cannot be the probe's profile.
        let error = run(
            None,
            env(
                &shipped_policies(),
                &[
                    ("KAGE__PROXIES__HEALTH__PROFILE", "firefox_151"),
                    ("KAGE__PROXIES__HEALTH__PLATFORM", "linux"),
                ],
            ),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("proxies.health.profile"), "{message}");
        assert!(message.contains("User-Agent"), "{message}");
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
