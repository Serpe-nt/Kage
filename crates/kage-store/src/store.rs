//! The connection pool, the migrations and the repositories.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kage_core::config::DatabaseConfig;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

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
///
/// A server that may no longer hold its database's lock fences its store
/// ([`Store::fence`]): from that instant nothing it writes reaches the
/// database (QA round four lock-01).
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    fenced: Arc<AtomicBool>,
}

/// The prefix of the `application_name` of a server's sessions; the rest is
/// the server's own id, so that it can find its own sessions and end them
/// ([`Store::end_own_sessions`]).
pub const APPLICATION_NAME_PREFIX: &str = "kage ";

impl Store {
    /// Connects with the configured pool size and timeout.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    ///
    /// Every session of the store, and the server lock's, is named
    /// `kage <id>` with an id of this store's own (`application_name`),
    /// whatever the URL names.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(config.url.expose())?.application_name(&format!(
            "{APPLICATION_NAME_PREFIX}{}",
            uuid::Uuid::now_v7().simple()
        ));
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.connect_timeout.get())
            .connect_with(options)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Wraps an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            fenced: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The underlying pool.
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes every connection. Queries after this fail.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Fences the store at once: from this call on, no statement of this
    /// store's leaves for the database. The pool is closed: what waits for
    /// a connection and what asks for one later is refused at once
    /// (`PoolClosed`), and a connection given back is closed, not used
    /// again. Every clone of the store is fenced with it, and so is
    /// whatever shares its pool (the artifact store). A statement already
    /// on its way when the store is fenced is not called back: see
    /// [`Store::end_own_sessions`].
    ///
    /// A server fences its store when it may no longer hold its database's
    /// lock (ADR 0005; QA round four lock-01): another server may take the
    /// lock soon, and the jobs this one held are claimed again once their
    /// leases run out.
    pub fn fence(&self) {
        if !self.fenced.swap(true, Ordering::SeqCst) {
            // The pool is marked closed by the call itself; the future
            // only waits for the connections in use to come back, which a
            // cut network may hold up for long.
            let closing = self.pool.close();
            drop(closing);
        }
    }

    /// Whether the store was fenced ([`Store::fence`]).
    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    /// The `application_name` this store's sessions carry, when it named
    /// them ([`Store::connect`]).
    pub fn application_name(&self) -> Option<String> {
        self.pool
            .connect_options()
            .get_application_name()
            .filter(|name| name.starts_with(APPLICATION_NAME_PREFIX))
            .map(str::to_owned)
    }

    /// Ends every session of this store's in the database, the server
    /// lock's among them, over a connection of its own that is closed
    /// afterwards; within `within`. A fenced server does this on its way
    /// out: a session its process can no longer close (a network that was
    /// cut while it dropped the connection) would otherwise hold the lock,
    /// and whatever was sent on it, until Postgres gives it up about a
    /// minute later (QA round four lock-02). Returns how many sessions
    /// were ended.
    ///
    /// # Errors
    ///
    /// Fails when the store's sessions carry no name of its own, or the
    /// database cannot be reached within `within`.
    pub async fn end_own_sessions(&self, within: Duration) -> Result<u64, StoreError> {
        let Some(name) = self.application_name() else {
            return Err(StoreError::Unnamed);
        };
        let options = (*self.pool.connect_options()).clone();
        let ending = async move {
            let mut connection = PgConnection::connect_with(&options).await?;
            let ended: i64 = sqlx::query_scalar(
                "SELECT count(*) FILTER (WHERE pg_terminate_backend(pid)) FROM pg_stat_activity \
                 WHERE application_name = $1 AND pid <> pg_backend_pid()",
            )
            .bind(&name)
            .fetch_one(&mut connection)
            .await?;
            connection.close().await?;
            Ok::<_, StoreError>(u64::try_from(ended).unwrap_or(0))
        };
        tokio::time::timeout(within, ending)
            .await
            .map_err(|_| StoreError::Unreachable(within))?
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
