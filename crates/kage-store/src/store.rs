//! The connection pool, the migrations and the repositories.

use kage_core::config::DatabaseConfig;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

use crate::StoreError;
use crate::repo::{
    ApiKeyRepo, ArtifactRepo, AttemptRepo, AuditRepo, BandwidthRepo, BreakerRepo, FrontierRepo,
    HealthEventRepo, HttpCookieRepo, JobItemRepo, JobRepo, PersonaDomainStateRepo, PersonaRepo,
    ProxyRepo, RequestRepo, ResponseCacheRepo, ResultRepo, RobotsCacheRepo, SelftestRepo,
    SessionRepo, SiteExitRepo, SiteScoutRepo, WebhookRepo,
};

/// The migrations in `crates/kage-store/migrations`, embedded at compile time.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// One migration and whether the database has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationState {
    /// The migration's version number.
    pub version: i64,
    /// Its description, from the file name.
    pub description: String,
    /// Whether it has been applied.
    pub applied: bool,
    /// Whether the applied copy is the one this binary carries. `None` when
    /// it has not been applied.
    pub checksum_matches: Option<bool>,
}

/// The state of every migration this binary carries, plus any the database
/// has that this binary does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// One entry per known migration, in version order.
    pub migrations: Vec<MigrationState>,
    /// Versions applied to the database that this binary does not carry: the
    /// database is ahead of the binary.
    pub unknown_versions: Vec<i64>,
}

impl MigrationStatus {
    /// Known migrations that have not been applied.
    pub fn pending(&self) -> Vec<i64> {
        self.migrations
            .iter()
            .filter(|migration| !migration.applied)
            .map(|migration| migration.version)
            .collect()
    }

    /// Applied migrations whose content differs from this binary's copy.
    pub fn modified(&self) -> Vec<i64> {
        self.migrations
            .iter()
            .filter(|migration| migration.checksum_matches == Some(false))
            .map(|migration| migration.version)
            .collect()
    }

    /// Whether the schema is exactly what this binary expects.
    pub fn is_clean(&self) -> bool {
        self.pending().is_empty() && self.modified().is_empty() && self.unknown_versions.is_empty()
    }
}

/// Access to Postgres: the pool, the migrations and one repository per
/// group of tables.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects with the configured pool size and timeout.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.connect_timeout.get())
            .connect(config.url.expose())
            .await?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool.
    pub const fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool.
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes every connection. Queries after this fail.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Applies every migration that has not been applied yet. Running it
    /// again changes nothing.
    ///
    /// # Errors
    ///
    /// Fails when a migration cannot be applied or an applied migration was
    /// modified afterwards.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Compares the database with the migrations this binary carries.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be queried.
    pub async fn migration_status(&self) -> Result<MigrationStatus, StoreError> {
        let table_exists: bool =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        let applied: Vec<(i64, bool, Vec<u8>)> = if table_exists {
            sqlx::query_as(
                "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        let migrations: Vec<MigrationState> = MIGRATOR
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| {
                let found = applied
                    .iter()
                    .find(|(version, success, _)| *version == migration.version && *success);
                MigrationState {
                    version: migration.version,
                    description: migration.description.to_string(),
                    applied: found.is_some(),
                    checksum_matches: found
                        .map(|(_, _, checksum)| checksum.as_slice() == &*migration.checksum),
                }
            })
            .collect();
        let unknown_versions = applied
            .iter()
            .map(|(version, _, _)| *version)
            .filter(|version| !MIGRATOR.version_exists(*version))
            .collect();

        Ok(MigrationStatus {
            migrations,
            unknown_versions,
        })
    }

    /// Proxies.
    pub const fn proxies(&self) -> ProxyRepo<'_> {
        ProxyRepo::new(&self.pool)
    }

    /// Cookie jars of the http path.
    pub const fn http_cookies(&self) -> HttpCookieRepo<'_> {
        HttpCookieRepo::new(&self.pool)
    }

    /// Personas.
    pub const fn personas(&self) -> PersonaRepo<'_> {
        PersonaRepo::new(&self.pool)
    }

    /// Per-domain persona state.
    pub const fn persona_domain_state(&self) -> PersonaDomainStateRepo<'_> {
        PersonaDomainStateRepo::new(&self.pool)
    }

    /// API keys.
    pub const fn api_keys(&self) -> ApiKeyRepo<'_> {
        ApiKeyRepo::new(&self.pool)
    }

    /// Sessions.
    pub const fn sessions(&self) -> SessionRepo<'_> {
        SessionRepo::new(&self.pool)
    }

    /// Requests.
    pub const fn requests(&self) -> RequestRepo<'_> {
        RequestRepo::new(&self.pool)
    }

    /// Attempts.
    pub const fn attempts(&self) -> AttemptRepo<'_> {
        AttemptRepo::new(&self.pool)
    }

    /// Results.
    pub const fn results(&self) -> ResultRepo<'_> {
        ResultRepo::new(&self.pool)
    }

    /// Artifact rows. The bytes are handled by [`crate::ArtifactStore`].
    pub const fn artifacts(&self) -> ArtifactRepo<'_> {
        ArtifactRepo::new(&self.pool)
    }

    /// Jobs.
    pub const fn jobs(&self) -> JobRepo<'_> {
        JobRepo::new(&self.pool)
    }

    /// Webhook deliveries and the log of their attempts.
    pub const fn webhooks(&self) -> WebhookRepo<'_> {
        WebhookRepo::new(&self.pool)
    }

    /// Job items.
    pub const fn job_items(&self) -> JobItemRepo<'_> {
        JobItemRepo::new(&self.pool)
    }

    /// The crawl frontier.
    pub const fn crawl_frontier(&self) -> FrontierRepo<'_> {
        FrontierRepo::new(&self.pool)
    }

    /// The robots.txt cache.
    pub const fn robots_cache(&self) -> RobotsCacheRepo<'_> {
        RobotsCacheRepo::new(&self.pool)
    }

    /// The persona that asks for a site's `robots.txt` and sitemaps.
    pub const fn site_scouts(&self) -> SiteScoutRepo<'_> {
        SiteScoutRepo::new(&self.pool)
    }

    /// The address a site's plain traffic leaves from under the pool.
    pub const fn site_exits(&self) -> SiteExitRepo<'_> {
        SiteExitRepo::new(&self.pool)
    }

    /// The response cache.
    pub const fn response_cache(&self) -> ResponseCacheRepo<'_> {
        ResponseCacheRepo::new(&self.pool)
    }

    /// The audit trail.
    pub const fn audit_events(&self) -> AuditRepo<'_> {
        AuditRepo::new(&self.pool)
    }

    /// Health events.
    pub const fn health_events(&self) -> HealthEventRepo<'_> {
        HealthEventRepo::new(&self.pool)
    }

    /// Self-test runs.
    pub const fn selftest_runs(&self) -> SelftestRepo<'_> {
        SelftestRepo::new(&self.pool)
    }

    /// Per-domain circuit breakers.
    pub const fn domain_breakers(&self) -> BreakerRepo<'_> {
        BreakerRepo::new(&self.pool)
    }

    /// Bytes through the forwarder.
    pub const fn bandwidth(&self) -> BandwidthRepo<'_> {
        BandwidthRepo::new(&self.pool)
    }
}

#[cfg(test)]
mod tests {
    use super::{MigrationState, MigrationStatus};

    fn state(version: i64, applied: bool, checksum_matches: Option<bool>) -> MigrationState {
        MigrationState {
            version,
            description: format!("migration {version}"),
            applied,
            checksum_matches,
        }
    }

    #[test]
    fn a_fully_applied_schema_is_clean() {
        let status = MigrationStatus {
            migrations: vec![state(1, true, Some(true)), state(2, true, Some(true))],
            unknown_versions: Vec::new(),
        };

        assert!(status.is_clean());
        assert_eq!(status.pending(), Vec::<i64>::new());
        assert_eq!(status.modified(), Vec::<i64>::new());
    }

    #[test]
    fn pending_modified_and_unknown_migrations_are_each_reported() {
        let pending = MigrationStatus {
            migrations: vec![state(1, true, Some(true)), state(2, false, None)],
            unknown_versions: Vec::new(),
        };
        assert_eq!(pending.pending(), [2]);
        assert!(!pending.is_clean());

        let modified = MigrationStatus {
            migrations: vec![state(1, true, Some(false))],
            unknown_versions: Vec::new(),
        };
        assert_eq!(modified.modified(), [1]);
        assert!(!modified.is_clean());

        let ahead = MigrationStatus {
            migrations: vec![state(1, true, Some(true))],
            unknown_versions: vec![9],
        };
        assert!(!ahead.is_clean());
    }
}
