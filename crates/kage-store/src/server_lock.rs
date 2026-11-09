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

/// The key of the advisory lock a server holds on its database: the bytes
/// of `kageserv`. Nothing else in Kage takes an advisory lock.
pub const SERVER_LOCK: i64 = 0x6b61_6765_7365_7276;

/// The TCP keepalives of the lock's session: after 30 s of silence
/// Postgres asks whether the server is still there, every 10 s, and gives
/// the connection up, and the lock with it, after 3 questions without an
/// answer.
pub const KEEPALIVES: &str = "SET tcp_keepalives_idle = 30; \
     SET tcp_keepalives_interval = 10; SET tcp_keepalives_count = 3";

/// The session that holds [`SERVER_LOCK`], when one does, and its
/// `application_name`.
const HOLDER: &str = "SELECT l.pid, a.application_name FROM pg_locks l \
     LEFT JOIN pg_stat_activity a USING (pid) WHERE l.locktype = 'advisory' AND l.granted \
     AND l.objsubid = 1 AND ((l.classid::bigint << 32) | l.objid::bigint) = $1";

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
    /// has the backend `ours` and the pool's own `application_name`
    /// (`kage <id>`, of this process alone), so that a backend number that
    /// Postgres gave to another server's session since is never ended.
    /// `None` when another session holds the lock: another server took it.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn take_back(pool: &PgPool, ours: i32) -> Result<Option<Self>, StoreError> {
        let options = pool.connect_options();
        let name = options.get_application_name().map(str::to_owned);
        let mut connection = PgConnection::connect_with(&options).await?;
        connection.execute(KEEPALIVES).await?;
        let mut ended = false;
        for _ in 0..TAKE_BACK_TRIES {
            let holder: Option<(i32, Option<String>)> = sqlx::query_as(HOLDER)
                .bind(SERVER_LOCK)
                .fetch_optional(&mut connection)
                .await?;
            let theirs = |(pid, holder_name): &(i32, Option<String>)| {
                *pid != ours || (name.is_some() && *holder_name != name)
            };
            match holder {
                Some(holder) if theirs(&holder) => {
                    connection.close().await?;
                    return Ok(None);
                }
                Some(_) if !ended => {
                    // Ended by its pid: it is this server's own session.
                    ended = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                        .bind(ours)
                        .fetch_one(&mut connection)
                        .await?;
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
