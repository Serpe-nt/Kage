//! The plain client of level 0 (spec §25) end to end, through a local
//! proxy: to wiremock over plain HTTP, and to the fingerprint echo over TLS.
//!
//! Covered here: Kage's minimal header set and its order on the wire, a
//! caller's headers and body as given, what stays denied, the cookies of
//! one redirect chain and nothing kept after it, the caller's credentials
//! only to the request's origin, redirects through the SSRF guard, and the
//! wire fingerprint against its recorded fixture.
//!
//! To record the fixture after a deliberate change (a wreq bump):
//!
//! ```text
//! KAGE_RECORD_FINGERPRINTS=1 cargo test -p kage-http --test plain
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use bytes::Bytes;
use common::{ScriptedResolver, TestEcho, TestProxy, factory};
use http::Method;
use kage_core::ssrf::{DeniedRange, SsrfDenial, SsrfPolicy};
use kage_http::headers::HeaderError;
use kage_http::selftest::Observed;
use kage_http::{
    ClientFactory, Guard, HttpError, PLAIN_ACCEPT, PLAIN_ACCEPT_ENCODING, PlainClient, PlainSpec,
};
use serde::{Deserialize, Serialize};
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const STRICT: SsrfPolicy = SsrfPolicy {
    allow_ip_targets: false,
};

const AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn get(text: &str) -> PlainSpec {
    PlainSpec {
        url: url(text),
        method: Method::GET,
        headers: Vec::new(),
        body: None,
        guard: Guard::Target(STRICT),
        timeout: None,
        max_body_bytes: None,
        max_redirects: None,
    }
}

fn with_headers(mut spec: PlainSpec, headers: &[(&str, &str)]) -> PlainSpec {
    spec.headers = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    spec
}

/// A proxy with two plain sites behind it, `site.kage.test` and
/// `other.kage.test`, and the echo.
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

    fn client(&self) -> PlainClient {
        self.factory().plain(&self.proxy.endpoint(), AGENT).unwrap()
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
async fn a_plain_request_says_who_it_is_in_a_well_formed_head() {
    let world = World::start().await;
    page(
        &world.site,
        "/page",
        ResponseTemplate::new(200).set_body_string("hi"),
    )
    .await;

    let fetched = world
        .client()
        .fetch(&get("http://site.kage.test/page"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.text(), "hi");

    // On HTTP/1.1, as the proxy saw the head: Host first, then Kage's own
    // three, nothing else.
    let seen = world.proxy.plain();
    assert_eq!(seen.len(), 1);
    let names = seen[0].header_names();
    assert_eq!(
        &names[..4],
        ["Host", "User-Agent", "Accept", "Accept-Encoding"]
    );
    // After them, only what the proxy is told.
    assert!(
        names[4..]
            .iter()
            .all(|name| name.eq_ignore_ascii_case("proxy-authorization")),
        "{names:?}"
    );
    assert_eq!(seen[0].header("User-Agent").as_deref(), Some(AGENT));
    assert_eq!(seen[0].header("Accept").as_deref(), Some(PLAIN_ACCEPT));
    assert_eq!(
        seen[0].header("Accept-Encoding").as_deref(),
        Some(PLAIN_ACCEPT_ENCODING)
    );
    // Nothing of a browser: no fetch metadata, no client hints, no language.
    for absent in [
        "Sec-Fetch-Mode",
        "Sec-CH-UA",
        "Accept-Language",
        "Upgrade-Insecure-Requests",
        "Cookie",
    ] {
        assert_eq!(seen[0].header(absent), None, "{absent}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_http2_and_http1_over_tls_the_echo_sees_the_same_set() {
    let world = World::start().await;
    let client = world.client();
    for (target, version) in [
        (TestEcho::url("/"), "h2"),
        (TestEcho::h1_url("/"), "http/1.1"),
    ] {
        let fetched = client.fetch(&get(target.as_str())).await.unwrap();
        let observed = Observed::parse(&fetched.body).unwrap();
        assert_eq!(observed.http_version, version);
        let names: Vec<&str> = observed
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        if version == "h2" {
            assert_eq!(names, ["user-agent", "accept", "accept-encoding"]);
        } else {
            // The echo reports HTTP/1.1 names as written.
            assert_eq!(names, ["Host", "User-Agent", "Accept", "Accept-Encoding"]);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callers_headers_and_body_go_as_given_and_its_own_agent_replaces_kages() {
    let world = World::start().await;
    page(&world.site, "/api", ResponseTemplate::new(201)).await;
    let mut spec = with_headers(
        get("http://site.kage.test/api"),
        &[
            ("User-Agent", "my-tool/3.1"),
            ("X-Request-Trace", "abc"),
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("Cookie", "session=s1"),
            ("Authorization", "Bearer t0ken"),
            ("Sec-CH-UA", "\"Any\";v=\"1\""),
        ],
    );
    spec.method = Method::POST;
    spec.body = Some(Bytes::from_static(b"{\"a\":1}"));

    let fetched = world.client().fetch(&spec).await.unwrap();
    assert_eq!(fetched.status, 201);

    let seen = &world.proxy.plain()[0];
    assert_eq!(seen.method, "POST");
    let names = seen.header_names();
    let at = |name: &str| names.iter().position(|found| found == name).unwrap();
    // Kage's place for the caller's own agent and accept; the rest in the
    // caller's order and spelling, after Kage's.
    assert_eq!(
        &names[..4],
        ["Host", "User-Agent", "Accept", "Accept-Encoding"]
    );
    assert!(at("X-Request-Trace") < at("Content-Type"));
    assert!(at("Content-Type") < at("Cookie"));
    assert!(at("Cookie") < at("Authorization"));
    assert!(at("Authorization") < at("Sec-CH-UA"));
    assert_eq!(seen.header("User-Agent").as_deref(), Some("my-tool/3.1"));
    assert_eq!(seen.header("Accept").as_deref(), Some("application/json"));

    let request = &received(&world.site).await[0];
    assert_eq!(request.body, b"{\"a\":1}");
    assert_eq!(header(request, "cookie").as_deref(), Some("session=s1"));
    assert_eq!(
        header(request, "authorization").as_deref(),
        Some("Bearer t0ken")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_transport_the_proxy_and_the_forwarding_family_stay_kages() {
    let world = World::start().await;
    let client = world.client();
    for name in [
        "Host",
        "Connection",
        "Content-Length",
        "Transfer-Encoding",
        "Proxy-Authorization",
        "Proxy-Connection",
        "X-Forwarded-For",
        "Forwarded",
        "X-Real-IP",
        "CF-Connecting-IP",
        "Accept-Encoding",
        "Via",
    ] {
        let error = client
            .fetch(&with_headers(get("http://site.kage.test/"), &[(name, "x")]))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, HttpError::Header(HeaderError::Denied { name: denied, .. }) if denied == name),
            "{name}: {error}"
        );
    }
    assert_eq!(world.proxy.plain().len(), 0, "nothing was sent");
    // Methods beyond the three are refused.
    let mut put = get("http://site.kage.test/");
    put.method = Method::PUT;
    assert!(matches!(
        client.fetch(&put).await,
        Err(HttpError::Method(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_set_in_the_chain_goes_to_its_later_hops_and_is_kept_by_nobody() {
    let world = World::start().await;
    page(
        &world.site,
        "/start",
        redirect(302, "/check").insert_header("set-cookie", "check=1; Path=/"),
    )
    .await;
    page(
        &world.site,
        "/check",
        ResponseTemplate::new(200).set_body_string("ok"),
    )
    .await;
    let client = world.client();

    let fetched = client
        .fetch(&get("http://site.kage.test/start"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.redirects().len(), 1);
    let requests = received(&world.site).await;
    assert_eq!(header(&requests[0], "cookie"), None);
    assert_eq!(header(&requests[1], "cookie").as_deref(), Some("check=1"));

    // The next request of the same client is a new chain: nothing kept.
    client
        .fetch(&get("http://site.kage.test/check"))
        .await
        .unwrap();
    assert_eq!(header(&received(&world.site).await[2], "cookie"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callers_credentials_never_cross_to_another_origin() {
    let world = World::start().await;
    page(
        &world.site,
        "/go",
        redirect(302, "http://other.kage.test/land"),
    )
    .await;
    page(
        &world.other,
        "/land",
        redirect(302, "http://site.kage.test/back"),
    )
    .await;
    page(&world.site, "/back", ResponseTemplate::new(200)).await;

    let fetched = world
        .client()
        .fetch(&with_headers(
            get("http://site.kage.test/go"),
            &[
                ("Cookie", "session=s1"),
                ("Authorization", "Bearer t0ken"),
                ("X-Keep", "1"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.final_url.as_str(), "http://site.kage.test/back");

    let site = received(&world.site).await;
    let other = received(&world.other).await;
    // At the request's origin, first and last: the caller's credentials.
    for request in [&site[0], &site[1]] {
        assert_eq!(header(request, "cookie").as_deref(), Some("session=s1"));
        assert_eq!(
            header(request, "authorization").as_deref(),
            Some("Bearer t0ken")
        );
    }
    // At another origin: neither, and the rest as given.
    assert_eq!(header(&other[0], "cookie"), None);
    assert_eq!(header(&other[0], "authorization"), None);
    assert_eq!(header(&other[0], "x-keep").as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_are_followed_through_the_guard_and_a_303_drops_the_body() {
    let world = World::start().await;
    page(&world.site, "/form", redirect(303, "/done")).await;
    page(&world.site, "/done", ResponseTemplate::new(200)).await;
    page(
        &world.site,
        "/inside",
        redirect(302, "http://10.0.0.5/admin"),
    )
    .await;
    let client = world.client();

    let mut post = with_headers(
        get("http://site.kage.test/form"),
        &[("Content-Type", "application/x-www-form-urlencoded")],
    );
    post.method = Method::POST;
    post.body = Some(Bytes::from_static(b"a=1"));
    let fetched = client.fetch(&post).await.unwrap();
    assert_eq!(fetched.status, 200);
    let requests = received(&world.site).await;
    assert_eq!(requests[1].method.as_str(), "GET");
    assert_eq!(requests[1].body, b"");
    assert_eq!(header(&requests[1], "content-type"), None);

    // A hop the guard refuses is never asked for.
    let error = client
        .fetch(&get("http://site.kage.test/inside"))
        .await
        .unwrap_err();
    assert!(matches!(error, HttpError::RedirectDenied(_)), "{error}");
    assert!(
        !world
            .proxy
            .plain()
            .iter()
            .any(|seen| seen.target.contains("10.0.0.5"))
    );
    // And the target itself.
    let refused = client
        .fetch(&get("http://169.254.169.254/latest"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            HttpError::Denied(SsrfDenial::Address { .. } | SsrfDenial::IpLiteral(_))
        ),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // One client, every way a hop is refused by its addresses.
async fn with_a_resolver_the_plain_client_resolves_every_hop_before_asking_for_it() {
    let world = World::start().await;
    page(
        &world.site,
        "/go",
        redirect(302, "http://other.kage.test/in"),
    )
    .await;
    page(
        &world.other,
        "/in",
        ResponseTemplate::new(200).set_body_string("INTERNAL"),
    )
    .await;
    let resolver = ScriptedResolver::new();
    let client = world
        .factory()
        .with_resolver(resolver.clone())
        .plain(&world.proxy.endpoint(), AGENT)
        .unwrap();
    let asked_for = |host: &str| {
        world
            .proxy
            .plain()
            .iter()
            .filter(|seen| seen.target.contains(host))
            .count()
    };

    // Public addresses on both hops: the fetch goes through, and both
    // names were looked up, each before its request.
    let fetched = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.text(), "INTERNAL");
    assert_eq!(resolver.asked(), ["site.kage.test", "other.kage.test"]);
    assert_eq!(asked_for("other.kage.test"), 1);

    // The hop's name points into a private network, alone or among public
    // addresses: the fetch ends as a refused hop, with the hop named, and
    // the hop is never asked for.
    for addresses in [vec!["10.0.0.7"], vec!["93.184.216.34", "10.0.0.7"]] {
        resolver.answer("other.kage.test", &addresses);
        let error = client
            .fetch(&get("http://site.kage.test/go"))
            .await
            .unwrap_err();
        let HttpError::RedirectDenied(denied) = &error else {
            panic!("expected a denied redirect, got {error:?}");
        };
        assert!(error.is_ssrf_denied());
        assert_eq!(denied.hop, 1);
        assert_eq!(denied.location, "http://other.kage.test/in");
        assert_eq!(
            denied.reason,
            SsrfDenial::Address {
                address: "10.0.0.7".parse().unwrap(),
                range: DeniedRange::Private
            }
        );
        assert_eq!(asked_for("other.kage.test"), 1);
    }

    // A hop whose name has no address, or that the host cannot look up,
    // is refused the same way.
    resolver.answer("other.kage.test", &[]);
    let error = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, HttpError::RedirectDenied(denied) if denied.hop == 1
            && denied.reason == SsrfDenial::Unresolved("other.kage.test".to_owned())),
        "{error:?}"
    );
    resolver.fail("other.kage.test", "SERVFAIL");
    let error = client
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, HttpError::RedirectDenied(denied) if denied.hop == 1
            && denied.reason == SsrfDenial::Unresolved("other.kage.test".to_owned())),
        "{error:?}"
    );
    assert_eq!(asked_for("other.kage.test"), 1);
    assert_eq!(received(&world.other).await.len(), 1);

    // The first URL itself points at this machine: nothing is sent at all.
    resolver.answer("site.kage.test", &["127.0.0.1"]);
    let before = world.proxy.plain().len();
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
    assert_eq!(world.proxy.plain().len(), before);

    // Without a resolver (the default) the proxy resolves: the same
    // redirect is followed, as before, and nobody is asked here.
    let asked = resolver.asked().len();
    let fetched = world
        .client()
        .fetch(&get("http://site.kage.test/go"))
        .await
        .unwrap();
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.text(), "INTERNAL");
    assert_eq!(asked_for("other.kage.test"), 2);
    assert_eq!(resolver.asked().len(), asked);
}

/// What the plain client looks like on the wire, as recorded.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Recorded {
    h2: Wire,
    http1: Wire,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Wire {
    ja4: String,
    akamai: Option<String>,
    headers: Vec<String>,
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fingerprints/plain.json")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_fingerprint_is_the_recorded_one() {
    let world = World::start().await;
    let client = world.client();
    let mut wire = Vec::new();
    for target in [TestEcho::url("/"), TestEcho::h1_url("/")] {
        let fetched = client.fetch(&get(target.as_str())).await.unwrap();
        let observed = Observed::parse(&fetched.body).unwrap();
        wire.push(Wire {
            ja4: observed.ja4,
            akamai: observed.akamai,
            headers: observed
                .headers
                .into_iter()
                .map(|(name, _)| name.to_ascii_lowercase())
                .collect(),
        });
    }
    let http1 = wire.pop().unwrap();
    let h2 = wire.pop().unwrap();
    let observed = Recorded { h2, http1 };
    if std::env::var("KAGE_RECORD_FINGERPRINTS").is_ok_and(|value| value == "1") {
        let mut text = serde_json::to_string_pretty(&observed).unwrap();
        text.push('\n');
        std::fs::write(fixture(), text).unwrap();
        return;
    }
    let text = std::fs::read_to_string(fixture())
        .unwrap_or_else(|_| panic!("no fixture; record it (see this file's header)"));
    let recorded: Recorded = serde_json::from_str(&text).unwrap();
    assert_eq!(
        observed, recorded,
        "the plain client's wire fingerprint moved; record it again if that was meant"
    );
}
