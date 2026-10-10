//! The client factory (spec §9): one wreq client per persona, emulation
//! profile and proxy, with that persona's cookie jar.
//!
//! A [`ClientSpec`] says who the client is (the persona, its profile, its
//! languages and its own identity values); a
//! [`kage_core::domain::ProxyEndpoint`] says where it leaves from. The
//! factory keeps one [`HttpClient`] per pair, so a persona's requests share
//! their connections, their TLS sessions and their cookies, as one browser's
//! do. A spec that names no persona (a probe, the self-test of a profile)
//! is keyed by its proxy alone.
//!
//! Every client the factory builds
//!
//! - presents the TLS and HTTP/2 fingerprint of its wreq-util profile;
//! - sends every request through its proxy and nowhere else: setting a proxy
//!   also switches off the proxies named in the environment, and there is no
//!   direct fallback (decision D17);
//! - never follows a redirect by itself: [`HttpClient::fetch`] does, one hop
//!   at a time, so that each hop passes the SSRF guard and gets its own
//!   headers and cookies;
//! - has no default headers: [`crate::headers`] builds the whole list for
//!   each request.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use kage_core::config::{HttpConfig, SubresourceScope};
use kage_core::domain::{ClaimedOs, ProxyEndpoint};
use kage_core::ids::{PersonaId, ProxyId};
use tokio::sync::Mutex;

use crate::cookies::{CookieBackend, CookieJar, JarOwner, MemoryCookies};
use crate::error::HttpError;
use crate::headers::{DEFAULT_LANGUAGES, Identity, IdentityOverride};
use crate::profile::ProfileChoice;
use crate::subresources::AssetCache;
use crate::timing::{ConnectTimingLayer, TimedSessionCache};

/// What subresource emulation may fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubresourceSettings {
    /// Which assets count as the page's own.
    pub scope: SubresourceScope,
    /// Most assets per page, the icon included.
    pub max: usize,
    /// Most assets in flight at once.
    pub max_concurrent: usize,
    /// An asset is read up to this many bytes.
    pub max_bytes_each: usize,
    /// All assets of one page together are read up to this many bytes.
    pub max_bytes_total: usize,
}

/// The limits and timeouts of the http path, from the `http` section of the
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpSettings {
    /// Longest one fetch may take: every hop and the body together.
    pub timeout: Duration,
    /// Longest the setup of one connection may take.
    pub connect_timeout: Duration,
    /// How long an unused connection is kept open.
    pub idle_connection_timeout: Duration,
    /// Redirect hops followed before a fetch fails.
    pub max_redirects: usize,
    /// A body is read up to this many bytes.
    pub max_body_bytes: usize,
    /// Session cookies are dropped when a jar was idle for this long.
    pub session_cookie_idle: Duration,
    /// Whether a fetch also fetches the page's own assets, unless the fetch
    /// says otherwise.
    pub fetch_subresources: bool,
    /// What subresource emulation may fetch.
    pub subresources: SubresourceSettings,
}

fn bytes(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl HttpSettings {
    /// The settings the configuration describes.
    pub fn from_config(config: &HttpConfig) -> Self {
        Self {
            timeout: config.timeout.get(),
            connect_timeout: config.connect_timeout.get(),
            idle_connection_timeout: config.idle_connection_timeout.get(),
            max_redirects: bytes(u64::from(config.max_redirects)),
            max_body_bytes: bytes(config.max_body_bytes),
            session_cookie_idle: config.session_cookie_idle.get(),
            fetch_subresources: config.fetch_subresources,
            subresources: SubresourceSettings {
                scope: config.subresources.scope,
                max: bytes(u64::from(config.subresources.max)),
                max_concurrent: bytes(u64::from(config.subresources.max_concurrent)),
                max_bytes_each: bytes(config.subresources.max_bytes_each),
                max_bytes_total: bytes(config.subresources.max_bytes_total),
            },
        }
    }
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self::from_config(&HttpConfig::default())
    }
}

/// Who a client is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    /// The emulation profile and the operating system it claims.
    pub profile: ProfileChoice,
    /// The languages the client prefers, most preferred first. They become
    /// `Accept-Language` in the browser family's own format.
    pub languages: Vec<String>,
    /// The persona's own identity values, where they replace the profile's.
    pub identity: IdentityOverride,
    /// The persona the client is, when it is one: its cookie jar is its
    /// own (spec §6.4).
    pub persona: Option<PersonaId>,
}

impl ClientSpec {
    /// A client of this profile that speaks English (United States), the
    /// way both browsers do out of the box.
    pub fn new(profile: ProfileChoice) -> Self {
        Self {
            profile,
            languages: DEFAULT_LANGUAGES
                .iter()
                .map(|&tag| tag.to_owned())
                .collect(),
            identity: IdentityOverride::default(),
            persona: None,
        }
    }

    /// The same client as a persona: it keeps that persona's cookies and
    /// no one else's.
    #[must_use]
    pub const fn for_persona(mut self, persona: PersonaId) -> Self {
        self.persona = Some(persona);
        self
    }

    /// The same client speaking other languages, most preferred first.
    #[must_use]
    pub fn with_languages<L: Into<String>>(
        mut self,
        languages: impl IntoIterator<Item = L>,
    ) -> Self {
        self.languages = languages.into_iter().map(Into::into).collect();
        self
    }

    /// The same client with the persona's own identity values.
    #[must_use]
    pub fn with_identity(mut self, identity: IdentityOverride) -> Self {
        self.identity = identity;
        self
    }
}

/// Resolves host names for the address check of the SSRF guard (spec §19).
///
/// On the http path the proxy resolves every name itself; a lookup here is
/// an extra one, made from the Kage host. None is made unless a resolver is
/// given to the factory: a lookup from the host shows the operator's
/// resolver to the target's name servers, which a per-session host name
/// turns into a link between the operator's network and the request. Which
/// resolver, if any, serves fetches is decided where the guard is wired to
/// the API (milestone M8).
#[async_trait]
pub trait HopResolver: Send + Sync + fmt::Debug {
    /// The addresses `host` resolves to.
    ///
    /// # Errors
    ///
    /// A description of why the name could not be resolved.
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    proxy: ProxyId,
    persona: Option<PersonaId>,
    profile: String,
    os: ClaimedOs,
}

/// Whether two endpoints are the same place with the same login.
fn same_endpoint(left: &ProxyEndpoint, right: &ProxyEndpoint) -> bool {
    left.id == right.id
        && left.host == right.host
        && left.port == right.port
        && left.scheme == right.scheme
        && left.username.expose() == right.username.expose()
        && left.password.expose() == right.password.expose()
}

struct Cached {
    client: Arc<HttpClient>,
    spec: ClientSpec,
    endpoint: ProxyEndpoint,
}

/// Builds and keeps the clients of the http path.
pub struct ClientFactory {
    settings: HttpSettings,
    cookies: Arc<dyn CookieBackend>,
    resolver: Option<Arc<dyn HopResolver>>,
    trust_anchors: Vec<Vec<u8>>,
    clients: Mutex<HashMap<ClientKey, Cached>>,
}

impl fmt::Debug for ClientFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientFactory")
            .field("settings", &self.settings)
            .field("cookies", &self.cookies)
            .field("resolver", &self.resolver)
            .field("trust_anchors", &self.trust_anchors.len())
            .finish_non_exhaustive()
    }
}

impl ClientFactory {
    /// A factory whose clients keep their cookies in `cookies`.
    pub fn new(settings: HttpSettings, cookies: Arc<dyn CookieBackend>) -> Self {
        Self {
            settings,
            cookies,
            resolver: None,
            trust_anchors: Vec::new(),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// A factory whose clients keep their cookies in memory only.
    pub fn in_memory(settings: HttpSettings) -> Self {
        Self::new(settings, Arc::new(MemoryCookies::new()))
    }

    /// Runs the address check of the SSRF guard on every hop with this
    /// resolver. See [`HopResolver`] for why there is none by default.
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<dyn HopResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Makes the clients trust a certificate authority in addition to the
    /// public ones: the one of the operator's own fingerprint echo. `pem`
    /// holds one or more certificates.
    #[must_use]
    pub fn with_trust_anchor(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.trust_anchors.push(pem.into());
        self
    }

    /// The limits and timeouts of the clients.
    pub const fn settings(&self) -> &HttpSettings {
        &self.settings
    }

    /// The client of `spec` that leaves through `proxy`. It is built on
    /// first use, with the proxy's stored cookies, and kept. When the
    /// proxy's address or login, or the client's identity, changed since
    /// then, the client is rebuilt and keeps its cookies.
    ///
    /// # Errors
    ///
    /// Fails when the identity is refused, the cookies cannot be loaded
    /// (the jar of a retired proxy never is), or wreq rejects the options.
    pub async fn client(
        &self,
        spec: &ClientSpec,
        proxy: &ProxyEndpoint,
    ) -> Result<Arc<HttpClient>, HttpError> {
        let owner = JarOwner {
            proxy: proxy.id,
            persona: spec.persona,
        };
        let key = ClientKey {
            proxy: proxy.id,
            persona: spec.persona,
            profile: spec.profile.name().to_owned(),
            os: spec.profile.os(),
        };
        let mut clients = self.clients.lock().await;
        let jar = match clients.get(&key) {
            Some(cached) if cached.spec == *spec && same_endpoint(&cached.endpoint, proxy) => {
                return Ok(Arc::clone(&cached.client));
            }
            // Same persona, new address or login: the cookies stay.
            Some(cached) => Arc::clone(&cached.client.jar),
            None => {
                let stored = self.cookies.load(owner).await?;
                Arc::new(CookieJar::from_stored(
                    stored,
                    Utc::now(),
                    self.settings.session_cookie_idle,
                ))
            }
        };
        let client = Arc::new(self.build(spec, proxy, jar, Some(Arc::clone(&self.cookies)))?);
        clients.insert(
            key,
            Cached {
                client: Arc::clone(&client),
                spec: spec.clone(),
                endpoint: proxy.clone(),
            },
        );
        Ok(client)
    }

    /// A client of `spec` through `proxy` that starts with no cookies,
    /// stores none and is not kept: for health probes and self-tests, which
    /// must open a fresh connection every time and leave no trace.
    ///
    /// # Errors
    ///
    /// Fails when the identity is refused or wreq rejects the options.
    pub fn ephemeral(
        &self,
        spec: &ClientSpec,
        proxy: &ProxyEndpoint,
    ) -> Result<HttpClient, HttpError> {
        self.build(spec, proxy, Arc::new(CookieJar::new()), None)
    }

    /// Forgets the clients of a proxy, closing their idle connections. For
    /// a proxy that was retired.
    pub async fn forget(&self, proxy: ProxyId) {
        self.clients
            .lock()
            .await
            .retain(|key, _| key.proxy != proxy);
    }

    /// Forgets the clients of a persona, closing their idle connections and
    /// dropping the cookies they hold in memory. For a persona that was
    /// retired while its proxy stays.
    pub async fn forget_persona(&self, persona: PersonaId) {
        self.clients
            .lock()
            .await
            .retain(|key, _| key.persona != Some(persona));
    }

    fn build(
        &self,
        spec: &ClientSpec,
        proxy: &ProxyEndpoint,
        jar: Arc<CookieJar>,
        backend: Option<Arc<dyn CookieBackend>>,
    ) -> Result<HttpClient, HttpError> {
        let identity = Identity::new(&spec.profile, &spec.languages, &spec.identity)?;
        // The address holds no credentials; they are added as a header.
        let through = wreq::Proxy::all(proxy.url())
            .map_err(|_| HttpError::Build("the proxy address is not valid".to_owned()))?
            .basic_auth(proxy.username.expose(), proxy.password.expose());
        let mut builder = wreq::Client::builder()
            .emulation(spec.profile.transport())
            // Setting a proxy also switches off every proxy named in the
            // environment; there is no direct fallback.
            .proxy(through)
            .redirect(wreq::redirect::Policy::none())
            .connect_timeout(self.settings.connect_timeout)
            .pool_idle_timeout(self.settings.idle_connection_timeout)
            .tls_session_cache(TimedSessionCache::default())
            .connector_layer(ConnectTimingLayer);
        if !self.trust_anchors.is_empty() {
            let mut store = wreq::tls::trust::CertStore::builder()
                .add_der_certs(webpki_root_certs::TLS_SERVER_ROOT_CERTS);
            for anchor in &self.trust_anchors {
                store = store.add_stack_pem_certs(anchor);
            }
            builder = builder.tls_cert_store(store.build().map_err(|_| {
                HttpError::Build("a trusted certificate authority is not valid PEM".to_owned())
            })?);
        }
        let wreq = builder
            .build()
            .map_err(|_| HttpError::Build("the HTTP client rejected its options".to_owned()))?;
        Ok(HttpClient {
            wreq,
            identity,
            profile: spec.profile.clone(),
            proxy: proxy.id,
            persona: spec.persona,
            jar,
            backend,
            settings: self.settings,
            resolver: self.resolver.clone(),
            assets: AssetCache::default(),
        })
    }
}

/// One client of the http path: a wreq client that presents as one browser
/// through one proxy, with its cookies.
pub struct HttpClient {
    pub(crate) wreq: wreq::Client,
    pub(crate) identity: Identity,
    pub(crate) profile: ProfileChoice,
    pub(crate) proxy: ProxyId,
    pub(crate) persona: Option<PersonaId>,
    pub(crate) jar: Arc<CookieJar>,
    pub(crate) backend: Option<Arc<dyn CookieBackend>>,
    pub(crate) settings: HttpSettings,
    pub(crate) resolver: Option<Arc<dyn HopResolver>>,
    pub(crate) assets: AssetCache,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field("profile", &self.profile)
            .field("proxy", &self.proxy)
            .field("jar", &self.jar)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// The emulation profile.
    pub const fn profile(&self) -> &ProfileChoice {
        &self.profile
    }

    /// The identity behind every request.
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The proxy every request leaves through.
    pub const fn proxy(&self) -> ProxyId {
        self.proxy
    }

    /// The persona the client is, when it is one.
    pub const fn persona(&self) -> Option<PersonaId> {
        self.persona
    }

    /// The client's cookies.
    pub fn jar(&self) -> &CookieJar {
        &self.jar
    }

    /// The limits and timeouts of the client.
    pub const fn settings(&self) -> &HttpSettings {
        &self.settings
    }

    /// Saves the cookies when they changed and the client keeps any.
    ///
    /// A failure is logged and the jar stays marked as changed, so the next
    /// fetch tries again: losing a save must not fail a fetch that worked.
    pub async fn persist(&self) {
        let Some(backend) = &self.backend else {
            return;
        };
        if !self.jar.take_dirty() {
            return;
        }
        let cookies = self.jar.snapshot(Utc::now());
        let owner = JarOwner {
            proxy: self.proxy,
            persona: self.persona,
        };
        if let Err(error) = backend.save(owner, &cookies).await {
            self.jar.mark_dirty();
            tracing::warn!(proxy = %self.proxy, %error, "the cookie jar could not be saved");
        }
    }
}
