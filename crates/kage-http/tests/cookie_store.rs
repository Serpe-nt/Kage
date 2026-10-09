//! The cookie jar on its real store (spec §6.4, §9): Postgres through
//! `kage-store`. Cookies survive a restart, belong to exactly one proxy, are
//! refused for a retired proxy, and the session ends after long idleness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{SubsecRound, Utc};
use common::{TestProxy, spec};
use kage_core::config::DatabaseConfig;
use kage_core::domain::{
    ClaimedOs, CookieSameSite, HttpCookie, Proxy, ProxyProviderKind, ProxyScheme, ProxyState,
};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::secret::{SealedSecret, Secret};
use kage_core::ssrf::SsrfPolicy;
use kage_http::cookies::{CookieBackend, CookieBackendError};
use kage_http::{ClientFactory, FetchSpec, HttpError, HttpSettings, StoreCookies};
use kage_store::Store;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The Postgres release the compose file pins (docs/versions.md).
const POSTGRES_TAG: &str = "17.11-trixie";

struct TestDb {
    store: Store,
    _container: ContainerAsync<Postgres>,
}

impl TestDb {
    async fn migrated() -> Self {
        let container = Postgres::default()
            .with_tag(POSTGRES_TAG)
            .start()
            .await
            .expect("cannot start the Postgres test container; is Docker running?");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let store = Store::connect(&DatabaseConfig {
            url: Secret::new(format!(
                "postgres://postgres:postgres@{host}:{port}/postgres"
            )),
            max_connections: 4,
            connect_timeout: HumanDuration::from_secs(20),
        })
        .await
        .unwrap();
        store.migrate().await.unwrap();
        Self {
            store,
            _container: container,
        }
    }

    /// Stores the row of a test proxy, so that cookies can refer to it.
    async fn adopt(&self, id: ProxyId, state: ProxyState) {
        let proxy = Proxy {
            id,
            provider: ProxyProviderKind::Static,
            provider_ref: id.to_string(),
            host: "127.0.0.1".to_owned(),
            port: 8080,
            username: SealedSecret::new(vec![1]),
            password: SealedSecret::new(vec![2]),
            scheme: ProxyScheme::Http,
            country: None,
            city: None,
            asn: None,
            state,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: None,
            created_at: Utc::now().trunc_subsecs(6),
            retired_at: (state == ProxyState::Retired).then(|| Utc::now().trunc_subsecs(6)),
        };
        self.store.proxies().insert(&proxy).await.unwrap();
    }
}

fn get(text: &str) -> FetchSpec {
    FetchSpec::get(Url::parse(text).unwrap(), SsrfPolicy::default())
}

async fn site(proxy: &TestProxy) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("set-cookie", "sid=abc; Path=/; HttpOnly; SameSite=Lax")
                .append_header("set-cookie", "theme=dark; Path=/; Max-Age=86400"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/account"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    proxy.route("site.kage.test:80", *server.address());
    server
}

async fn last_cookie_header(server: &MockServer) -> Option<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .last()
        .and_then(|request| request.headers.get("cookie"))
        .map(|value| value.to_str().unwrap().to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn cookies_are_stored_in_postgres_and_found_again_after_a_restart() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let server = site(&proxy).await;
    db.adopt(proxy.id, ProxyState::Healthy).await;
    let chrome = spec("chrome_149", ClaimedOs::Windows);

    let before = ClientFactory::new(
        HttpSettings::default(),
        Arc::new(StoreCookies::new(db.store.clone())),
    );
    let client = before.client(&chrome, &proxy.endpoint()).await.unwrap();
    client
        .fetch(&get("http://site.kage.test/login"))
        .await
        .unwrap();

    // One row per cookie, with every attribute.
    let stored = db.store.http_cookies().list(proxy.id).await.unwrap();
    assert_eq!(stored.len(), 2);
    let sid = &stored[0];
    assert_eq!((sid.name.as_str(), sid.value.as_str()), ("sid", "abc"));
    assert_eq!(sid.domain, "site.kage.test");
    assert!(sid.host_only && sid.http_only && !sid.secure);
    assert_eq!(sid.same_site.map(CookieSameSite::as_str), Some("lax"));
    assert_eq!(sid.expires_at, None);
    assert_eq!(stored[1].name, "theme");
    assert!(stored[1].expires_at.is_some());

    // A new process: another factory on the same database.
    drop(client);
    drop(before);
    let after = ClientFactory::new(
        HttpSettings::default(),
        Arc::new(StoreCookies::new(db.store.clone())),
    );
    let revived = after.client(&chrome, &proxy.endpoint()).await.unwrap();
    revived
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(
        last_cookie_header(&server).await.as_deref(),
        Some("sid=abc; theme=dark")
    );

    // Another proxy has a jar of its own.
    let stranger = TestProxy::start().await;
    stranger.route("site.kage.test:80", *server.address());
    db.adopt(stranger.id, ProxyState::Healthy).await;
    let other = after.client(&chrome, &stranger.endpoint()).await.unwrap();
    other
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(last_cookie_header(&server).await, None);
    assert_eq!(db.store.http_cookies().count(stranger.id).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jar_that_sat_idle_for_long_has_lost_its_session_cookies() {
    let db = TestDb::migrated().await;
    let proxy = TestProxy::start().await;
    let server = site(&proxy).await;
    db.adopt(proxy.id, ProxyState::Degraded).await;
    let chrome = spec("chrome_149", ClaimedOs::Windows);
    let backend = Arc::new(StoreCookies::new(db.store.clone()));
    let factory = |idle: Duration| {
        ClientFactory::new(
            HttpSettings {
                session_cookie_idle: idle,
                ..HttpSettings::default()
            },
            backend.clone(),
        )
    };

    let first = factory(Duration::from_hours(12));
    let client = first.client(&chrome, &proxy.endpoint()).await.unwrap();
    client
        .fetch(&get("http://site.kage.test/login"))
        .await
        .unwrap();
    assert_eq!(db.store.http_cookies().count(proxy.id).await.unwrap(), 2);

    // The browser stayed closed longer than the session allows: on the
    // next start only the cookie with an expiry of its own is left.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let later = factory(Duration::from_millis(10));
    let revived = later.client(&chrome, &proxy.endpoint()).await.unwrap();
    assert_eq!(revived.jar().len(), 1);
    revived
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(
        last_cookie_header(&server).await.as_deref(),
        Some("theme=dark")
    );
    // The store follows.
    let stored: Vec<String> = db
        .store
        .http_cookies()
        .list(proxy.id)
        .await
        .unwrap()
        .into_iter()
        .map(|cookie| cookie.name)
        .collect();
    assert_eq!(stored, ["theme"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_jar_of_a_retired_or_unknown_proxy_is_never_loaded_or_saved() {
    let db = TestDb::migrated().await;
    let retired = TestProxy::start().await;
    let unknown = TestProxy::start().await;
    db.adopt(retired.id, ProxyState::Retired).await;
    // Cookies left over from the proxy's life.
    let now = Utc::now().trunc_subsecs(6);
    db.store
        .http_cookies()
        .replace(
            retired.id,
            &[HttpCookie {
                domain: "site.kage.test".to_owned(),
                host_only: true,
                path: "/".to_owned(),
                name: "sid".to_owned(),
                value: "old".to_owned(),
                secure: false,
                http_only: false,
                same_site: None,
                expires_at: None,
                created_at: now,
                last_used_at: now,
            }],
        )
        .await
        .unwrap();
    let backend = StoreCookies::new(db.store.clone());
    let factory = ClientFactory::new(HttpSettings::default(), Arc::new(backend.clone()));
    let chrome = spec("chrome_149", ClaimedOs::Windows);

    // A new address must never present the old cookies.
    let error = factory
        .client(&chrome, &retired.endpoint())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, HttpError::Cookies(CookieBackendError::ProxyRetired(id)) if *id == retired.id),
        "{error:?}"
    );
    assert!(error.to_string().contains("retired"));
    assert!(matches!(
        backend.save(retired.id, &[]).await,
        Err(CookieBackendError::ProxyRetired(_))
    ));
    assert_eq!(db.store.http_cookies().count(retired.id).await.unwrap(), 1);

    let error = factory
        .client(&chrome, &unknown.endpoint())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        HttpError::Cookies(CookieBackendError::UnknownProxy(id)) if id == unknown.id
    ));

    // A probe needs no jar and works for any proxy.
    assert!(factory.ephemeral(&chrome, &retired.endpoint()).is_ok());

    // Saving fails quietly for the fetch: a proxy retired mid-flight.
    let live = TestProxy::start().await;
    let server = site(&live).await;
    db.adopt(live.id, ProxyState::Healthy).await;
    let client = factory.client(&chrome, &live.endpoint()).await.unwrap();
    db.store
        .proxies()
        .modify(live.id, |proxy| proxy.state = ProxyState::Retired)
        .await
        .unwrap();
    let fetched = client
        .fetch(&get("http://site.kage.test/login"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(db.store.http_cookies().count(live.id).await.unwrap(), 0);
    drop(server);
}
