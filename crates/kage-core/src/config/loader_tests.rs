//! What the loader takes and what it cannot mean (F132, F133, F136, F137).

use std::time::Duration;

use super::{Config, ConfigError, MAX_DATABASE_CONNECTIONS, defaults};
use crate::domain::Engine;

const DB_URL: &str = "postgres://kage:db-pass-4f9a@db.internal:5432/kage";
const TOKEN: &str = "sidecar-token-77c1";
const MASTER: &str = "0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e";

fn with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = [
        ("KAGE__DATABASE__URL", DB_URL),
        ("KAGE__SIDECAR__TOKEN", TOKEN),
        ("KAGE__SECRETS__MASTER_KEY", MASTER),
    ]
    .iter()
    .chain(extra)
    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
    .collect();
    // The last one given wins, as in a process's environment.
    all.dedup_by(|later, earlier| later.0 == earlier.0);
    all
}

fn refused(result: Result<Config, ConfigError>) -> String {
    match result {
        Err(ConfigError::Invalid(message)) => message,
        other => panic!("expected an invalid configuration, got {other:?}"),
    }
}

#[test]
fn the_defaults_layer_reads_back_as_the_defaults_and_holds_no_secret() {
    let tree = defaults().unwrap();
    for secret in super::SECRETS {
        let (section, key) = secret.key.split_once('.').unwrap();
        assert!(tree[section].get(key).is_none(), "{}", secret.key);
    }
    let loaded = Config::load(None, with(&[])).unwrap();
    // Everything but the secrets is the defaults' own.
    let mut shown = loaded.redacted();
    for secret in super::SECRETS {
        let (section, key) = secret.key.split_once('.').unwrap();
        if let Some(section) = shown
            .get_mut(section)
            .and_then(serde_json::Value::as_object_mut)
        {
            section.remove(key);
        }
    }
    assert_eq!(shown, tree);
}

#[test]
fn a_member_of_an_object_is_set_alone_by_the_environment_and_by_the_file() {
    // F132 (cli-01): every one of the twenty keys that were refused.
    let defaults = Config::load(None, with(&[])).unwrap();
    let most = defaults.personas.warmup.visits.max + 5;
    let alone = Config::load(
        None,
        with(&[("KAGE__PERSONAS__WARMUP__VISITS__MAX", &most.to_string())]),
    )
    .unwrap();
    assert_eq!(alone.personas.warmup.visits.max, most);
    assert_eq!(
        alone.personas.warmup.visits.min,
        defaults.personas.warmup.visits.min
    );
    // Each of them set to its own default, alone, as cli-01 asked.
    let tree = super::defaults().unwrap();
    for key in [
        "browser.scroll_depth.min",
        "browser.scroll_depth.max",
        "personas.warmup.active_hours.from",
        "personas.warmup.active_hours.until",
        "personas.warmup.first_visit_after.min",
        "personas.warmup.first_visit_after.max",
        "personas.warmup.gap.median",
        "personas.warmup.gap.p95",
        "personas.warmup.visits.min",
        "personas.warmup.visits.max",
        "scheduler.active_hours.up.from",
        "scheduler.active_hours.up.until",
        "scheduler.active_hours.to_bed.from",
        "scheduler.active_hours.to_bed.until",
        "scheduler.retry_pause.min",
        "scheduler.retry_pause.median",
        "scheduler.retry_pause.p95",
        "scheduler.visit_start_spacing.min",
        "scheduler.visit_start_spacing.median",
        "scheduler.visit_start_spacing.p95",
    ] {
        let value = key
            .split('.')
            .fold(&tree, |value, member| &value[member])
            .clone();
        // A text goes in quotes in figment's grammar, a number as it is.
        let value = match value {
            serde_json::Value::String(text) => format!("{text:?}"),
            other => other.to_string(),
        };
        let name = format!("KAGE__{}", key.to_ascii_uppercase().replace('.', "__"));
        let loaded = Config::load(None, with(&[(name.as_str(), value.as_str())]));
        assert!(loaded.is_ok(), "{name}={value}: {loaded:?}");
    }

    // The same in a file that holds nothing else.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kage.yaml");
    std::fs::write(&file, "personas:\n  warmup:\n    visits:\n      max: 7\n").unwrap();
    let from_file = Config::load(Some(&file), with(&[])).unwrap();
    assert_eq!(from_file.personas.warmup.visits.max, 7);
    assert_eq!(
        from_file.personas.warmup.visits.min,
        defaults.personas.warmup.visits.min
    );
    // The environment over the file, member by member.
    let both = Config::load(
        Some(&file),
        with(&[("KAGE__PERSONAS__WARMUP__VISITS__MIN", "3")]),
    )
    .unwrap();
    assert_eq!(
        (
            both.personas.warmup.visits.min,
            both.personas.warmup.visits.max
        ),
        (3, 7)
    );
    // A list is still replaced whole, not merged with the default.
    std::fs::write(
        &file,
        "personas:\n  mix:\n    - { engine: firefox, os: windows, weight: 1 }\n",
    )
    .unwrap();
    let mix = Config::load(Some(&file), with(&[])).unwrap().personas.mix;
    assert_eq!(mix.len(), 1);
    assert_eq!(mix[0].engine, Engine::Firefox);
}

#[test]
fn null_from_the_environment_switches_an_optional_key_off() {
    // F137 (cli-13): as `null` in the file does.
    let on = Config::load(None, with(&[])).unwrap();
    assert!(on.proxies.health.geo_url.is_some());
    let off = Config::load(
        None,
        with(&[
            ("KAGE__PROXIES__HEALTH__GEO_URL", "null"),
            ("KAGE__PROXIES__HEALTH__CONFIRM_URL", " null "),
        ]),
    )
    .unwrap();
    assert_eq!(off.proxies.health.geo_url, None);
    assert_eq!(off.proxies.health.confirm_url, None);
    // A key that must have a value says so.
    let message = refused(Config::load(None, with(&[("KAGE__SERVER__BIND", "null")])));
    assert!(message.contains("server.bind"), "{message}");
}

#[test]
fn a_url_key_refuses_credentials_and_never_repeats_them() {
    // F133 (cli-06).
    for (name, value) in [
        ("KAGE__SIDECAR__CHROMIUM_URL", "http://u:p4ss-w0rd@a.test:1"),
        ("KAGE__SIDECAR__FIREFOX_URL", "http://u:p4ss-w0rd@a.test:1"),
        (
            "KAGE__SERVER__CORS_ALLOWED_ORIGINS",
            "[\"https://u:p4ss-w0rd@console.test\"]",
        ),
        (
            "KAGE__SELFTEST__BROWSER__PAGES",
            "[{id=\"own\",kind=\"echo\",url=\"https://u:p4ss-w0rd@echo.test/\"}]",
        ),
    ] {
        let message = refused(Config::load(None, with(&[(name, value)])));
        assert!(!message.contains("p4ss-w0rd"), "{name}: {message}");
        assert!(
            message.contains("credentials") || message.contains("not an origin"),
            "{name}: {message}"
        );
    }
}

#[test]
fn a_database_url_names_only_parameters_the_driver_reads() {
    // F133 (cli-08): a misspelt parameter would be logged with its value.
    let message = refused(Config::load(
        None,
        with(&[(
            "KAGE__DATABASE__URL",
            "postgres://kage@db/kage?passwd=s3cret-v4lue",
        )]),
    ));
    assert!(message.contains("\"passwd\""), "{message}");
    assert!(!message.contains("s3cret-v4lue"), "{message}");
    for url in [
        "postgres://kage@db/kage?sslmode=require&application_name=kage",
        "postgres://kage@db/kage?options[search_path]=kage&statement-cache-capacity=100",
        "postgresql://kage:pw@db:5432/kage",
    ] {
        assert!(
            Config::load(None, with(&[("KAGE__DATABASE__URL", url)])).is_ok(),
            "{url}"
        );
    }
}

#[test]
fn values_the_program_cannot_count_with_are_refused() {
    // F137 (cli-07): each of them panicked a sweep or exhausted memory.
    for (name, value, key) in [
        (
            "KAGE__PERSONAS__RETENTION_DAYS",
            "4294967295",
            "personas.retention_days",
        ),
        (
            "KAGE__PERSONAS__ARCHIVE_AFTER",
            "99999999d",
            "personas.archive_after",
        ),
        (
            "KAGE__PROXIES__HEALTH__INTERVAL",
            "99999999d",
            "proxies.health.interval",
        ),
        (
            "KAGE__DATABASE__MAX_CONNECTIONS",
            "4294967295",
            "database.max_connections",
        ),
    ] {
        let message = refused(Config::load(None, with(&[(name, value)])));
        assert!(message.starts_with(key), "{name}: {message}");
    }
    let most = MAX_DATABASE_CONNECTIONS.to_string();
    let at_most = Config::load(None, with(&[("KAGE__DATABASE__MAX_CONNECTIONS", &most)]));
    assert!(at_most.is_ok());
    let day = Config::load(None, with(&[("KAGE__PROXIES__HEALTH__INTERVAL", "1d")])).unwrap();
    assert_eq!(day.proxies.health.interval.get(), Duration::from_hours(24));
}

#[test]
fn the_host_personas_engines_are_the_mixs() {
    // F137 (cli-09).
    let direct = [
        ("KAGE__EGRESS__MODE", "direct"),
        ("KAGE__EGRESS__DIRECT__COUNTRY", "DE"),
        (
            "KAGE__PERSONAS__MIX",
            "[{engine=\"firefox\",os=\"windows\",weight=1}]",
        ),
    ];
    let mut outside = direct.to_vec();
    outside.push(("KAGE__EGRESS__DIRECT__ENGINES", "[\"chromium\"]"));
    let message = refused(Config::load(None, with(&outside)));
    assert!(
        message.starts_with("egress.direct.engines names chromium"),
        "{message}"
    );
    let mut inside = direct.to_vec();
    inside.push(("KAGE__EGRESS__DIRECT__ENGINES", "[\"firefox\"]"));
    assert!(Config::load(None, with(&inside)).is_ok());
}

#[test]
fn a_wildcard_is_no_private_name_and_no_origin() {
    // F136 (auth-05, cli-12).
    let message = refused(Config::load(
        None,
        with(&[
            ("KAGE__EGRESS__MODE", "direct"),
            ("KAGE__EGRESS__DIRECT__COUNTRY", "DE"),
            ("KAGE__EGRESS__DIRECT__PRIVATE_NAMES", "[\"*.kage.test\"]"),
        ]),
    ));
    assert!(message.contains("wildcard"), "{message}");
    let message = refused(Config::load(
        None,
        with(&[(
            "KAGE__SERVER__CORS_ALLOWED_ORIGINS",
            "[\"https://*.example.invalid\"]",
        )]),
    ));
    assert!(
        message.starts_with("server.cors_allowed_origins"),
        "{message}"
    );
}
