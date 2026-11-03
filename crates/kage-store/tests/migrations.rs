//! The migrations: they apply from an empty database, applying them again
//! changes nothing, and the schema they build is the one spec §18 lists.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use chrono::Duration;
use common::{TestDb, now};
use kage_core::config::Config;
use kage_store::repo::RobotsCacheEntry;
use kage_store::{Store, StoreError};

/// Every table spec §18 names. Milestone M6 added columns, no table; M7
/// added `bandwidth_usage`, M9 the two tables of the webhook deliveries,
/// M15 `site_exits`.
const SPEC_TABLES: [&str; 24] = [
    "api_keys",
    "artifacts",
    "attempts",
    "audit_events",
    "bandwidth_usage",
    "crawl_frontier",
    "domain_breakers",
    "health_events",
    "http_cookies",
    "job_items",
    "jobs",
    "persona_domain_state",
    "personas",
    "proxies",
    "requests",
    "response_cache",
    "results",
    "robots_cache",
    "selftest_runs",
    "sessions",
    "site_exits",
    "site_scouts",
    "webhook_attempts",
    "webhook_deliveries",
];

async fn tables(store: &Store) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'
         ORDER BY table_name",
    )
    .fetch_all(store.pool())
    .await
    .unwrap()
}

async fn applied_count(store: &Store) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn migrations_apply_from_an_empty_database() {
    let db = TestDb::empty().await;

    // Before: nothing applied, nothing created, and the status says so.
    assert_eq!(tables(&db.store).await, Vec::<String>::new());
    let before = db.store.migration_status().await.unwrap();
    assert!(!before.is_clean());
    assert_eq!(before.pending().len(), before.migrations.len());
    assert!(before.migrations.iter().all(|migration| !migration.applied));
    assert!(before.migrations.len() >= 7);

    db.store.migrate().await.unwrap();

    // After: exactly the tables of spec §18.
    assert_eq!(tables(&db.store).await, SPEC_TABLES);
    let after = db.store.migration_status().await.unwrap();
    assert!(after.is_clean(), "{after:?}");
    assert_eq!(after.pending(), Vec::<i64>::new());
    assert_eq!(after.modified(), Vec::<i64>::new());
    assert_eq!(after.unknown_versions, Vec::<i64>::new());
    assert!(
        after
            .migrations
            .iter()
            .all(|migration| migration.applied && migration.checksum_matches == Some(true))
    );
    // Versions are consecutive from 1, so none was skipped or renumbered.
    let versions: Vec<i64> = after
        .migrations
        .iter()
        .map(|migration| migration.version)
        .collect();
    let expected: Vec<i64> = (1..=i64::try_from(versions.len()).unwrap()).collect();
    assert_eq!(versions, expected);
    assert!(
        after
            .migrations
            .iter()
            .all(|migration| !migration.description.is_empty())
    );
}

#[tokio::test]
async fn applying_the_migrations_again_changes_nothing() {
    let db = TestDb::migrated().await;
    let applied = applied_count(&db.store).await;
    let tables_before = tables(&db.store).await;
    // A row proves the second run did not rebuild anything.
    let proxy = common::proxy("ref-1");
    db.store.proxies().insert(&proxy).await.unwrap();

    db.store.migrate().await.unwrap();
    db.store.migrate().await.unwrap();

    assert_eq!(applied_count(&db.store).await, applied);
    assert_eq!(tables(&db.store).await, tables_before);
    assert_eq!(db.store.proxies().get(proxy.id).await.unwrap(), Some(proxy));
    assert!(db.store.migration_status().await.unwrap().is_clean());
}

#[tokio::test]
async fn status_reports_a_database_that_is_ahead_or_was_modified() {
    let db = TestDb::migrated().await;

    // A version this binary does not carry: the database is ahead.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (9999, 'from the future', true, '\\x00', 0)",
    )
    .execute(db.store.pool())
    .await
    .unwrap();
    let ahead = db.store.migration_status().await.unwrap();
    assert_eq!(ahead.unknown_versions, [9999]);
    assert!(!ahead.is_clean());
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999")
        .execute(db.store.pool())
        .await
        .unwrap();

    // An applied migration whose file changed afterwards.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1")
        .execute(db.store.pool())
        .await
        .unwrap();
    let modified = db.store.migration_status().await.unwrap();
    assert_eq!(modified.modified(), [1]);
    assert!(!modified.is_clean());
    // Running the migrations refuses to continue on top of it.
    assert!(matches!(
        db.store.migrate().await,
        Err(StoreError::Migrate(_))
    ));
}

async fn rejects(store: &Store, sql: &'static str) -> bool {
    sqlx::query(sql).execute(store.pool()).await.is_err()
}

#[tokio::test]
async fn the_schema_rejects_values_outside_the_domain_model() {
    let db = TestDb::migrated().await;
    let store = &db.store;
    let persona = common::seed_persona(store).await;
    let key = common::seed_key(store).await;

    // Enum columns only take the names of the domain model.
    assert!(
        rejects(
            store,
            "INSERT INTO proxies (id, provider, provider_ref, host, port, username, password, state, created_at)
             VALUES (gen_random_uuid(), 'webshare', 'x', 'h', 8080, 'u', '\\x00', 'zombie', now())",
        )
        .await
    );
    assert!(
        rejects(
            store,
            "INSERT INTO proxies (id, provider, provider_ref, host, port, username, password, created_at)
             VALUES (gen_random_uuid(), 'webshare', 'y', 'h', 0, 'u', '\\x00', now())",
        )
        .await
    );
    assert!(
        rejects(
            store,
            "INSERT INTO proxies (id, provider, provider_ref, host, port, username, password, country, created_at)
             VALUES (gen_random_uuid(), 'webshare', 'z', 'h', 8080, 'u', '\\x00', 'Germany', now())",
        )
        .await
    );
    assert!(rejects(store, "UPDATE personas SET engine = 'netscape'").await);
    assert!(rejects(store, "UPDATE personas SET state = 'asleep'").await);
    assert!(rejects(store, "UPDATE api_keys SET scopes = ARRAY['fetch', 'root']").await);
    assert!(rejects(store, "UPDATE api_keys SET secret_hash = '\\x0102'").await);
    assert!(rejects(store, "UPDATE api_keys SET requests_per_minute = 0").await);

    // One persona per proxy.
    let second = sqlx::query(
        "INSERT INTO personas (id, proxy_id, engine, claimed_os, browser_major, fingerprint, locale,
                               languages, timezone, screen, profile_dir, created_at)
         SELECT gen_random_uuid(), proxy_id, engine, claimed_os, browser_major, fingerprint, locale,
                languages, timezone, screen, profile_dir, created_at
         FROM personas WHERE id = $1",
    )
    .bind(persona.id.as_uuid())
    .execute(store.pool())
    .await;
    assert!(second.is_err());

    // A session id follows the same rule as the domain type.
    let bad_session = sqlx::query(
        "INSERT INTO sessions (api_key_id, id, persona_id, created_at, last_used_at, expires_at)
         VALUES ($1, 'has space', $2, now(), now(), now())",
    )
    .bind(key.id.as_uuid())
    .bind(persona.id.as_uuid())
    .execute(store.pool())
    .await;
    assert!(bad_session.is_err());

    // Artifacts are addressed by a real SHA-256.
    assert!(
        rejects(
            store,
            "INSERT INTO artifacts (id, content_hash, kind, content_type, size_bytes, stored_bytes, encoding, created_at, expires_at)
             VALUES (gen_random_uuid(), 'nothex', 'raw_body', 'text/html', 1, 1, 'identity', now(), now())",
        )
        .await
    );
    // An audit event names a key only when its actor is a key.
    let stray_key = sqlx::query(
        "INSERT INTO audit_events (id, at, actor, api_key_id, action, outcome, details)
         VALUES (gen_random_uuid(), now(), 'cli', $1, 'x', 'success', '{}')",
    )
    .bind(key.id.as_uuid())
    .execute(store.pool())
    .await;
    assert!(stray_key.is_err());
}

#[tokio::test]
async fn the_store_connects_from_configuration() {
    let db = TestDb::migrated().await;
    let env = |url: &str| {
        vec![
            ("KAGE__DATABASE__URL".to_owned(), url.to_owned()),
            ("KAGE__DATABASE__MAX_CONNECTIONS".to_owned(), "2".to_owned()),
            (
                "KAGE__DATABASE__CONNECT_TIMEOUT".to_owned(),
                "2s".to_owned(),
            ),
            ("KAGE__SIDECAR__TOKEN".to_owned(), "token".to_owned()),
            ("KAGE__SECRETS__MASTER_KEY".to_owned(), "0f".repeat(32)),
        ]
    };

    let config = Config::load(None, env(&db.url)).unwrap();
    let store = Store::connect(&config.database).await.unwrap();
    assert!(store.migration_status().await.unwrap().is_clean());
    assert_eq!(store.pool().options().get_max_connections(), 2);
    store.close().await;
    assert!(store.migration_status().await.is_err());

    // Nothing listens here: the connection fails instead of hanging.
    let unreachable = Config::load(None, env("postgres://kage:pw@127.0.0.1:1/kage")).unwrap();
    let error = Store::connect(&unreachable.database).await.unwrap_err();
    assert!(matches!(error, StoreError::Database(_)));
    // The error never repeats the password from the URL.
    assert!(!error.to_string().contains(":pw@"));
}

/// The version of the migration that clears the empty `robots.txt` rows
/// 2.0.1 kept (F99).
const EMPTY_ROBOTS_FILES: i64 = 19;

/// A `robots_cache` row of `origin` as a scout's answer kept it.
fn robots_row(
    origin: &str,
    status: Option<u16>,
    body: Option<&str>,
    unreachable: bool,
) -> RobotsCacheEntry {
    let fetched_at = now() - Duration::hours(2);
    RobotsCacheEntry {
        origin: origin.to_owned(),
        site: "example.org".to_owned(),
        fetched_at,
        expires_at: fetched_at + Duration::hours(24),
        status,
        body: body.map(str::to_owned),
        unreachable,
    }
}

async fn robots_origins(store: &Store) -> Vec<String> {
    sqlx::query_scalar("SELECT origin FROM robots_cache ORDER BY origin")
        .fetch_all(store.pool())
        .await
        .unwrap()
}

/// Makes the database look as 2.0.1 left it: every migration but the one
/// of 2.0.2 applied, so the next `migrate` is the upgrade.
async fn forget_the_empty_files_migration(store: &Store) {
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(EMPTY_ROBOTS_FILES)
        .execute(store.pool())
        .await
        .unwrap();
    let status = store.migration_status().await.unwrap();
    assert_eq!(status.pending(), [EMPTY_ROBOTS_FILES]);
}

/// F99: until 2.0.2 a `robots.txt` the plain client of level 0 asked for
/// first was kept as a file with nothing in it (a `2xx`, an empty body),
/// so `enforce` enforced nothing for a day. The upgrade's migration
/// deletes exactly those rows, so the scout asks again, and keeps every
/// honest one: a real file, "no file" (a `404`), "could not be had".
#[tokio::test]
async fn the_upgrade_clears_the_empty_robots_files_2_0_1_kept_and_nothing_else() {
    let db = TestDb::migrated().await;
    let store = &db.store;
    forget_the_empty_files_migration(store).await;

    let rows = [
        // What the bug wrote: a fetched file with nothing in it.
        robots_row("https://empty.example.org", Some(200), Some(""), false),
        robots_row("https://empty-due.example.org", Some(204), Some(""), false),
        // A real file, however short.
        robots_row(
            "https://file.example.org",
            Some(200),
            Some("User-agent: *\nDisallow: /private\n"),
            false,
        ),
        robots_row("https://newline.example.org", Some(200), Some("\n"), false),
        // No file: everything is allowed.
        robots_row("https://missing.example.org", Some(404), None, false),
        // It could not be had: a server error, a timeout.
        robots_row("https://down.example.org", Some(503), None, true),
        robots_row("https://silent.example.org", None, None, true),
    ];
    for row in &rows {
        store.robots_cache().put(row).await.unwrap();
    }

    store.migrate().await.unwrap();

    assert!(store.migration_status().await.unwrap().is_clean());
    assert_eq!(
        robots_origins(store).await,
        [
            "https://down.example.org",
            "https://file.example.org",
            "https://missing.example.org",
            "https://newline.example.org",
            "https://silent.example.org",
        ]
    );
    // What is kept is kept as it was.
    for row in rows.iter().skip(2) {
        assert_eq!(
            store
                .robots_cache()
                .get(&row.origin)
                .await
                .unwrap()
                .as_ref(),
            Some(row)
        );
    }

    // Once: an empty file kept after the upgrade (2.0.2 keeps what the
    // site sent, and a site may send nothing) stays when the migrations
    // run again.
    let honest = robots_row("https://empty.example.org", Some(200), Some(""), false);
    store.robots_cache().put(&honest).await.unwrap();
    store.migrate().await.unwrap();
    assert_eq!(
        store.robots_cache().get(&honest.origin).await.unwrap(),
        Some(honest)
    );

    // Idempotent: run once more as if it had not been, it deletes what has
    // the bug's shape and nothing else.
    forget_the_empty_files_migration(store).await;
    store.migrate().await.unwrap();
    assert_eq!(robots_origins(store).await.len(), 5);
    assert!(store.migration_status().await.unwrap().is_clean());
}
