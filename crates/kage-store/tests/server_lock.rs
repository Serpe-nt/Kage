//! One server per database (ADR 0005, F174): the advisory lock a server
//! holds for its life, and what happens to it when the server goes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDb;
use kage_store::{SERVER_LOCK, ServerLock};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn one_server_holds_the_database_and_the_next_takes_it_when_it_is_gone() {
    let db = TestDb::empty().await;
    // Two servers on one database: two pools of their own.
    let other = PgPoolOptions::new()
        .max_connections(2)
        .connect(&db.url)
        .await
        .unwrap();

    let mut first = ServerLock::take(db.store.pool())
        .await
        .unwrap()
        .expect("the first server takes the lock");
    assert!(first.held().await.unwrap());
    // The second is refused, as often as it asks.
    for _ in 0..2 {
        assert!(ServerLock::take(&other).await.unwrap().is_none());
    }
    // The lock is held on a connection of its own: the server's pool is
    // free to do its work, and is not the lock's.
    let in_pool: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SERVER_LOCK)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert!(!in_pool);

    // A server that stops lets it go: the next takes it at once.
    first.release().await.unwrap();
    let second = ServerLock::take(&other)
        .await
        .unwrap()
        .expect("taken once the first let it go");
    assert!(ServerLock::take(db.store.pool()).await.unwrap().is_none());

    // A server that is killed has its connection closed by its system:
    // Postgres ends the session, and the lock with it. Here the session is
    // ended from outside, as Postgres ends it for a peer that vanished.
    let mut second = second;
    let ended: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks \
         WHERE locktype = 'advisory' AND objsubid = 1 \
         AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(SERVER_LOCK)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert!(ended);
    // The server that held it finds out at its next look.
    assert!(second.held().await.is_err());
    let mut again = None;
    for _ in 0..50 {
        again = ServerLock::take(db.store.pool()).await.unwrap();
        if again.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut again = again.expect("taken once the session that held it ended");
    assert!(again.held().await.unwrap());
}
