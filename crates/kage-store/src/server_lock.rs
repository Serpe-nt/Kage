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

use std::time::Duration;

use sqlx::{Connection, Executor, PgConnection, PgPool};

use crate::StoreError;
use crate::store::APPLICATION_NAME_PREFIX;

/// The key of the advisory lock a server holds on its database: the bytes
/// of `kageserv`. No other advisory lock of Kage's has this key (the
/// proxies' and the artifacts' are transaction locks of keys of their own).
pub const SERVER_LOCK: i64 = 0x6b61_6765_7365_7276;

/// The TCP keepalives of the lock's session: after 30 s of silence
/// Postgres asks whether the server is still there, every 10 s, and gives
/// the connection up, and the lock with it, after 3 questions without an
/// answer. Keepalives are not sent while an answer of Postgres's waits to
/// be acknowledged: the TCP user timeout gives the connection up when what
/// Postgres sent has waited 60 s for an acknowledgement (review of #98).
pub const KEEPALIVES: &str = "SET tcp_keepalives_idle = 30; \
     SET tcp_keepalives_interval = 10; SET tcp_keepalives_count = 3; \
     SET tcp_user_timeout = 60000";

/// The session that holds [`SERVER_LOCK`] on this database, when one does,
/// and its `application_name`. An advisory lock is a database's: a server
/// of another database on the same Postgres holds a lock of the same key
/// that is not this one (review of #98).
const HOLDER: &str = "SELECT l.pid, a.application_name FROM pg_locks l \
     LEFT JOIN pg_stat_activity a USING (pid) WHERE l.locktype = 'advisory' AND l.granted \
     AND l.objsubid = 1 AND ((l.classid::bigint << 32) | l.objid::bigint) = $1 \
     AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())";

/// How often [`ServerLock::take_back`] asks for the lock while the session
/// it ended lets it go.
const TAKE_BACK_TRIES: u32 = 20;

/// The pause between two of those asks.
const TAKE_BACK_PAUSE: Duration = Duration::from_millis(50);

/// The lock a server holds on its database while it runs. Dropping it
/// closes its connection, and Postgres lets the lock go.
#[derive(Debug)]
pub struct ServerLock {
    connection: PgConnection,
    /// The backend of the lock's session, as Postgres names it.
    pid: i32,
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
        let (held, pid): (bool, i32) =
            sqlx::query_as("SELECT pg_try_advisory_lock($1), pg_backend_pid()")
                .bind(SERVER_LOCK)
                .fetch_one(&mut connection)
                .await?;
        if held {
            Ok(Some(Self { connection, pid }))
        } else {
            connection.close().await?;
            Ok(None)
        }
    }

    /// Takes the lock again on a connection of its own, after the session
    /// that held it, `ours`, stopped answering this server (a look into a
    /// cut network that timed out). A session of ours that still holds the
    /// lock is ended first: its connection waits behind what the network
    /// held up and answers nothing in time, though the database is
    /// reachable again (QA round four lock-04). A session is ours when it
    /// carries the pool's own `application_name` (`kage <id>`, of this
    /// process alone), whatever its backend: a session an earlier take of
    /// this process left behind holds the lock under another number. It is
    /// ended by its backend and its name together, so that a backend number
    /// that Postgres gave to another session since is never ended. Only a
    /// pool whose sessions carry no name of a server's (a test's) goes by
    /// the backend `ours`. A holder whose name cannot be read (it is
    /// leaving) is looked at again. `None` when another session holds the
    /// lock: another server took it.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn take_back(pool: &PgPool, ours: i32) -> Result<Option<Self>, StoreError> {
        let options = pool.connect_options();
        let name = options
            .get_application_name()
            .filter(|name| name.starts_with(APPLICATION_NAME_PREFIX))
            .map(str::to_owned);
        let mut connection = PgConnection::connect_with(&options).await?;
        connection.execute(KEEPALIVES).await?;
        let mut ended = None;
        for _ in 0..TAKE_BACK_TRIES {
            let holder: Option<(i32, Option<String>)> = sqlx::query_as(HOLDER)
                .bind(SERVER_LOCK)
                .fetch_optional(&mut connection)
                .await?;
            // Whether the holder is this process's own session; `None`
            // while that cannot be told (its name is gone as it leaves).
            let mine = |(pid, holder_name): &(i32, Option<String>)| match (&name, holder_name) {
                (Some(name), Some(holder_name)) => Some(holder_name == name),
                (Some(_), None) => None,
                (None, _) => Some(*pid == ours),
            };
            match holder.as_ref().map(|holder| (holder.0, mine(holder))) {
                Some((_, Some(false))) => {
                    connection.close().await?;
                    return Ok(None);
                }
                Some((pid, Some(true))) if ended != Some(pid) => {
                    // This server's own session: ended by its backend and
                    // its name, in one statement.
                    let _: Option<bool> = sqlx::query_scalar(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                         WHERE pid = $1 AND ($2::text IS NULL OR application_name = $2)",
                    )
                    .bind(pid)
                    .bind(name.as_deref())
                    .fetch_optional(&mut connection)
                    .await?;
                    ended = Some(pid);
                }
                Some(_) => {}
                None => {
                    let (held, pid): (bool, i32) =
                        sqlx::query_as("SELECT pg_try_advisory_lock($1), pg_backend_pid()")
                            .bind(SERVER_LOCK)
                            .fetch_one(&mut connection)
                            .await?;
                    if held {
                        return Ok(Some(Self { connection, pid }));
                    }
                    // Another server took it in between.
                    connection.close().await?;
                    return Ok(None);
                }
            }
            tokio::time::sleep(TAKE_BACK_PAUSE).await;
        }
        // Ours still, and not gone yet: the next look tries again.
        connection.close().await?;
        Err(StoreError::Unreachable(TAKE_BACK_PAUSE * TAKE_BACK_TRIES))
    }

    /// The backend of the lock's session, as Postgres names it
    /// (`pg_backend_pid()`).
    pub const fn pid(&self) -> i32 {
        self.pid
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
