//! The provider conformance suite (spec §7.1, ADR 0013): what every
//! [`ProxyProvider`] must do, checked against a mock of its API.
//!
//! A provider is a subject: a [`World`] that builds the provider against
//! its mock and scripts the mock (what it lists, a proxy that leaves the
//! list, a swap the provider makes by itself, a replacement that finishes
//! or fails, a 429, a refused key, a source that cannot be read). Six rules
//! run against it, each returning the violations it found rather than
//! panicking, so that one run says everything a provider breaks:
//!
//! | Rule | What it holds |
//! |---|---|
//! | [`listing`] | the list is complete, its references are stable and unique, every entry is a proxy one can log in to |
//! | [`credentials`] | no credential and no key reaches a log line, a `Debug` output, or an error's text, whatever happens |
//! | [`state_machine`] | through a real pool on Postgres: an unchanged list changes nothing, `new` → `healthy`, a proxy that leaves is retired, and where replacement is offered a due proxy is replaced or, refused, back in `backoff`; where it is not, nothing is ever asked |
//! | [`replaced_log`] | where offered, a swap the provider made by itself is in its log; where not, the answer is `Unsupported` |
//! | [`rate_limits`] | a 429 with `Retry-After` is waited out before the next request (not applicable to a provider without an API) |
//! | [`typed_errors`] | what the capabilities say is unsupported answers `Unsupported` with the kind and the operation, nothing else does; a refused key is `Unauthorized`; a source that cannot be read is the provider's typed error for it |
//!
//! Subjects: the Webshare provider against `MockWebshare` and the declared
//! list over a file must pass every rule; a provider written to break each
//! rule once must make every rule report. A contributor adds a subject for
//! a new provider: a [`World`] over a mock written from the provider's
//! documentation (`CONTRIBUTING.md`, "Adding a proxy provider").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use common::{MASTER_KEY_BYTES, MockWebshare, PROXY_PASS, PROXY_USER, TestDb, WEBSHARE_TOKEN};
use kage_core::alert::MemoryAlertSink;
use kage_core::config::{ProxiesConfig, WebshareConfig};
use kage_core::domain::{
    HealthFailure, Proxy, ProxyHealth, ProxyProviderKind, ProxyScheme, ProxyState, ReplaceReason,
};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::secret::{SealedSecret, Secret};
use kage_core::seed::Seed;
use kage_proxy::machine::{ExitObservation, ProbeReport};
use kage_proxy::{
    Capabilities, Clock, CredentialCipher, ManualClock, Operation, Pool, PoolParts, PoolRules,
    ProviderError, ProviderProxy, ProxyProvider, ReplaceSpec, ReplaceState, ReplaceTicket,
    ReplacedProxy, ReplacementOutcome, StaticListProvider, WebshareProvider,
};
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// When the pool's clock starts in the state machine rule.
fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

/// A source the provider cannot read, and how its error must look.
type Unreadable = (
    Arc<dyn ProxyProvider>,
    fn(&ProviderError) -> bool,
    &'static str,
);

/// A provider and its mock, as the suite drives them.
trait World: Send + Sync {
    /// The provider over the mock.
    fn provider(&self) -> Arc<dyn ProxyProvider>;
    /// Lists `count` more proxies, each at an address of its own.
    fn list(&self, count: u32);
    /// Takes the first listed proxy off the list; returns its address.
    fn take_first(&self) -> Option<String>;
    /// The provider swaps the first listed proxy by itself and writes it
    /// in its log; returns the address that left. `None` when it keeps no
    /// log.
    fn replace_first_by_itself(&self) -> Option<String>;
    /// Replacements asked for finish at the next look.
    fn complete_replacements(&self);
    /// The next replacement asked for fails at the provider.
    fn fail_next_replacement(&self);
    /// The API answers the next list request with 429 and `Retry-After`;
    /// `false` for a provider without an API.
    fn rate_limit_next(&self, seconds: u64) -> bool;
    /// The API refuses the key at the next request; `false` without one.
    fn refuse_key(&self) -> bool;
    /// A provider whose source cannot be read, the test of its typed
    /// error, and that error's name.
    fn unreadable(&self) -> Option<Unreadable>;
    /// Every credential and key the provider holds or lists.
    fn secrets(&self) -> Vec<String>;
}

/// How a subject's world is made: fresh for every rule.
type Make = fn() -> Pin<Box<dyn Future<Output = Box<dyn World>>>>;

/// A proxy of the pool's domain for `item`, as the pool would hold it, for
/// the operations that take one.
fn as_proxy(provider: ProxyProviderKind, item: &ProviderProxy) -> Proxy {
    Proxy {
        id: ProxyId::new(),
        provider,
        provider_ref: item.provider_ref.clone(),
        host: item.host.clone(),
        port: item.port,
        username: SealedSecret::new(vec![1]),
        password: SealedSecret::new(vec![2]),
        scheme: ProxyScheme::Http,
        country: item.country,
        locale: None,
        timezone: None,
        city: None,
        asn: None,
        state: ProxyState::Healthy,
        consecutive_failures: 0,
        backoff_count: 0,
        backoff_until: None,
        failing_since: None,
        replacement: None,
        last_health: None,
        created_at: start(),
        retired_at: None,
    }
}

// --- The rules -----------------------------------------------------------------

/// The list is complete, stable and made of proxies one can log in to.
async fn listing(world: &dyn World) -> Vec<String> {
    let mut found = Vec::new();
    world.list(3);
    let provider = world.provider();
    let (first, second) = match (provider.list().await, provider.list().await) {
        (Ok(first), Ok(second)) => (first, second),
        (Err(error), _) | (_, Err(error)) => return vec![format!("listing failed: {error}")],
    };
    if first.len() != 3 {
        found.push(format!(
            "3 proxies were listed, the provider gave {}",
            first.len()
        ));
    }
    let refs = |items: &[ProviderProxy]| {
        items
            .iter()
            .map(|item| item.provider_ref.clone())
            .collect::<Vec<_>>()
    };
    let mut unique = refs(&first);
    unique.sort();
    unique.dedup();
    if unique.len() != first.len() {
        found.push("two listed proxies share one reference".to_owned());
    }
    if refs(&first) != refs(&second) {
        found.push("the references changed between two listings of one list".to_owned());
    }
    for item in &first {
        if item.provider_ref.trim().is_empty() {
            found.push("a listed proxy has an empty reference".to_owned());
        }
        if item.host.trim().is_empty() || item.host.contains(['/', ' ']) {
            found.push(format!("a listed host is no host: {:?}", item.host));
        }
        if item.port == 0 {
            found.push("a listed proxy has port 0".to_owned());
        }
        if item.username.expose().is_empty() {
            found.push("a listed proxy has no user name".to_owned());
        }
    }
    found
}

/// A writer that keeps what a test's log subscriber writes.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// No credential or key reaches a log line, a `Debug` output or an
/// error's text, whatever the provider is asked and whatever goes wrong.
async fn credentials(world: &dyn World) -> Vec<String> {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let logging = tracing::subscriber::set_default(subscriber);

    world.list(2);
    let provider = world.provider();
    let mut texts = vec![format!("{provider:?}")];
    let listed = provider.list().await;
    if let Ok(items) = &listed {
        texts.extend(items.iter().map(|item| format!("{item:?}")));
    }
    record(&mut texts, listed.map(|items| format!("{items:?}")));
    if let Ok(items) = provider.list().await
        && let Some(first) = items.first()
    {
        let proxy = as_proxy(provider.kind(), first);
        let ticket = provider.replace(&proxy, ReplaceSpec::default()).await;
        record(
            &mut texts,
            ticket.clone().map(|ticket| format!("{ticket:?}")),
        );
        let ticket = ticket.unwrap_or(ReplaceTicket("1".to_owned()));
        let state = provider.replace_status(&ticket).await;
        record(&mut texts, state.map(|state| format!("{state:?}")));
    }
    let since = Utc::now() - chrono::Duration::days(1);
    let log = provider.replaced(since).await;
    record(&mut texts, log.map(|log| format!("{log:?}")));
    let account = provider.account().await;
    record(&mut texts, account.map(|account| format!("{account:?}")));
    let stats = provider.stats(since).await;
    record(&mut texts, stats.map(|hours| format!("{hours:?}")));
    if world.refuse_key() {
        let refused = provider.list().await;
        record(&mut texts, refused.map(|items| format!("{items:?}")));
    }
    if let Some((unreadable, _, _)) = world.unreadable() {
        texts.push(format!("{unreadable:?}"));
        let failed = unreadable.list().await;
        record(&mut texts, failed.map(|items| format!("{items:?}")));
    }
    drop(logging);

    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    let mut found = Vec::new();
    for secret in world.secrets() {
        if log.contains(&secret) {
            found.push(format!(
                "a log line holds a credential ({} characters)",
                secret.len()
            ));
        }
        if texts.iter().any(|text| text.contains(&secret)) {
            found.push(format!(
                "a Debug output or an error holds a credential ({} characters)",
                secret.len()
            ));
        }
    }
    found.dedup();
    found
}

/// Keeps what an operation answered, as text: its `Debug`, or its error's
/// `Display` and `Debug`.
fn record(texts: &mut Vec<String>, result: Result<String, ProviderError>) {
    texts.push(match result {
        Ok(debug) => debug,
        Err(error) => format!("{error} {error:?}"),
    });
}

/// A probe that answered, from an exit address of the proxy's own.
fn answered(proxy: &Proxy, at: DateTime<Utc>) -> ProbeReport {
    ProbeReport {
        at,
        exit: Ok(ExitObservation {
            ip: std::net::IpAddr::V6(std::net::Ipv6Addr::from(proxy.id.as_uuid().as_u128())),
            latency_ms: 120,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(false),
        geo: None,
    }
}

/// Makes a proxy due for a replacement for unreachability.
async fn make_due(db: &TestDb, id: ProxyId, now: DateTime<Utc>) {
    db.store
        .proxies()
        .modify(id, |stored| {
            stored.state = ProxyState::Backoff;
            stored.backoff_count = 9;
            stored.backoff_until = Some(now + chrono::Duration::hours(1));
            stored.failing_since = Some(now - chrono::Duration::days(2));
            stored.last_health = Some(ProxyHealth {
                failure: Some(HealthFailure::Unreachable),
                ..ProxyHealth::new(now, false)
            });
        })
        .await
        .unwrap();
}

/// The pool's edges, driven through a real pool on Postgres.
// The edges in the order a pool meets them: long, and better read whole.
#[allow(clippy::too_many_lines)]
async fn state_machine(world: &dyn World) -> Vec<String> {
    let mut found = Vec::new();
    let db = TestDb::migrated().await;
    let clock = Arc::new(ManualClock::new(start()));
    let provider = world.provider();
    let capabilities = provider.capabilities();
    let pool = Pool::new(PoolParts {
        store: db.store.clone(),
        provider: Some(Arc::clone(&provider)),
        cipher: CredentialCipher::new(&MASTER_KEY_BYTES),
        rules: PoolRules::default(),
        missing_syncs: 1,
        alerts: Arc::new(MemoryAlertSink::new()),
        clock: clock.clone(),
        seed: Seed::from_u64(5),
    });
    world.list(4);

    match pool.reconcile().await {
        Ok(report) if report.added.len() == 4 => {}
        Ok(report) => found.push(format!("4 listed proxies, {} adopted", report.added.len())),
        Err(error) => return vec![format!("the first sync failed: {error}")],
    }
    let live = pool.live().await.unwrap();
    if live.iter().any(|proxy| proxy.state != ProxyState::New) {
        found.push("an adopted proxy is not new".to_owned());
    }
    match pool.reconcile().await {
        Ok(report) if report.added.is_empty() && report.retired.is_empty() => {}
        Ok(report) => found.push(format!(
            "a sync of an unchanged list added {} and retired {}",
            report.added.len(),
            report.retired.len()
        )),
        Err(error) => found.push(format!("a second sync failed: {error}")),
    }
    let live = pool.live().await.unwrap();
    for proxy in &live {
        let after = pool
            .record_probe(proxy.id, &answered(proxy, clock.now()))
            .await
            .unwrap();
        if after.is_none_or(|after| after.state != ProxyState::Healthy) {
            found.push("a new proxy whose probe answered is not healthy".to_owned());
        }
    }

    if let Some(gone) = world.take_first() {
        match pool.reconcile().await {
            Ok(_) => {
                let still = pool
                    .live()
                    .await
                    .unwrap()
                    .into_iter()
                    .any(|proxy| proxy.host == gone);
                if still {
                    found.push("a proxy that left the list is still live".to_owned());
                }
            }
            Err(error) => found.push(format!("the sync after a proxy left failed: {error}")),
        }
    }

    let mut live = pool.live().await.unwrap();
    let Some(due) = live.pop() else {
        found.push("no proxy is left to replace".to_owned());
        return found;
    };
    make_due(&db, due.id, clock.now()).await;
    if capabilities.replacement {
        world.complete_replacements();
        let report = pool.tick().await.unwrap();
        if report.requested != [due.id] {
            found.push("a proxy due for replacement was not asked to be replaced".to_owned());
        }
        let mut retired = false;
        for _ in 0..6 {
            clock.advance(chrono::Duration::minutes(1));
            pool.tick().await.unwrap();
            if pool
                .get(due.id)
                .await
                .unwrap()
                .is_some_and(|proxy| proxy.state == ProxyState::Retired)
            {
                retired = true;
                break;
            }
        }
        if !retired {
            found.push("a replacement the provider finished did not retire the proxy".to_owned());
        }
        let Some(next) = pool.live().await.unwrap().pop() else {
            found.push("the replaced proxy's successor was not adopted".to_owned());
            return found;
        };
        world.fail_next_replacement();
        make_due(&db, next.id, clock.now()).await;
        if !pool.tick().await.unwrap().requested.contains(&next.id) {
            found
                .push("a second proxy due for replacement was not asked to be replaced".to_owned());
        }
        let mut back = false;
        for _ in 0..6 {
            clock.advance(chrono::Duration::minutes(1));
            let report = pool.tick().await.unwrap();
            if report.failed.contains(&next.id)
                && pool
                    .get(next.id)
                    .await
                    .unwrap()
                    .is_some_and(|proxy| proxy.state == ProxyState::Backoff)
            {
                back = true;
                break;
            }
        }
        if !back {
            found.push(
                "a replacement the provider failed did not put the proxy back in backoff"
                    .to_owned(),
            );
        }
    } else {
        match pool
            .request_replacement(due.id, ReplaceReason::Operator)
            .await
        {
            Ok(ReplacementOutcome::Unsupported) => {}
            other => found.push(format!(
                "a provider without replacement answered a request with {other:?}"
            )),
        }
        let report = pool.tick().await.unwrap();
        if !report.requested.is_empty() {
            found.push("a replacement was asked of a provider that offers none".to_owned());
        }
    }
    found
}

/// Where offered, the provider's own swap is in its log.
async fn replaced_log(world: &dyn World) -> Vec<String> {
    let provider = world.provider();
    world.list(2);
    let since = Utc::now() - chrono::Duration::hours(1);
    if !provider.capabilities().replaced_log {
        return match provider.replaced(since).await {
            Err(ProviderError::Unsupported { operation, .. })
                if operation == Operation::ReplacedLog.as_str() =>
            {
                Vec::new()
            }
            other => vec![format!(
                "a provider without a replaced log answered {other:?}"
            )],
        };
    }
    let Some(old) = world.replace_first_by_itself() else {
        return vec!["the provider claims a replaced log its world cannot fill".to_owned()];
    };
    match provider.replaced(since).await {
        Ok(log)
            if log
                .iter()
                .any(|entry: &ReplacedProxy| entry.old_host == old) =>
        {
            Vec::new()
        }
        Ok(_) => vec!["a swap the provider made by itself is missing from its log".to_owned()],
        Err(error) => vec![format!("the replaced log could not be read: {error}")],
    }
}

/// A 429 with `Retry-After` is waited out before the next request.
async fn rate_limits(world: &dyn World) -> Vec<String> {
    world.list(1);
    if !world.rate_limit_next(1) {
        // A provider without an API has no limit to keep.
        return Vec::new();
    }
    let started = Instant::now();
    let listed = world.provider().list().await;
    let waited = started.elapsed();
    let mut found = Vec::new();
    if let Err(error) = listed {
        found.push(format!("a list request after one 429 failed: {error}"));
    }
    if waited < Duration::from_millis(950) {
        found.push(format!(
            "the next request went out {} ms after a 429 that asked for 1 s",
            waited.as_millis()
        ));
    }
    found
}

/// Errors are typed: unsupported is said as such and only then.
async fn typed_errors(world: &dyn World) -> Vec<String> {
    let mut found = Vec::new();
    world.list(1);
    let provider = world.provider();
    let kind = provider.kind();
    let capabilities = provider.capabilities();
    let Some(first) = provider
        .list()
        .await
        .ok()
        .and_then(|items| items.into_iter().next())
    else {
        return vec!["nothing could be listed".to_owned()];
    };
    let proxy = as_proxy(kind, &first);
    let since = Utc::now() - chrono::Duration::days(1);
    let answers: Vec<(Operation, Result<(), ProviderError>)> = vec![
        (
            Operation::Replacement,
            provider
                .replace(&proxy, ReplaceSpec::default())
                .await
                .map(drop),
        ),
        (
            Operation::Replacement,
            provider
                .replace_status(&ReplaceTicket("1".to_owned()))
                .await
                .map(|_: ReplaceState| ()),
        ),
        (
            Operation::ReplacedLog,
            provider.replaced(since).await.map(drop),
        ),
        (Operation::Account, provider.account().await.map(drop)),
        (Operation::Stats, provider.stats(since).await.map(drop)),
    ];
    for (operation, answer) in answers {
        let unsupported = matches!(
            &answer,
            Err(ProviderError::Unsupported { provider, operation: named })
                if *provider == kind && *named == operation.as_str()
        );
        let said_unsupported = matches!(&answer, Err(ProviderError::Unsupported { .. }));
        let offered = capabilities.offers(operation);
        if !offered && !unsupported {
            found.push(format!(
                "{operation} is not offered and was answered {answer:?}, not Unsupported"
            ));
        }
        if offered && said_unsupported {
            found.push(format!(
                "{operation} is offered and was answered Unsupported"
            ));
        }
    }
    if world.refuse_key()
        && !matches!(
            provider.list().await,
            Err(ProviderError::Unauthorized { .. })
        )
    {
        found.push("a refused key was not answered Unauthorized".to_owned());
    }
    if let Some((unreadable, expected, name)) = world.unreadable() {
        match unreadable.list().await {
            Err(error) if expected(&error) => {}
            other => found.push(format!(
                "a source that cannot be read was answered {other:?}, not {name}"
            )),
        }
    }
    found
}

/// Every rule's violations, by rule.
async fn run(make: Make) -> BTreeMap<&'static str, Vec<String>> {
    let mut results = BTreeMap::new();
    results.insert("listing", listing(make().await.as_ref()).await);
    results.insert("credentials", credentials(make().await.as_ref()).await);
    results.insert("state_machine", state_machine(make().await.as_ref()).await);
    results.insert("replaced_log", replaced_log(make().await.as_ref()).await);
    results.insert("rate_limits", rate_limits(make().await.as_ref()).await);
    results.insert("typed_errors", typed_errors(make().await.as_ref()).await);
    results
}

async fn assert_conforms(make: Make) {
    let results = run(make).await;
    let broken: BTreeMap<_, _> = results
        .into_iter()
        .filter(|(_, found)| !found.is_empty())
        .collect();
    assert!(broken.is_empty(), "the provider breaks rules: {broken:#?}");
}

// --- Subjects --------------------------------------------------------------------

/// The Webshare provider against the mock of its API.
struct WebshareWorld {
    mock: MockWebshare,
    provider: Arc<WebshareProvider>,
}

impl WebshareWorld {
    fn config(base_url: String) -> WebshareConfig {
        WebshareConfig {
            api_key: Some(Secret::new(WEBSHARE_TOKEN)),
            base_url,
            timeout: HumanDuration::from_secs(5),
            rate_limit_backoff: HumanDuration::new(Duration::from_millis(20)),
            // The mock is local; the buckets are tested in their own module.
            requests_per_minute: 60_000,
            list_requests_per_minute: 60_000,
            max_retries: 2,
            ..WebshareConfig::default()
        }
    }

    async fn start() -> Box<dyn World> {
        let mock = MockWebshare::start().await;
        let provider = Arc::new(WebshareProvider::new(&Self::config(mock.base_url())).unwrap());
        Box::new(Self { mock, provider })
    }
}

impl World for WebshareWorld {
    fn provider(&self) -> Arc<dyn ProxyProvider> {
        self.provider.clone()
    }

    fn list(&self, count: u32) {
        self.mock.with(|state| {
            state.add_proxies(count, "DE");
        });
    }

    fn take_first(&self) -> Option<String> {
        self.mock
            .with(|state| (!state.proxies.is_empty()).then(|| state.proxies.remove(0).ip))
    }

    fn replace_first_by_itself(&self) -> Option<String> {
        self.mock.with(|state| {
            let old = state.proxies.first()?.ip.clone();
            let now = state.now;
            state.auto_replace(&old, "auto_invalidated", now);
            Some(old)
        })
    }

    fn complete_replacements(&self) {
        self.mock.with(|state| state.polls_to_complete = 0);
    }

    fn fail_next_replacement(&self) {
        self.mock.with(|state| {
            state.polls_to_complete = 0;
            state.fail_replacement = Some((
                "replacement_failed".to_owned(),
                "no proxy was found".to_owned(),
            ));
        });
    }

    fn rate_limit_next(&self, seconds: u64) -> bool {
        self.mock
            .rate_limit("/api/v2/proxy/list/", Some(seconds), 1);
        true
    }

    fn refuse_key(&self) -> bool {
        self.mock.inject(
            None,
            "/api/v2/proxy/list/",
            401,
            json!({ "detail": "Invalid token." }),
            1,
        );
        true
    }

    fn unreadable(&self) -> Option<Unreadable> {
        let dead =
            WebshareProvider::new(&Self::config("http://127.0.0.1:9/api/".to_owned())).ok()?;
        Some((
            Arc::new(dead),
            |error| matches!(error, ProviderError::Transport(_)),
            "Transport",
        ))
    }

    fn secrets(&self) -> Vec<String> {
        vec![
            WEBSHARE_TOKEN.to_owned(),
            PROXY_PASS.to_owned(),
            PROXY_USER.to_owned(),
        ]
    }
}

/// The declared list over a file, read again at every listing.
struct DeclaredWorld {
    _dir: tempfile::TempDir,
    file: PathBuf,
    lines: Mutex<Vec<(String, String)>>,
    next: AtomicU32,
    provider: Arc<StaticListProvider>,
}

impl DeclaredWorld {
    fn start() -> Box<dyn World> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proxies.txt");
        std::fs::write(&file, "# the operator's list\n").unwrap();
        let provider = StaticListProvider::from_config(&ProxiesConfig {
            static_list_file: Some(file.clone()),
            ..ProxiesConfig::default()
        })
        .unwrap();
        Box::new(Self {
            _dir: dir,
            file,
            lines: Mutex::new(Vec::new()),
            next: AtomicU32::new(1),
            provider: Arc::new(provider),
        })
    }

    fn write(&self) {
        let lines = self.lines.lock().unwrap();
        let mut text = String::from("# the operator's list\n");
        for (_, line) in lines.iter() {
            text.push_str(line);
            text.push('\n');
        }
        std::fs::File::create(&self.file)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .unwrap();
    }
}

impl World for DeclaredWorld {
    fn provider(&self) -> Arc<dyn ProxyProvider> {
        self.provider.clone()
    }

    fn list(&self, count: u32) {
        for _ in 0..count {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let host = format!("198.51.100.{n}");
            let line = format!("{host}:8080:user-decl-{n}:pass-decl-{n}:AT:de-AT");
            self.lines.lock().unwrap().push((host, line));
        }
        self.write();
    }

    fn take_first(&self) -> Option<String> {
        let gone = {
            let mut lines = self.lines.lock().unwrap();
            (!lines.is_empty()).then(|| lines.remove(0).0)
        };
        self.write();
        gone
    }

    fn replace_first_by_itself(&self) -> Option<String> {
        None
    }

    fn complete_replacements(&self) {}

    fn fail_next_replacement(&self) {}

    fn rate_limit_next(&self, _seconds: u64) -> bool {
        false
    }

    fn refuse_key(&self) -> bool {
        false
    }

    fn unreadable(&self) -> Option<Unreadable> {
        let missing = StaticListProvider::from_config(&ProxiesConfig {
            static_list_file: Some(self.file.with_extension("missing")),
            ..ProxiesConfig::default()
        })
        .ok()?;
        Some((
            Arc::new(missing),
            |error| matches!(error, ProviderError::InvalidList(_)),
            "InvalidList",
        ))
    }

    fn secrets(&self) -> Vec<String> {
        let last = self.next.load(Ordering::SeqCst);
        (1..last)
            .flat_map(|n| [format!("user-decl-{n}"), format!("pass-decl-{n}")])
            .collect()
    }
}

/// What the broken provider's "API" holds.
#[derive(Debug, Default)]
struct BrokenState {
    lines: Vec<String>,
    throttle: u32,
    refuse: u32,
}

struct BrokenResponder(Arc<Mutex<BrokenState>>);

impl Respond for BrokenResponder {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap();
        if state.refuse > 0 {
            state.refuse -= 1;
            return ResponseTemplate::new(401);
        }
        if state.throttle > 0 {
            state.throttle -= 1;
            return ResponseTemplate::new(429).insert_header("retry-after", "1");
        }
        ResponseTemplate::new(200).set_body_string(state.lines.join("\n"))
    }
}

/// The secret the broken provider holds for its API.
const BROKEN_KEY: &str = "broken-api-key-0b7e";

/// A provider written to break every rule once: it claims what it cannot
/// do, lists one proxy twice under references that change, logs and
/// prints its key, retries a 429 at once, and calls every failure a decode
/// error.
struct BrokenProvider {
    client: wreq::Client,
    url: String,
    key: String,
    calls: AtomicU32,
}

impl fmt::Debug for BrokenProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BrokenProvider {{ url: {}, key: {} }}",
            self.url, self.key
        )
    }
}

#[async_trait]
impl ProxyProvider for BrokenProvider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::Static
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            replacement: true,
            replaced_log: true,
            ..Capabilities::LIST_ONLY
        }
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        tracing::info!(key = %self.key, "listing");
        let mut tries = 0;
        let text = loop {
            let response = self
                .client
                .get(&self.url)
                .send()
                .await
                .map_err(|error| ProviderError::Decode(error.to_string()))?;
            match response.status().as_u16() {
                429 if tries < 5 => tries += 1,
                200 => {
                    break response
                        .text()
                        .await
                        .map_err(|error| ProviderError::Decode(error.to_string()))?;
                }
                status => return Err(ProviderError::Decode(format!("HTTP {status}"))),
            }
        };
        let mut items: Vec<ProviderProxy> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(':');
                let host = fields.next()?.to_owned();
                let port = fields.next()?.parse().ok()?;
                Some(ProviderProxy {
                    provider_ref: format!("{host}#{call}"),
                    host,
                    port,
                    username: Secret::new(fields.next()?),
                    password: Secret::new(fields.next()?),
                    country: None,
                    locale: None,
                    timezone: None,
                    city: None,
                    asn: None,
                    valid: None,
                })
            })
            .collect();
        if let Some(first) = items.first().cloned() {
            items.push(first);
        }
        Ok(items)
    }

    async fn replaced(&self, _since: DateTime<Utc>) -> Result<Vec<ReplacedProxy>, ProviderError> {
        Ok(Vec::new())
    }
}

/// The broken provider and its "API".
struct BrokenWorld {
    _server: MockServer,
    state: Arc<Mutex<BrokenState>>,
    provider: Arc<BrokenProvider>,
}

impl BrokenWorld {
    async fn start() -> Box<dyn World> {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(BrokenState::default()));
        Mock::given(any())
            .respond_with(BrokenResponder(Arc::clone(&state)))
            .mount(&server)
            .await;
        let provider = Arc::new(BrokenProvider {
            client: wreq::Client::builder().no_proxy().build().unwrap(),
            url: format!("{}/list", server.uri()),
            key: BROKEN_KEY.to_owned(),
            calls: AtomicU32::new(0),
        });
        Box::new(Self {
            _server: server,
            state,
            provider,
        })
    }
}

impl World for BrokenWorld {
    fn provider(&self) -> Arc<dyn ProxyProvider> {
        self.provider.clone()
    }

    fn list(&self, count: u32) {
        let mut state = self.state.lock().unwrap();
        for _ in 0..count {
            let n = state.lines.len() + 1;
            state.lines.push(format!(
                "198.51.100.{n}:8080:user-broken-{n}:pass-broken-{n}"
            ));
        }
    }

    fn take_first(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        (!state.lines.is_empty()).then(|| {
            let line = state.lines.remove(0);
            line.split(':').next().unwrap_or_default().to_owned()
        })
    }

    fn replace_first_by_itself(&self) -> Option<String> {
        let old = self.take_first()?;
        self.list(1);
        Some(old)
    }

    fn complete_replacements(&self) {}

    fn fail_next_replacement(&self) {}

    fn rate_limit_next(&self, _seconds: u64) -> bool {
        self.state.lock().unwrap().throttle = 1;
        true
    }

    fn refuse_key(&self) -> bool {
        self.state.lock().unwrap().refuse = 1;
        true
    }

    fn unreadable(&self) -> Option<Unreadable> {
        let dead = BrokenProvider {
            client: wreq::Client::builder().no_proxy().build().ok()?,
            url: "http://127.0.0.1:9/list".to_owned(),
            key: BROKEN_KEY.to_owned(),
            calls: AtomicU32::new(0),
        };
        Some((
            Arc::new(dead),
            |error| matches!(error, ProviderError::Transport(_)),
            "Transport",
        ))
    }

    fn secrets(&self) -> Vec<String> {
        vec![BROKEN_KEY.to_owned()]
    }
}

// --- The tests ---------------------------------------------------------------------

/// Webshare's provider passes every rule against its mock.
#[tokio::test(flavor = "current_thread")]
async fn the_webshare_provider_conforms() {
    assert_conforms(|| Box::pin(WebshareWorld::start())).await;
}

/// The declared list passes every rule: it lists, re-reads its file, and
/// answers every other operation as unsupported.
#[tokio::test(flavor = "current_thread")]
async fn the_declared_list_conforms() {
    assert_conforms(|| Box::pin(std::future::ready(DeclaredWorld::start()))).await;
}

/// A provider that breaks each rule once is caught by each rule: the
/// suite fails what it is meant to fail.
#[tokio::test(flavor = "current_thread")]
async fn the_suite_finds_what_a_broken_provider_breaks() {
    let results = run(|| Box::pin(BrokenWorld::start())).await;

    for (rule, found) in &results {
        assert!(
            !found.is_empty(),
            "{rule} found nothing wrong: {results:#?}"
        );
    }
    let all = results
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "two listed proxies share one reference",
        "the references changed between two listings",
        "a log line holds a credential",
        "a Debug output or an error holds a credential",
        "a sync of an unchanged list",
        "a swap the provider made by itself is missing from its log",
        "the next request went out",
        "replacement is offered and was answered Unsupported",
        "a refused key was not answered Unauthorized",
        "not Transport",
    ] {
        assert!(all.contains(expected), "{expected}: {results:#?}");
    }
}
