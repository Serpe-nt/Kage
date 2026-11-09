//! A server that may no longer hold its database's lock (ADR 0005): its
//! store is fenced and writes nothing more, its sessions are ended on its
//! way out, and a session of its own that stopped answering is taken back
//! from (QA round four lock-01, lock-02, lock-04).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{TestDb, proxy};
use kage_core::config::DatabaseConfig;
use kage_core::duration::HumanDuration;
use kage_core::secret::Secret;
use kage_store::{APPLICATION_NAME_PREFIX, SERVER_LOCK, ServerLock, Store};
use sqlx::postgres::PgPoolOptions;

/// A server's store over `url`, its sessions named as `kage serve` names
/// them.
async fn server_store(url: &str, connections: u32) -> Store {
    Store::connect(&DatabaseConfig {
        url: Secret::new(url),
        max_connections: connections,
        connect_timeout: HumanDuration::from_secs(10),
    })
    .await
    .unwrap()
}

/// How many proxies the database holds, asked over a pool of the test's.
async fn proxies(url: &str) -> i64 {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let count = sqlx::query_scalar("SELECT count(*) FROM proxies")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}

/// The session that holds the server lock, asked over a pool of the
/// test's.
async fn holder(url: &str) -> Option<i32> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let pid = sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
         AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(SERVER_LOCK)
    .fetch_optional(&pool)
    .await
    .unwrap();
    pool.close().await;
    pid
}

/// QA round four lock-01: once a server decided that it may no longer hold
/// its lock, nothing it writes reaches the database. A write asked for
/// after the fence, and one that waited for a connection when the fence
/// came, are refused at once, whatever the network does afterwards.
#[tokio::test]
async fn a_fenced_store_writes_nothing_and_refuses_at_once() {
    let db = TestDb::migrated().await;
    let store = server_store(&db.url, 1).await;
    assert!(!store.fenced());
    store.proxies().insert(&proxy("before")).await.unwrap();

    // The pool's one connection is busy: the next write waits for it.
    let mut busy = store.pool().acquire().await.unwrap();
    let waiting = {
        let store = store.clone();
        tokio::spawn(async move { store.proxies().insert(&proxy("waited")).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting.is_finished());

    let fenced_at = Instant::now();
    store.fence();
    assert!(store.fenced());
    // Every clone is fenced with it.
    assert!(store.clone().fenced());
    assert!(waiting.await.unwrap().is_err());
    assert!(store.proxies().insert(&proxy("after")).await.is_err());
    assert!(
        fenced_at.elapsed() < Duration::from_secs(1),
        "{:?}",
        fenced_at.elapsed()
    );
    // The connection in use is not used again once it is given back.
    sqlx::query("SELECT 1").execute(&mut *busy).await.unwrap();
    drop(busy);
    assert!(store.proxies().insert(&proxy("again")).await.is_err());
    assert_eq!(proxies(&db.url).await, 1);
    // Fencing twice changes nothing.
    store.fence();
    assert!(store.fenced());
}

/// QA round four lock-02: a server that stopped for its lock ends its own
/// sessions on its way out, the lock's among them, and a statement of its
/// still under way: the next server takes the lock at once, instead of
/// being told for up to a minute that another server runs.
#[tokio::test]
async fn a_server_ends_its_own_sessions_and_the_next_takes_the_lock_at_once() {
    let db = TestDb::migrated().await;
    let store = server_store(&db.url, 2).await;
    let name = store.application_name().unwrap();
    assert!(name.starts_with(APPLICATION_NAME_PREFIX), "{name}");
    // Two servers' stores are told apart.
    let other = server_store(&db.url, 1).await;
    assert_ne!(other.application_name().unwrap(), name);
    other.proxies().insert(&proxy("other")).await.unwrap();
    let theirs = sessions_of(&db.url, &other.application_name().unwrap()).await;
    let first = *theirs.first().expect("the other server has a session");

    let lock = ServerLock::take(store.pool()).await.unwrap().unwrap();
    assert_eq!(holder(&db.url).await, Some(lock.pid()));
    // A statement that runs on, as one a cut network held up would.
    let under_way = {
        let store = store.clone();
        tokio::spawn(async move {
            sqlx::query("SELECT pg_sleep(60)")
                .execute(store.pool())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    store.fence();
    // The process cannot close the lock's connection: it is left as the
    // network left it.
    std::mem::forget(lock);

    let ended = store
        .end_own_sessions(Duration::from_secs(3))
        .await
        .unwrap();
    assert!(ended >= 2, "{ended}");
    assert!(under_way.await.unwrap().is_err());
    let mut next = None;
    for _ in 0..20 {
        next = ServerLock::take(other.pool()).await.unwrap();
        if next.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let next = next.expect("the next server takes the lock at once");
    assert_eq!(holder(&db.url).await, Some(next.pid()));
    // Another server's sessions are not this one's to end.
    let after = sessions_of(&db.url, &other.application_name().unwrap()).await;
    assert!(after.contains(&first), "{first} {after:?}");
    for pid in &theirs {
        assert!(after.contains(pid), "{theirs:?} {after:?}");
    }
}

/// The backends of the sessions named `name`.
async fn sessions_of(url: &str, name: &str) -> Vec<i32> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let pids = sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE application_name = $1")
        .bind(name)
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    pids
}

/// A store whose sessions carry no name of its own does not end anyone's.
#[tokio::test]
async fn a_store_without_a_name_of_its_own_ends_no_session() {
    let db = TestDb::empty().await;
    assert_eq!(db.store.application_name(), None);
    assert!(
        db.store
            .end_own_sessions(Duration::from_secs(3))
            .await
            .is_err()
    );
}

/// QA round four lock-04: the session that holds a server's lock stopped
/// answering it (a look sent into a cut network, which the network then
/// held up), though the database answers again. The server takes the lock
/// back on a new connection, ending its own old session; before, a new
/// connection found the lock held by that very session and took it for
/// another server's.
#[tokio::test]
async fn a_lock_held_by_the_servers_own_silent_session_is_taken_back() {
    let db = TestDb::migrated().await;
    let store = server_store(&db.url, 2).await;
    let mut old = ServerLock::take(store.pool()).await.unwrap().unwrap();
    let stale = old.pid();
    // A new connection alone finds the lock held.
    assert!(ServerLock::take(store.pool()).await.unwrap().is_none());

    let mut back = ServerLock::take_back(store.pool(), stale)
        .await
        .unwrap()
        .expect("its own session's lock is taken back");
    assert_ne!(back.pid(), stale);
    assert!(back.held().await.unwrap());
    assert_eq!(holder(&db.url).await, Some(back.pid()));
    // The old session is gone.
    assert!(old.held().await.is_err());

    // Taking back a lock that nobody holds takes it.
    back.release().await.unwrap();
    let free = ServerLock::take_back(store.pool(), stale)
        .await
        .unwrap()
        .expect("a free lock is taken");
    assert_eq!(holder(&db.url).await, Some(free.pid()));
}

/// Another server's lock is never taken back: the server that asks finds
/// it taken, and the other's session lives on.
#[tokio::test]
async fn another_servers_lock_is_left_alone() {
    let db = TestDb::migrated().await;
    let ours = server_store(&db.url, 2).await;
    let theirs = server_store(&db.url, 2).await;
    let mine = ServerLock::take(ours.pool()).await.unwrap().unwrap();
    let stale = mine.pid();
    mine.release().await.unwrap();
    let mut other = ServerLock::take(theirs.pool()).await.unwrap().unwrap();

    assert!(
        ServerLock::take_back(ours.pool(), stale)
            .await
            .unwrap()
            .is_none()
    );
    // Nor when its session has the very backend number this server's had
    // (Postgres gives a number again once its session is gone): the
    // session is not of this process's.
    assert!(
        ServerLock::take_back(ours.pool(), other.pid())
            .await
            .unwrap()
            .is_none()
    );
    assert!(other.held().await.unwrap());
    assert_eq!(holder(&db.url).await, Some(other.pid()));
}
