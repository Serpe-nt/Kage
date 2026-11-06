//! One server per database (ADR 0005, F174).
//!
//! Kage is single-instance: one `kage serve` works one database. A server
//! holds a Postgres session-level advisory lock ([`SERVER_LOCK`]) for its
//! whole life, on a connection of its own outside the pool; a second
//! server on the same database finds the lock taken and refuses to start.
//!
//! An advisory lock rather than a heartbeat row with a lease: Postgres
//! lets the lock go the moment the session that holds it ends, whatever
//! ended it (a stop, a kill, a crash of the process or of its container),
//! so a server started again after a kill starts at once, with nothing to
//! wait out and nothing stale to judge. The session keeps TCP keepalives
//! short ([`KEEPALIVES`]), so that a host that
//! vanished without closing its connection lets the lock go within about a
//! minute, not after the system's two hours. The lock needs a session that
//! stays one connection: a transaction-pooling proxy in front of Postgres
//! would not keep it, and Kage's compose has none.

use sqlx::{Connection, Executor, PgConnection, PgPool};

use crate::StoreError;

/// The key of the advisory lock a server holds on its database: the bytes
/// of `kageserv`. Nothing else in Kage takes an advisory lock.
pub const SERVER_LOCK: i64 = 0x6b61_6765_7365_7276;

/// The TCP keepalives of the lock's session: after 30 s of silence
/// Postgres asks whether the server is still there, every 10 s, and gives
/// the connection up, and the lock with it, after 3 questions without an
/// answer.
pub const KEEPALIVES: &str = "SET tcp_keepalives_idle = 30; \
     SET tcp_keepalives_interval = 10; SET tcp_keepalives_count = 3";

/// The lock a server holds on its database while it runs. Dropping it
/// closes its connection, and Postgres lets the lock go.
#[derive(Debug)]
pub struct ServerLock {
    connection: PgConnection,
}

impl ServerLock {
    /// Takes the lock on the database of `pool`, on a connection of its
    /// own. `None` when another session holds it: another server runs on
    /// the database.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn take(pool: &PgPool) -> Result<Option<Self>, StoreError> {
        let mut connection = PgConnection::connect_with(&pool.connect_options()).await?;
        // A peer that vanished is found out within about a minute.
        connection.execute(KEEPALIVES).await?;
        let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SERVER_LOCK)
            .fetch_one(&mut connection)
            .await?;
        if held {
            Ok(Some(Self { connection }))
        } else {
            connection.close().await?;
            Ok(None)
        }
    }

    /// Whether the lock is still this server's: its session answers and
    /// holds it.
    ///
    /// # Errors
    ///
    /// Fails when the session does not answer: the connection is gone, and
    /// the lock with it.
    pub async fn held(&mut self) -> Result<bool, StoreError> {
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' \
             AND pid = pg_backend_pid() AND granted AND objsubid = 1 \
             AND ((classid::bigint << 32) | objid::bigint) = $1)",
        )
        .bind(SERVER_LOCK)
        .fetch_one(&mut self.connection)
        .await?;
        Ok(held)
    }

    /// Lets the lock go and closes its connection.
    ///
    /// # Errors
    ///
    /// Fails when the database does not answer; the lock goes with the
    /// connection all the same.
    pub async fn release(mut self) -> Result<(), StoreError> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(SERVER_LOCK)
            .execute(&mut self.connection)
            .await?;
        self.connection.close().await?;
        Ok(())
    }
}
