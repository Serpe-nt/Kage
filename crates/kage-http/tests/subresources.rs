//! Subresource emulation end to end (spec §9), through a local proxy to
//! wiremock: which assets are fetched, with which headers, in which order,
//! what the freshness cache spares, and what the limits cut.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{TestEcho, TestProxy, factory, spec};
use kage_core::config::SubresourceScope;
use kage_core::domain::ClaimedOs;
use kage_core::seed::Seed;
use kage_core::ssrf::SsrfPolicy;
use kage_http::{ClientFactory, FetchSpec, HttpClient, HttpSettings};
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const PAGE: &str = r#"<!doctype html>
<html>
<head>
  <title>Shop</title>
  <link rel="stylesheet" href="/css/main.css">
  <script src="/js/app.js"></script>
  <link rel="stylesheet" href="http://static.site.kage.test/css/theme.css">
  <script src="http://cdn.other.test/lib.js"></script>
  <script src="http://tracker.other.test/tracker.js"></script>
  <link rel="icon" href="/img/icon.png">
</head>
<body><img src="/img/hero.jpg"><script src="/js/late.js"></script></body>
</html>"#;

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
        proxy.route("static.site.kage.test:80", *site.address());
        proxy.route("other.kage.test:80", *other.address());
        proxy.route("cdn.other.test:80", *other.address());
        proxy.route("tracker.other.test:80", *other.address());
        Self {
            proxy,
            site,
            other,
            echo,
        }
    }

    fn client(&self, factory: &ClientFactory, profile: &str) -> HttpClient {
        factory
            .ephemeral(&spec(profile, ClaimedOs::Windows), &self.proxy.endpoint())
            .unwrap()
    }
}

async fn serve(server: &MockServer, at: &str, response: ResponseTemplate) {
    Mock::given(path(at))
        .respond_with(response)
        .mount(server)
        .await;
}

fn html(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/html; charset=utf-8")
}

fn asset(body: &str, media: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, media)
}

fn page_with_assets(at: &str) -> FetchSpec {
    let mut spec = FetchSpec::get(
        Url::parse(&format!("http://site.kage.test{at}")).unwrap(),
        SsrfPolicy::default(),
    );
    spec.fetch_subresources = Some(true);
    spec.seed = Some(Seed::from_u64(7));
    spec
}

async fn paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect()
}

fn header(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}

async fn standard_site(world: &World) {
    serve(&world.site, "/shop", html(PAGE)).await;
    serve(
        &world.site,
        "/css/main.css",
        asset("body{}", "text/css").insert_header("cache-control", "max-age=600"),
    )
    .await;
    serve(
        &world.site,
        "/js/app.js",
        asset("var a;", "text/javascript").insert_header("etag", "\"app-1\""),
    )
    .await;
    serve(&world.site, "/css/theme.css", asset("p{}", "text/css")).await;
    serve(
        &world.site,
        "/js/late.js",
        asset("var l;", "text/javascript"),
    )
    .await;
    serve(&world.site, "/img/icon.png", asset("png", "image/png")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pages_own_assets_are_fetched_in_document_order_and_the_icon_last() {
    let world = World::start().await;
    standard_site(&world).await;
    let factory = factory(&world.echo);
    let client = world.client(&factory, "chrome_149");

    let fetched = client.fetch(&page_with_assets("/shop")).await.unwrap();

    assert_eq!(fetched.status, 200);
    let report = fetched.subresources.expect("a page has a report");
    assert_eq!(report.seed, Seed::from_u64(7));
    let summary: Vec<(&str, &str, Option<u16>)> = report
        .assets
        .iter()
        .map(|asset| (asset.url.as_str(), asset.kind, asset.status))
        .collect();
    assert_eq!(
        summary,
        [
            ("http://site.kage.test/css/main.css", "style", Some(200)),
            ("http://site.kage.test/js/app.js", "script", Some(200)),
            (
                "http://static.site.kage.test/css/theme.css",
                "style",
                Some(200)
            ),
            ("http://site.kage.test/js/late.js", "script", Some(200)),
            ("http://site.kage.test/img/icon.png", "image", Some(200)),
        ]
    );
    assert!(
        report
            .assets
            .iter()
            .all(|asset| asset.error.is_none() && !asset.from_cache)
    );
    assert_eq!(report.requests, 5);
    assert_eq!(report.bytes, 6 + 6 + 3 + 6 + 3);
    // The icon waits at least a tenth of a second after the rest.
    assert!(report.duration_ms >= 100, "{report:?}");

    // The site saw the page first and the icon last.
    let seen = paths(&world.site).await;
    assert_eq!(seen.first().map(String::as_str), Some("/shop"));
    assert_eq!(seen.last().map(String::as_str), Some("/img/icon.png"));
    assert_eq!(seen.len(), 6);
    // Another site's script and the image were never asked for.
    assert_eq!(paths(&world.other).await, Vec::<String>::new());
    assert!(!seen.contains(&"/img/hero.jpg".to_owned()));

    // Each kind carries its own Accept, and the page as Referer.
    let requests = world.site.received_requests().await.unwrap();
    let by_path = |wanted: &str| {
        requests
            .iter()
            .find(|request| request.url.path() == wanted)
            .unwrap()
    };
    assert_eq!(
        header(by_path("/css/main.css"), "accept").as_deref(),
        Some("text/css,*/*;q=0.1")
    );
    assert_eq!(
        header(by_path("/js/app.js"), "accept").as_deref(),
        Some("*/*")
    );
    assert!(
        header(by_path("/img/icon.png"), "accept")
            .unwrap()
            .starts_with("image/avif")
    );
    for at in ["/css/main.css", "/js/app.js", "/img/icon.png"] {
        let request = by_path(at);
        assert_eq!(
            header(request, "referer").as_deref(),
            Some("http://site.kage.test/shop"),
            "{at}"
        );
        assert_eq!(header(request, "upgrade-insecure-requests"), None, "{at}");
    }
    // The asset on the sibling host gets the page's origin only.
    assert_eq!(
        header(by_path("/css/theme.css"), "referer").as_deref(),
        Some("http://site.kage.test/")
    );
    assert_eq!(
        header(by_path("/css/theme.css"), "host").as_deref(),
        Some("static.site.kage.test")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_visitor_asks_only_for_what_a_browser_would_ask_again() {
    let world = World::start().await;
    standard_site(&world).await;
    let factory = factory(&world.echo);
    let client = world.client(&factory, "firefox_151");

    client.fetch(&page_with_assets("/shop")).await.unwrap();
    let first_visit = paths(&world.site).await.len();
    assert_eq!(first_visit, 6);

    // The script's validator now makes the server answer "not modified".
    world.site.reset().await;
    serve(&world.site, "/shop", html(PAGE)).await;
    serve(&world.site, "/js/app.js", ResponseTemplate::new(304)).await;
    serve(&world.site, "/css/theme.css", asset("p{}", "text/css")).await;
    serve(
        &world.site,
        "/js/late.js",
        asset("var l;", "text/javascript"),
    )
    .await;
    serve(&world.site, "/img/icon.png", ResponseTemplate::new(404)).await;

    let again = client.fetch(&page_with_assets("/shop")).await.unwrap();
    let report = again.subresources.unwrap();
    let by_path = |wanted: &str| {
        report
            .assets
            .iter()
            .find(|asset| asset.url.path() == wanted)
            .unwrap()
    };

    // Fresh for ten minutes: not asked for.
    let stylesheet = by_path("/css/main.css");
    assert!(stylesheet.from_cache);
    assert_eq!(stylesheet.status, None);
    assert!(
        !paths(&world.site)
            .await
            .contains(&"/css/main.css".to_owned())
    );
    // Stored with a validator only: revalidated.
    let script = by_path("/js/app.js");
    assert!(script.revalidated && !script.from_cache);
    assert_eq!(script.status, Some(304));
    let requests = world.site.received_requests().await.unwrap();
    let revalidation = requests
        .iter()
        .find(|request| request.url.path() == "/js/app.js")
        .unwrap();
    assert_eq!(
        header(revalidation, "if-none-match").as_deref(),
        Some("\"app-1\"")
    );
    // Nothing to reuse: fetched again as before.
    assert!(!by_path("/css/theme.css").revalidated);
    assert_eq!(by_path("/css/theme.css").status, Some(200));
    // A missing icon is a status, not a failure, and is not remembered.
    assert_eq!(by_path("/img/icon.png").status, Some(404));
    assert_eq!(by_path("/img/icon.png").error, None);
    assert_eq!(report.requests, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_scope_and_failures_shape_what_is_fetched() {
    let world = World::start().await;
    serve(
        &world.site,
        "/many",
        html(
            "<link rel=stylesheet href=/a.css><link rel=stylesheet href=/big.css>\
             <script src=/moved.js></script><script src=/away.js></script>\
             <script src=/gone.js></script><script src=/never.js></script>",
        ),
    )
    .await;
    serve(&world.site, "/a.css", asset("a{}", "text/css")).await;
    serve(
        &world.site,
        "/big.css",
        asset(&"x".repeat(5000), "text/css"),
    )
    .await;
    serve(
        &world.site,
        "/moved.js",
        ResponseTemplate::new(302).insert_header("location", "/js/real.js"),
    )
    .await;
    serve(
        &world.site,
        "/js/real.js",
        asset("var r;", "text/javascript"),
    )
    .await;
    serve(
        &world.site,
        "/away.js",
        ResponseTemplate::new(302).insert_header("location", "http://other.kage.test/x.js"),
    )
    .await;
    serve(&world.site, "/gone.js", ResponseTemplate::new(500)).await;
    let settings = HttpSettings {
        subresources: kage_http::client::SubresourceSettings {
            scope: SubresourceScope::SameOrigin,
            max: 6,
            max_concurrent: 2,
            max_bytes_each: 1000,
            max_bytes_total: 8_000_000,
        },
        ..HttpSettings::default()
    };
    let factory = ClientFactory::in_memory(settings);
    let client = world.client(&factory, "chrome_149");

    let report = client
        .fetch(&page_with_assets("/many"))
        .await
        .unwrap()
        .subresources
        .unwrap();

    let outcome: Vec<(&str, Option<u16>, Option<&str>)> = report
        .assets
        .iter()
        .map(|asset| (asset.url.path(), asset.status, asset.error.as_deref()))
        .collect();
    assert_eq!(
        outcome,
        [
            ("/a.css", Some(200), None),
            // Read up to the limit, then abandoned.
            ("/big.css", Some(200), Some("larger than the byte limit")),
            // A redirect inside the site is followed.
            ("/moved.js", Some(200), None),
            // A redirect out of it is not.
            ("/away.js", Some(302), Some("redirected to another site")),
            ("/gone.js", Some(500), None),
            // Six in all: five assets and the icon, so `/never.js` waits
            // for nobody. There is no icon link: /favicon.ico is asked for.
            ("/favicon.ico", Some(404), None),
        ]
    );
    assert_eq!(report.assets[1].bytes, 1000);
    assert_eq!(paths(&world.other).await, Vec::<String>::new());
    assert!(!paths(&world.site).await.contains(&"/never.js".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_page_fetched_with_get_has_assets() {
    let world = World::start().await;
    standard_site(&world).await;
    serve(&world.site, "/data.json", asset(PAGE, "application/json")).await;
    serve(
        &world.site,
        "/missing",
        ResponseTemplate::new(404).set_body_raw(PAGE, "text/html"),
    )
    .await;
    let factory = factory(&world.echo);
    let client = world.client(&factory, "chrome_149");

    // Not HTML, and not a 200.
    for at in ["/data.json", "/missing"] {
        let fetched = client.fetch(&page_with_assets(at)).await.unwrap();
        assert_eq!(fetched.subresources, None, "{at}");
    }
    // Off unless asked for: the client's setting is the default, off.
    let mut plain = page_with_assets("/shop");
    plain.fetch_subresources = None;
    assert_eq!(client.fetch(&plain).await.unwrap().subresources, None);
    // A HEAD has no page to look at.
    let mut head = page_with_assets("/shop");
    head.method = http::Method::HEAD;
    assert_eq!(client.fetch(&head).await.unwrap().subresources, None);

    assert_eq!(
        paths(&world.site).await,
        ["/data.json", "/missing", "/shop", "/shop"]
    );

    // Without a seed one is drawn and reported, and two runs differ.
    let mut unseeded = page_with_assets("/shop");
    unseeded.seed = None;
    let first = client.fetch(&unseeded).await.unwrap().subresources.unwrap();
    let second = client.fetch(&unseeded).await.unwrap().subresources.unwrap();
    assert_ne!(first.seed, second.seed);
}
