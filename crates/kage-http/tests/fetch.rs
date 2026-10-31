//! The http path end to end (spec §9), through a local proxy: to wiremock
//! over plain HTTP, and to the fingerprint echo over TLS.
//!
//! Covered here: every request leaves through the proxy with its
//! credentials; the header order and spelling on the wire; the fetch
//! metadata of a typed navigation and of a followed link; cookies across
//! requests and clients; redirects, hop by hop, with the SSRF guard on each;
//! the body cap; the per-phase timing; and how failures are told apart.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use common::{ScriptedResolver, TestEcho, TestProxy, factory, observation, spec};
use http::Method;
use kage_core::domain::{ClaimedOs, CookieSameSite, HttpCookie};
use kage_core::ssrf::{DeniedRange, SsrfDenial, SsrfPolicy};
use kage_http::headers::HeaderError;
use kage_http::{
    ClientFactory, FetchSpec, Guard, HttpClient, HttpError, HttpSettings, MemoryCookies, Stage,
};
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const STRICT: SsrfPolicy = SsrfPolicy {
    allow_ip_targets: false,
};

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn get(text: &str) -> FetchSpec {
    FetchSpec::get(url(text), STRICT)
}

/// A proxy with two plain sites behind it, `site.kage.test` (and its
/// subdomain `api.site.kage.test`) and `other.kage.test`, and the echo.
struct World {
    proxy: TestProxy,
    site: MockServer,
    other: MockServer,
    echo: TestEcho,
}

impl World {
    async fn start() -> Self {
        let proxy = TestProxy::start().await;
        let site = MockServer::start().await;
        let other = MockServer::start().await;
        let echo = TestEcho::start().await;
        proxy.route("site.kage.test:80", *site.address());
        proxy.route("api.site.kage.test:80", *site.address());
        proxy.route("other.kage.test:80", *other.address());
        echo.route_through(&proxy);
        Self {
            proxy,
            site,
            other,
            echo,
        }
    }

    fn factory(&self) -> ClientFactory {
        factory(&self.echo)
    }

    fn chrome(&self) -> HttpClient {
        self.factory()
            .ephemeral(
                &spec("chrome_149", ClaimedOs::Windows),
                &self.proxy.endpoint(),
            )
            .unwrap()
    }

    fn firefox(&self) -> HttpClient {
        self.factory()
            .ephemeral(
                &spec("firefox_151", ClaimedOs::Windows),
                &self.proxy.endpoint(),
            )
            .unwrap()
    }
}

async fn page(server: &MockServer, at: &str, response: ResponseTemplate) {
    Mock::given(path(at))
        .respond_with(response)
        .mount(server)
        .await;
}

fn redirect(status: u16, location: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).insert_header("location", location)
}

async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

fn header(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_leaves_through_the_proxy_and_returns_status_headers_body_and_timing() {
    let world = World::start().await;
    page(
        &world.site,
        "/page",
        ResponseTemplate::new(200)
            .insert_header("x-served-by", "wiremock")
            .set_body_raw("<html>hello</html>", "text/html; charset=utf-8"),
    )
    .await;

    let fetched = world
        .chrome()
        .fetch(&get("http://site.kage.test/page?q=1"))
        .await
        .unwrap();

    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.final_url.as_str(), "http://site.kage.test/page?q=1");
    assert_eq!(fetched.http_version, "http/1.1");
    assert_eq!(fetched.text(), "<html>hello</html>");
    assert!(!fetched.truncated && !fetched.downgraded);
    assert_eq!(fetched.header("X-Served-By"), Some("wiremock"));
    assert_eq!(
        fetched.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(fetched.header("absent"), None);
    assert_eq!(fetched.hops.len(), 1);
    assert_eq!(fetched.redirects(), []);
    assert_eq!(fetched.hops[0].status, 200);
    assert_eq!(fetched.hops[0].location, None);
    assert_eq!(fetched.subresources, None);

    // The proxy saw the request in absolute form, with the login.
    let seen = world.proxy.plain();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].target, "http://site.kage.test/page?q=1");
    assert!(
        seen[0]
            .header("Proxy-Authorization")
            .unwrap()
            .starts_with("Basic ")
    );
    assert_eq!(world.proxy.refused(), 0);
    // And the target got it from the proxy.
    let at_target = received(&world.site).await;
    assert_eq!(at_target.len(), 1);
    assert_eq!(at_target[0].url.path(), "/page");
    assert_eq!(
        header(&at_target[0], "host").as_deref(),
        Some("site.kage.test")
    );

    // Timing: a connection was opened, without TLS; no DNS phase.
    let timing = fetched.timing;
    assert_eq!(timing.dns_ms, None);
    assert!(timing.connect_ms.is_some());
    assert_eq!(timing.tls_ms, None);
    assert!(timing.ttfb_ms.unwrap() <= timing.total_ms);
    assert_eq!(timing.load_ms, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn chromes_headers_on_the_wire_over_plain_http() {
    let world = World::start().await;
    page(&world.site, "/list", ResponseTemplate::new(200)).await;
    let client = world.chrome();
    let now = Utc::now();
    for name in ["first", "second"] {
        client.jar().import(HttpCookie {
            domain: "site.kage.test".to_owned(),
            host_only: true,
            path: "/".to_owned(),
            name: name.to_owned(),
            value: "1".to_owned(),
            secure: false,
            http_only: false,
            same_site: None,
            expires_at: None,
            created_at: now,
            last_used_at: now,
        });
    }

    client
        .fetch(&get("http://site.kage.test/list").header("Referer", "http://site.kage.test/"))
        .await
        .unwrap();

    let seen = &world.proxy.plain()[0];
    assert_eq!(
        seen.head.lines().next(),
        Some("GET http://site.kage.test/list HTTP/1.1")
    );
    // A plain origin is not trustworthy: no client hints, no fetch
    // metadata, no brotli; and no Priority header on HTTP/1.1.
    assert_eq!(
        seen.header_names(),
        [
            "Host",
            "Proxy-Connection",
            "Proxy-Authorization",
            "Upgrade-Insecure-Requests",
            "User-Agent",
            "Accept",
            "Referer",
            "Accept-Encoding",
            "Accept-Language",
            "Cookie",
        ]
    );
    assert_eq!(seen.header("Host").as_deref(), Some("site.kage.test"));
    assert_eq!(
        seen.header("Proxy-Connection").as_deref(),
        Some("keep-alive")
    );
    assert_eq!(
        seen.header("Upgrade-Insecure-Requests").as_deref(),
        Some("1")
    );
    assert_eq!(
        seen.header("Accept-Encoding").as_deref(),
        Some("gzip, deflate")
    );
    assert_eq!(
        seen.header("Accept-Language").as_deref(),
        Some("en-US,en;q=0.9")
    );
    assert_eq!(
        seen.header("Referer").as_deref(),
        Some("http://site.kage.test/")
    );
    assert_eq!(seen.header("Cookie").as_deref(), Some("first=1; second=1"));
    assert!(
        seen.header("User-Agent")
            .unwrap()
            .contains("Chrome/149.0.0.0")
    );
    assert!(
        seen.header("Accept")
            .unwrap()
            .starts_with("text/html,application/xhtml+xml")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn firefoxs_headers_on_the_wire_over_plain_http() {
    let world = World::start().await;
    page(&world.site, "/list", ResponseTemplate::new(200)).await;
    let factory = world.factory();
    let client = factory
        .ephemeral(
            &spec("firefox_151", ClaimedOs::Windows).with_languages(["de", "en-US", "en"]),
            &world.proxy.endpoint(),
        )
        .unwrap();

    client
        .fetch(&get("http://site.kage.test/list").header("Referer", "http://site.kage.test/"))
        .await
        .unwrap();

    let seen = &world.proxy.plain()[0];
    assert_eq!(
        seen.header_names(),
        [
            "Host",
            "User-Agent",
            "Accept",
            "Accept-Language",
            "Accept-Encoding",
            "Connection",
            "Proxy-Authorization",
            "Referer",
            "Upgrade-Insecure-Requests",
            "Priority",
        ]
    );
    assert_eq!(
        seen.header("Accept-Language").as_deref(),
        Some("de,en-US;q=0.7,en;q=0.3")
    );
    assert_eq!(
        seen.header("Accept-Encoding").as_deref(),
        Some("gzip, deflate")
    );
    assert_eq!(seen.header("Connection").as_deref(), Some("keep-alive"));
    assert!(
        seen.header("User-Agent")
            .unwrap()
            .ends_with("Firefox/151.0")
    );
}

/// The fetch metadata for both kinds of request the spec names: a typed
/// navigation, and a request that follows a link (it carries a Referer).
#[tokio::test(flavor = "multi_thread")]
async fn fetch_metadata_tells_a_typed_navigation_from_a_followed_link() {
    let world = World::start().await;
    let client = world.chrome();
    let ask = |referer: Option<&'static str>| {
        let client = &client;
        async move {
            let mut spec = FetchSpec::operator_get(TestEcho::url("/item/9"));
            if let Some(referer) = referer {
                spec = spec.header("Referer", referer);
            }
            observation(&client.fetch(&spec).await.unwrap().body)
        }
    };

    let typed = ask(None).await;
    assert_eq!(typed.http_version, "h2");
    assert_eq!(typed.header("sec-fetch-site"), Some("none"));
    assert_eq!(typed.header("sec-fetch-mode"), Some("navigate"));
    assert_eq!(typed.header("sec-fetch-user"), Some("?1"));
    assert_eq!(typed.header("sec-fetch-dest"), Some("document"));
    assert_eq!(typed.header("referer"), None);

    for (referer, site, sent) in [
        // The same origin sends its whole URL, without the fragment.
        (
            "https://echo.kage.test/list?page=2#row-7",
            "same-origin",
            "https://echo.kage.test/list?page=2",
        ),
        // Another host of the same site, and another site, send their origin.
        (
            "https://www.kage.test/a?q=1",
            "same-site",
            "https://www.kage.test/",
        ),
        (
            "https://search.example/results?q=kage",
            "cross-site",
            "https://search.example/",
        ),
    ] {
        let followed = ask(Some(referer)).await;
        assert_eq!(followed.header("sec-fetch-site"), Some(site), "{referer}");
        // A followed link is still a navigation the user caused.
        assert_eq!(followed.header("sec-fetch-mode"), Some("navigate"));
        assert_eq!(followed.header("sec-fetch-user"), Some("?1"));
        assert_eq!(followed.header("sec-fetch-dest"), Some("document"));
        assert_eq!(followed.header("referer"), Some(sent), "{referer}");
        // Chrome sends the Referer right after the fetch metadata.
        let names = followed.header_names();
        let position = |name: &str| names.iter().position(|found| *found == name).unwrap();
        assert_eq!(position("referer"), position("sec-fetch-dest") + 1);
        assert_eq!(position("accept-encoding"), position("referer") + 1);
    }
    // A page on plain http is another site than the https target, and a
    // browser sends its origin onward.
    let upgraded = ask(Some("http://echo.kage.test/list")).await;
    assert_eq!(upgraded.header("sec-fetch-site"), Some("cross-site"));
    assert_eq!(upgraded.header("referer"), Some("http://echo.kage.test/"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cookies_and_referer_sit_where_each_browser_puts_them_on_http2() {
    let world = World::start().await;
    let now = Utc::now();
    let cookie = |name: &str| HttpCookie {
        domain: "kage.test".to_owned(),
        host_only: false,
        path: "/".to_owned(),
        name: name.to_owned(),
        value: "1".to_owned(),
        secure: true,
        http_only: true,
        same_site: Some(CookieSameSite::Lax),
        expires_at: None,
        created_at: now,
        last_used_at: now,
    };
    let spec = FetchSpec::operator_get(TestEcho::url("/"))
        .header("Referer", "https://echo.kage.test/list")
        .header("X-Requested-With", "kage-test");

    let chrome = world.chrome();
    chrome.jar().import(cookie("a"));
    chrome.jar().import(cookie("b"));
    let seen = observation(&chrome.fetch(&spec).await.unwrap().body);
    assert_eq!(
        seen.header_names(),
        [
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "upgrade-insecure-requests",
            "user-agent",
            "accept",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-user",
            "sec-fetch-dest",
            "referer",
            "accept-encoding",
            "accept-language",
            "x-requested-with",
            "cookie",
            "cookie",
            "priority",
        ]
    );
    // One field per cookie on HTTP/2, as browsers send them.
    let cookies: Vec<&str> = seen
        .headers()
        .iter()
        .filter(|(name, _)| name == "cookie")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(cookies, ["a=1", "b=1"]);
    assert_eq!(
        seen.http2.as_ref().unwrap().pseudo_headers,
        ["m", "a", "s", "p"]
    );

    let firefox = world.firefox();
    firefox.jar().import(cookie("a"));
    let seen = observation(&firefox.fetch(&spec).await.unwrap().body);
    assert_eq!(
        seen.header_names(),
        [
            "user-agent",
            "accept",
            "accept-language",
            "accept-encoding",
            "referer",
            "x-requested-with",
            "cookie",
            "upgrade-insecure-requests",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-fetch-user",
            "priority",
            "te",
        ]
    );
    assert_eq!(
        seen.http2.as_ref().unwrap().pseudo_headers,
        ["m", "p", "a", "s"]
    );
    assert_eq!(seen.header("te"), Some("trailers"));
}

/// A server that only speaks HTTP/1.1, inside a tunnel: the browser's own
/// spelling, a connection header, and none of the HTTP/2-only fields.
#[tokio::test(flavor = "multi_thread")]
async fn http1_inside_a_tunnel_uses_each_browsers_spelling() {
    let world = World::start().await;

    let chrome = world.chrome();
    let fetched = chrome
        .fetch(&FetchSpec::operator_get(TestEcho::h1_url("/")))
        .await
        .unwrap();
    assert_eq!(fetched.http_version, "http/1.1");
    let seen = observation(&fetched.body);
    assert_eq!(seen.http1.as_ref().unwrap().request_line, "GET / HTTP/1.1");
    assert_eq!(
        seen.header_names(),
        [
            "Host",
            "Connection",
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "Upgrade-Insecure-Requests",
            "User-Agent",
            "Accept",
            "Sec-Fetch-Site",
            "Sec-Fetch-Mode",
            "Sec-Fetch-User",
            "Sec-Fetch-Dest",
            "Accept-Encoding",
            "Accept-Language",
        ]
    );
    assert_eq!(seen.header("Host"), Some("h1.echo.kage.test"));
    assert_eq!(seen.header("Connection"), Some("keep-alive"));
    assert_eq!(
        seen.header("Accept-Encoding"),
        Some("gzip, deflate, br, zstd")
    );
    // The handshake is the same one: the client offered HTTP/2 as always.
    assert_eq!(seen.tls.alpn, ["h2", "http/1.1"]);
    assert_eq!(seen.tls.sni.as_deref(), Some("h1.echo.kage.test"));

    let fetched = world
        .firefox()
        .fetch(&FetchSpec::operator_get(TestEcho::h1_url("/")))
        .await
        .unwrap();
    assert_eq!(
        observation(&fetched.body).header_names(),
        [
            "Host",
            "User-Agent",
            "Accept",
            "Accept-Language",
            "Accept-Encoding",
            "Connection",
            "Upgrade-Insecure-Requests",
            "Sec-Fetch-Dest",
            "Sec-Fetch-Mode",
            "Sec-Fetch-Site",
            "Sec-Fetch-User",
            "Priority",
        ]
    );
}

// One jar followed through requests, clients and factories: each step
// needs the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn cookies_persist_across_requests_clients_and_factories() {
    let world = World::start().await;
    page(
        &world.site,
        "/login",
        ResponseTemplate::new(200)
            .append_header("set-cookie", "sid=abc123; Path=/; HttpOnly")
            .append_header("set-cookie", "theme=dark; Path=/; Max-Age=3600")
            .append_header("set-cookie", "elsewhere=1; Domain=other.kage.test"),
    )
    .await;
    page(&world.site, "/account", ResponseTemplate::new(200)).await;
    page(
        &world.site,
        "/probe",
        ResponseTemplate::new(200).append_header("set-cookie", "probe=1; Path=/"),
    )
    .await;
    page(&world.other, "/", ResponseTemplate::new(200)).await;
    let backend = Arc::new(MemoryCookies::new());
    let chrome = spec("chrome_149", ClaimedOs::Windows);
    let cookie_at = |server: &'static str| {
        let world = &world;
        async move {
            let server = if server == "site" {
                &world.site
            } else {
                &world.other
            };
            header(received(server).await.last().unwrap(), "cookie")
        }
    };

    let first = ClientFactory::new(HttpSettings::default(), backend.clone());
    let client = first
        .client(&chrome, &world.proxy.endpoint())
        .await
        .unwrap();
    client
        .fetch(&get("http://site.kage.test/login"))
        .await
        .unwrap();
    // Nothing to send on the first request.
    assert_eq!(cookie_at("site").await, None);

    client
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(
        cookie_at("site").await.as_deref(),
        Some("sid=abc123; theme=dark")
    );
    // A cookie for another site was refused, and nothing leaks there.
    client.fetch(&get("http://other.kage.test/")).await.unwrap();
    assert_eq!(cookie_at("other").await, None);

    // The factory hands out the same client again.
    let again = first
        .client(&chrome, &world.proxy.endpoint())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&client, &again));

    // A restart: a new factory on the same store finds the cookies.
    let second = ClientFactory::new(HttpSettings::default(), backend.clone());
    let revived = second
        .client(&chrome, &world.proxy.endpoint())
        .await
        .unwrap();
    assert_eq!(revived.jar().len(), 2);
    revived
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(
        cookie_at("site").await.as_deref(),
        Some("sid=abc123; theme=dark")
    );

    // New credentials for the same proxy rebuild the client and keep them.
    let rotated = second
        .client(
            &chrome,
            &world.proxy.endpoint_with("other-user", "other-pass"),
        )
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&revived, &rotated));
    assert_eq!(rotated.jar().len(), 2);
    assert!(matches!(
        rotated.fetch(&get("http://site.kage.test/account")).await,
        Err(HttpError::ProxyAuth)
    ));

    // Another proxy is another persona: it has no cookies.
    let stranger = TestProxy::start().await;
    stranger.route("site.kage.test:80", *world.site.address());
    let other_client = second.client(&chrome, &stranger.endpoint()).await.unwrap();
    assert!(other_client.jar().is_empty());
    other_client
        .fetch(&get("http://site.kage.test/account"))
        .await
        .unwrap();
    assert_eq!(cookie_at("site").await, None);

    // A probe's client starts empty and stores nothing.
    let probe = second.ephemeral(&chrome, &world.proxy.endpoint()).unwrap();
    assert!(probe.jar().is_empty());
    probe
        .fetch(&get("http://site.kage.test/probe"))
        .await
        .unwrap();
    assert_eq!(probe.jar().len(), 1);
    second.forget(world.proxy.id).await;
    let fresh = second
        .client(&chrome, &world.proxy.endpoint())
        .await
        .unwrap();
    let kept: Vec<String> = fresh
        .jar()
        .snapshot(Utc::now())
        .into_iter()
        .map(|cookie| cookie.name)
        .collect();
    assert_eq!(
        kept,
        ["sid", "theme"],
        "what the probe was given must not be stored"
    );
    assert_eq!(fresh.proxy(), world.proxy.id);
    assert_eq!(fresh.profile().name(), "chrome_149");
    assert_eq!(fresh.settings(), second.settings());
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_are_followed_hop_by_hop_with_cookies_and_referer_per_hop() {
    let world = World::start().await;
    page(
        &world.site,
        "/start",
        redirect(302, "/next?step=2").append_header("set-cookie", "hop=1; Path=/"),
    )
    .await;
    page(
        &world.site,
        "/next",
        redirect(301, "http://other.kage.test/landing"),
    )
    .await;
    page(
        &world.other,
        "/landing",
        ResponseTemplate::new(200).set_body_string("arrived"),
    )
    .await;

    let fetched = world
        .chrome()
        .fetch(
            &get("http://site.kage.test/start").header("Referer", "http://site.kage.test/home?x=1"),
        )
        .await
        .unwrap();

    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.text(), "arrived");
    assert_eq!(fetched.final_url.as_str(), "http://other.kage.test/landing");
    let hops: Vec<(&str, u16, Option<&str>)> = fetched
        .hops
        .iter()
        .map(|hop| (hop.url.as_str(), hop.status, hop.location.as_deref()))
        .collect();
    assert_eq!(
        hops,
        [
            ("http://site.kage.test/start", 302, Some("/next?step=2")),
            (
                "http://site.kage.test/next?step=2",
                301,
                Some("http://other.kage.test/landing")
            ),
            ("http://other.kage.test/landing", 200, None),
        ]
    );
    assert_eq!(fetched.redirects().len(), 2);

    let at_site = received(&world.site).await;
    // The cookie the first hop set goes to the second hop of the same
    // site, with the page's whole URL as Referer.
    assert_eq!(header(&at_site[0], "cookie"), None);
    assert_eq!(header(&at_site[1], "cookie").as_deref(), Some("hop=1"));
    assert_eq!(
        header(&at_site[1], "referer").as_deref(),
        Some("http://site.kage.test/home?x=1")
    );
    // The other site gets neither the cookie nor more than the origin.
    let at_other = received(&world.other).await;
    assert_eq!(header(&at_other[0], "cookie"), None);
    assert_eq!(
        header(&at_other[0], "referer").as_deref(),
        Some("http://site.kage.test/")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_changes_the_method_the_way_a_browser_does() {
    let world = World::start().await;
    page(&world.site, "/form", redirect(303, "/done")).await;
    page(&world.site, "/moved", redirect(301, "/done")).await;
    page(&world.site, "/api", redirect(307, "/api2")).await;
    page(&world.site, "/done", ResponseTemplate::new(200)).await;
    page(&world.site, "/api2", ResponseTemplate::new(200)).await;
    let client = world.chrome();
    let post = |at: &str| {
        let mut spec = get(&format!("http://site.kage.test{at}"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Referer", "http://site.kage.test/form");
        spec.method = Method::POST;
        spec.body = Some(Bytes::from_static(b"a=1&b=2"));
        spec
    };

    // 303 and 301 turn a POST into a GET without the body.
    for at in ["/form", "/moved"] {
        let before = received(&world.site).await.len();
        client.fetch(&post(at)).await.unwrap();
        let requests = received(&world.site).await;
        let (first, second) = (&requests[before], &requests[before + 1]);
        assert_eq!(first.method.as_str(), "POST");
        assert_eq!(first.body, b"a=1&b=2");
        assert_eq!(header(first, "content-length").as_deref(), Some("7"));
        assert_eq!(
            header(first, "origin").as_deref(),
            Some("http://site.kage.test")
        );
        assert_eq!(header(first, "cache-control").as_deref(), Some("max-age=0"));
        assert_eq!(second.method.as_str(), "GET", "{at}");
        assert_eq!(second.body, &b""[..]);
        assert_eq!(header(second, "content-type"), None);
        assert_eq!(header(second, "content-length"), None);
        assert_eq!(header(second, "origin"), None);
    }

    // 307 repeats the request as it was.
    let before = received(&world.site).await.len();
    client.fetch(&post("/api")).await.unwrap();
    let requests = received(&world.site).await;
    let repeated = &requests[before + 1];
    assert_eq!(repeated.url.path(), "/api2");
    assert_eq!(repeated.method.as_str(), "POST");
    assert_eq!(repeated.body, b"a=1&b=2");
    assert_eq!(
        header(repeated, "content-type").as_deref(),
        Some("application/x-www-form-urlencoded")
    );

    // A HEAD stays a HEAD and has no body to return.
    let mut head = get("http://site.kage.test/form");
    head.method = Method::HEAD;
    let fetched = client.fetch(&head).await.unwrap();
    assert_eq!(fetched.status, 200);
    assert!(fetched.body.is_empty());
    assert_eq!(
        received(&world.site).await.last().unwrap().method.as_str(),
        "HEAD"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_to_a_denied_target_ends_the_fetch_and_is_never_requested() {
    let world = World::start().await;
    page(
        &world.site,
        "/meta",
        redirect(302, "http://169.254.169.254/latest/meta-data/"),
    )
    .await;
    page(
        &world.site,
        "/local",
        redirect(302, "http://localhost:5432/"),
    )
    .await;
    page(
        &world.site,
        "/literal",
        redirect(302, "http://93.184.216.34/"),
    )
    .await;
    page(&world.site, "/file", redirect(302, "file:///etc/passwd")).await;
    page(
        &world.site,
        "/creds",
        redirect(302, "http://user:pw@other.kage.test/"),
    )
    .await;
    let client = world.chrome();

    for (at, expect) in [
        ("/meta", "cloud_metadata"),
        ("/local", "it names this machine"),
        ("/literal", "literal IP address"),
        ("/file", "scheme"),
        ("/creds", "credentials"),
    ] {
        let before = world.proxy.seen().len();
        let error = client
            .fetch(&get(&format!("http://site.kage.test{at}")))
            .await
            .unwrap_err();

        assert!(error.is_ssrf_denied(), "{at}: {error}");
        let HttpError::RedirectDenied(denied) = &error else {
            panic!("{at}: expected a denied redirect, got {error:?}");
        };
        assert_eq!(denied.hop, 1);
        assert!(error.to_string().contains(expect), "{at}: {error}");
        assert!(denied.evidence().starts_with("redirect:1:"));
        // Only the first hop was ever sent.
        assert_eq!(world.proxy.seen().len(), before + 1, "{at}");
    }

    // The same range is refused as the URL itself, before anything is sent.
    let before = world.proxy.seen().len();
    let error = client
        .fetch(&get("http://169.254.169.254/"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        HttpError::Denied(SsrfDenial::Address {
            range: DeniedRange::CloudMetadata,
            ..
        })
    ));
    assert!(error.is_ssrf_denied());
    assert_eq!(world.proxy.seen().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_resolver_every_hop_is_checked_by_address_too() {
    let world = World::start().await;
    page(
        &world.site,
        "/go",
        redirect(302, "http://other.kage.test/in"),
    )
    .await;
    page(&world.site, "/echo", ResponseTemplate::new(200)).await;
    page(&world.other, "/in", ResponseTemplate::new(200)).await;
    let resolver = ScriptedResolver::new();
    let factory = world.factory().with_resolver(resolver.clone());
    let client = factory
        .ephemeral(
            &spec("chrome_149", ClaimedOs::Windows),
            &world.proxy.endpoint(),
        )
        .unwrap();

    // Public addresses on both hops: the fetch goes through, and both
    // names were asked.
    let fetched = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(resolver.asked(), ["site.kage.test", "other.kage.test"]);

    // The redirect's name points into a private network.
    resolver.answer("other.kage.test", &["93.184.216.34", "10.0.0.7"]);
    let before = world.proxy.seen().len();
    let error = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap_err();
    let HttpError::RedirectDenied(denied) = &error else {
        panic!("expected a denied redirect, got {error:?}");
    };
    assert_eq!(denied.hop, 1);
    assert_eq!(denied.location, "http://other.kage.test/in");
    assert_eq!(
        denied.reason,
        SsrfDenial::Address {
            address: "10.0.0.7".parse().unwrap(),
            range: DeniedRange::Private
        }
    );
    assert_eq!(world.proxy.seen().len(), before + 1);

    // A hop whose name has no address, or that the host cannot look up,
    // is refused the same way: only the first hop was asked for.
    for answer in [Ok(Vec::new()), Err("SERVFAIL")] {
        match answer {
            Ok(addresses) => resolver.answer("other.kage.test", &addresses),
            Err(reason) => resolver.fail("other.kage.test", reason),
        }
        let error = client
            .fetch(&get("http://site.kage.test/go"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, HttpError::RedirectDenied(denied) if denied.hop == 1
                && denied.reason == SsrfDenial::Unresolved("other.kage.test".to_owned())),
            "{error:?}"
        );
    }
    assert_eq!(world.proxy.seen().len(), before + 3);

    // The first name itself points at this machine: nothing is sent.
    resolver.answer("site.kage.test", &["127.0.0.1"]);
    let before = world.proxy.seen().len();
    let error = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        HttpError::Denied(SsrfDenial::Address {
            range: DeniedRange::Loopback,
            ..
        })
    ));
    assert_eq!(world.proxy.seen().len(), before);

    // A name that does not resolve, and a resolver that fails: both are a
    // name with no address, which is not fetched.
    resolver.answer("site.kage.test", &[]);
    assert!(matches!(
        client.fetch(&get("http://site.kage.test/go")).await,
        Err(HttpError::Denied(SsrfDenial::Unresolved(_)))
    ));
    resolver.fail("site.kage.test", "SERVFAIL");
    assert!(matches!(
        client.fetch(&get("http://site.kage.test/go")).await,
        Err(HttpError::Denied(SsrfDenial::Unresolved(_)))
    ));
    assert_eq!(world.proxy.seen().len(), before);

    // An operator's endpoint is not a target: the resolver is not asked.
    let asked = resolver.asked().len();
    client
        .fetch(&FetchSpec::operator_get(url("http://site.kage.test/echo")))
        .await
        .unwrap();
    assert_eq!(resolver.asked().len(), asked);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_redirect_limit_cuts_a_loop() {
    let world = World::start().await;
    page(&world.site, "/loop", redirect(302, "/loop")).await;
    let client = world.chrome();

    // The default: ten redirects are followed, the eleventh is refused.
    let error = client
        .fetch(&get("http://site.kage.test/loop"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, HttpError::TooManyRedirects { max: 10, location } if location == "/loop"),
        "{error:?}"
    );
    assert!(!error.is_ssrf_denied());
    assert_eq!(received(&world.site).await.len(), 11);

    let mut short = get("http://site.kage.test/loop");
    short.max_redirects = Some(2);
    assert!(matches!(
        client.fetch(&short).await,
        Err(HttpError::TooManyRedirects { max: 2, .. })
    ));
    assert_eq!(received(&world.site).await.len(), 11 + 3);

    // An operator's endpoint is not followed at all, and a redirect it
    // answers with is refused rather than chased.
    let error = client
        .fetch(&FetchSpec::operator_get(url("http://site.kage.test/loop")))
        .await
        .unwrap_err();
    assert!(matches!(error, HttpError::TooManyRedirects { max: 0, .. }));
    let mut one = FetchSpec::operator_get(url("http://site.kage.test/loop"));
    one.max_redirects = Some(1);
    assert!(matches!(
        client.fetch(&one).await,
        Err(HttpError::TooManyRedirects { max: 1, .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_from_plain_http_to_https_opens_a_second_connection() {
    let world = World::start().await;
    page(&world.site, "/up", redirect(302, "https://echo.kage.test/")).await;
    let client = world.chrome();

    let mut spec = FetchSpec::operator_get(url("http://site.kage.test/up"));
    spec.max_redirects = Some(3);
    let fetched = client.fetch(&spec).await.unwrap();
    assert_eq!(fetched.final_url.as_str(), "https://echo.kage.test/");
    assert_eq!(fetched.http_version, "h2");
    assert!(!fetched.downgraded);
    // Two connections were opened; one of them with a handshake.
    assert!(fetched.timing.connect_ms.is_some() && fetched.timing.tls_ms.is_some());
    assert_eq!(fetched.hops[0].setup.tls, None);
    assert!(fetched.hops[1].setup.tls.is_some());
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_body_is_read_up_to_the_cap_and_no_further() {
    let world = World::start().await;
    page(
        &world.site,
        "/big",
        ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 300_000]),
    )
    .await;
    page(
        &world.site,
        "/exact",
        ResponseTemplate::new(200).set_body_bytes(vec![b'y'; 4096]),
    )
    .await;
    // Ten megabytes of zeros in about ten kilobytes.
    page(
        &world.site,
        "/bomb",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_bytes(gzip(&vec![0_u8; 10_000_000])),
    )
    .await;
    page(
        &world.site,
        "/packed",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_bytes(gzip(b"decoded text")),
    )
    .await;
    let client = world.chrome();
    let capped = |at: &str, cap: usize| {
        let mut spec = get(&format!("http://site.kage.test{at}"));
        spec.max_body_bytes = Some(cap);
        spec
    };

    let big = client.fetch(&capped("/big", 65_536)).await.unwrap();
    assert_eq!(big.status, 200);
    assert!(big.truncated);
    assert_eq!(big.body.len(), 65_536);
    assert!(big.body.iter().all(|byte| *byte == b'x'));

    // A body of exactly the cap is whole.
    let exact = client.fetch(&capped("/exact", 4096)).await.unwrap();
    assert!(!exact.truncated);
    assert_eq!(exact.body.len(), 4096);
    let one_short = client.fetch(&capped("/exact", 4095)).await.unwrap();
    assert!(one_short.truncated);
    assert_eq!(one_short.body.len(), 4095);

    // The cap counts decoded bytes: a small transfer cannot unpack into
    // memory without limit.
    let bomb = client.fetch(&capped("/bomb", 100_000)).await.unwrap();
    assert!(bomb.truncated);
    assert_eq!(bomb.body.len(), 100_000);

    // The body is what a page would see: decoded.
    let packed = client
        .fetch(&get("http://site.kage.test/packed"))
        .await
        .unwrap();
    assert_eq!(packed.text(), "decoded text");
    assert!(!packed.truncated);
    assert_eq!(packed.header("content-encoding"), None);

    // A request cannot ask for more than the client allows.
    let factory = ClientFactory::in_memory(HttpSettings {
        max_body_bytes: 1000,
        ..HttpSettings::default()
    });
    let small = factory
        .ephemeral(
            &spec("chrome_149", ClaimedOs::Windows),
            &world.proxy.endpoint(),
        )
        .unwrap();
    let fetched = small.fetch(&capped("/big", 1_000_000)).await.unwrap();
    assert!(fetched.truncated);
    assert_eq!(fetched.body.len(), 1000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_request_is_never_sent() {
    let world = World::start().await;
    let client = world.chrome();

    for name in [
        "User-Agent",
        "Sec-Fetch-Site",
        "sec-ch-ua",
        "Accept-Encoding",
        "Cookie",
    ] {
        let error = client
            .fetch(&get("http://site.kage.test/").header(name, "x"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, HttpError::Header(HeaderError::Denied { name: denied, .. }) if denied == name),
            "{name}: {error:?}"
        );
    }
    let error = client
        .fetch(&get("http://site.kage.test/").header("Referer", "not a url"))
        .await
        .unwrap_err();
    assert!(matches!(&error, HttpError::InvalidReferer(text) if text == "not a url"));
    assert!(matches!(
        client
            .fetch(&get("http://site.kage.test/").header("Referer", "ftp://x.example/"))
            .await,
        Err(HttpError::InvalidReferer(_))
    ));
    let mut put = get("http://site.kage.test/");
    put.method = Method::PUT;
    assert!(matches!(
        client.fetch(&put).await,
        Err(HttpError::Method(method)) if method == "PUT"
    ));
    assert!(matches!(
        client.fetch(&get("ftp://site.kage.test/")).await,
        Err(HttpError::Denied(SsrfDenial::Scheme(_)))
    ));
    let mut odd = FetchSpec::operator_get(url("ftp://site.kage.test/"));
    odd.guard = Guard::OperatorEndpoint;
    assert!(matches!(
        client.fetch(&odd).await,
        Err(HttpError::InvalidUrl(_))
    ));

    assert!(world.proxy.seen().is_empty(), "{:?}", world.proxy.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_of_the_proxy_the_tunnel_and_the_clock_are_told_apart() {
    let world = World::start().await;
    page(&world.site, "/ok", ResponseTemplate::new(200)).await;
    page(
        &world.site,
        "/slow",
        ResponseTemplate::new(200).set_delay(Duration::from_secs(5)),
    )
    .await;
    let factory = world.factory();
    let chrome = spec("chrome_149", ClaimedOs::Windows);

    // Wrong credentials: on a plain request and when opening a tunnel.
    let wrong = factory
        .ephemeral(&chrome, &world.proxy.endpoint_with("nobody", "wrong"))
        .unwrap();
    let error = wrong
        .fetch(&get("http://site.kage.test/ok"))
        .await
        .unwrap_err();
    assert!(matches!(error, HttpError::ProxyAuth));
    assert!(error.is_proxy_failure());
    assert!(matches!(
        wrong
            .fetch(&FetchSpec::operator_get(TestEcho::url("/")))
            .await,
        Err(HttpError::ProxyAuth)
    ));
    assert_eq!(world.proxy.refused(), 2);
    // The password is in no message.
    assert!(!error.to_string().contains("wrong"));

    // A target the proxy cannot reach: the tunnel is refused; a plain
    // request gets the proxy's own answer.
    let client = world.chrome();
    let error = client
        .fetch(&FetchSpec::operator_get(url("https://nowhere.kage.test/")))
        .await
        .unwrap_err();
    assert!(matches!(error, HttpError::Connect(_)), "{error:?}");
    assert!(error.is_proxy_failure());
    let plain = client
        .fetch(&get("http://nowhere.kage.test/"))
        .await
        .unwrap();
    assert_eq!(plain.status, 502);

    // Time runs out while waiting for the response.
    let mut slow = get("http://site.kage.test/slow");
    slow.timeout = Some(Duration::from_millis(300));
    let error = client.fetch(&slow).await.unwrap_err();
    assert!(
        matches!(
            error,
            HttpError::Timeout {
                stage: Stage::WaitingForResponse,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.is_timeout());

    // Time runs out while the proxy is still opening the tunnel.
    world.proxy.set_delay(Duration::from_secs(5));
    let mut stuck = FetchSpec::operator_get(TestEcho::url("/"));
    stuck.timeout = Some(Duration::from_millis(300));
    let error = world.chrome().fetch(&stuck).await.unwrap_err();
    assert!(
        matches!(
            error,
            HttpError::Timeout {
                stage: Stage::Connecting,
                ..
            }
        ),
        "{error:?}"
    );
    world.proxy.set_delay(Duration::ZERO);

    // A dead proxy.
    world.proxy.set_up(false);
    let error = world
        .chrome()
        .fetch(&get("http://site.kage.test/ok"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, HttpError::Connect(_) | HttpError::Request(_)),
        "{error:?}"
    );
    let error = world
        .chrome()
        .fetch(&FetchSpec::operator_get(TestEcho::url("/")))
        .await
        .unwrap_err();
    assert!(matches!(error, HttpError::Connect(_)), "{error:?}");
}

/// Follow-up F2 of the M2 live check: connection setup is told apart from
/// the request, and the tunnel from the handshake.
#[tokio::test(flavor = "multi_thread")]
async fn timing_separates_the_tunnel_the_handshake_and_the_wait() {
    let world = World::start().await;
    world.proxy.set_delay(Duration::from_millis(150));
    let factory = world.factory();
    let client = factory
        .client(
            &spec("chrome_149", ClaimedOs::Windows),
            &world.proxy.endpoint(),
        )
        .await
        .unwrap();
    let echo = FetchSpec::operator_get(TestEcho::url("/"));

    // A fresh connection: the proxy's slowness is in `connect`, not in
    // `tls`.
    let first = client.fetch(&echo).await.unwrap();
    let timing = first.timing;
    let connect = timing.connect_ms.unwrap();
    let tls = timing.tls_ms.unwrap();
    assert!((150..2000).contains(&connect), "{timing:?}");
    assert!(
        tls < 150,
        "the handshake on loopback took {tls} ms: {timing:?}"
    );
    assert!(timing.ttfb_ms.unwrap() >= connect + tls, "{timing:?}");
    assert!(timing.total_ms >= timing.ttfb_ms.unwrap());
    assert_eq!(timing.dns_ms, None);
    let hop = &first.hops[0];
    assert!(hop.setup.opened_connection());
    assert!(hop.waited >= Duration::from_millis(150));

    // The connection is kept: the next request pays for neither.
    let second = client.fetch(&echo).await.unwrap();
    assert_eq!(second.timing.connect_ms, None, "{:?}", second.timing);
    assert_eq!(second.timing.tls_ms, None);
    assert!(!second.hops[0].setup.opened_connection());
    assert!(second.timing.ttfb_ms.unwrap() < 150, "{:?}", second.timing);
    assert_eq!(world.proxy.tunnels().len(), 1);
}
