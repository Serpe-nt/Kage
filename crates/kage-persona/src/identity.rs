//! A persona on the http path (spec §6.1 step 4, §6.4, §9): which wreq
//! profile it presents, and the client that presents it.
//!
//! The browser is who a persona is; the http path may only speak for it
//! when it can be the same browser on the wire. Two rules decide that:
//!
//! - **At generation** the persona's engine, browser major and claimed
//!   system are mapped to a wreq-util profile
//!   ([`kage_http::profile::resolve`]): the profile of the same major, or
//!   the newest one presenting as a newer major that a capture verified.
//!   Without one the persona is browser-only and `wreq_profile` is null.
//! - **Whenever the http path is used**, and whenever cookies move between
//!   the two paths, the stored profile must still be the one the persona's
//!   engine major resolves to. A persona whose browser moved to another
//!   major than its profile presents keeps its cookies in the browser.
//!
//! The client is built with the persona's own `User-Agent` and, for
//! Chrome, the `sec-ch-ua` brand list of its real major, through the
//! identity override that callers of the API cannot reach, and with the
//! persona's languages; its cookie jar is the persona's own.

use std::collections::HashMap;
use std::fmt;
use std::sync::{PoisonError, RwLock};

use kage_core::domain::{ClaimedOs, Engine, Persona, ProxyEndpoint};
use kage_core::ids::ProxyId;
use kage_http::profile::{self, Family};
use kage_http::{ClientFactory, ClientSpec, HttpClient, HttpSettings, IdentityOverride};
use kage_proxy::health::{EmulatedClientFactory, ProbeClientError, ProbeClientFactory};

/// Why a persona has no http path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpPathOff {
    /// No emulation profile looks like the persona's browser on the wire:
    /// it is browser-only (spec risk R7).
    #[error("it has no emulation profile and is browser-only")]
    NoProfile,
    /// The stored profile is not one this build can present.
    #[error("its emulation profile {0:?} is not one this build knows")]
    UnknownProfile(String),
    /// The stored profile presents another browser than the persona's
    /// engine is, or another major than the engine now bundles.
    #[error(
        "its emulation profile {profile} does not present {engine} {browser_major}, the browser \
         it runs"
    )]
    MajorMismatch {
        /// The stored profile.
        profile: String,
        /// The persona's engine.
        engine: Engine,
        /// The major of the persona's browser.
        browser_major: u32,
    },
}

/// The wreq profile of a new persona: the name to store, or `None` for a
/// browser-only persona.
pub fn profile_for(engine: Engine, browser_major: u32, os: ClaimedOs) -> Option<String> {
    profile::resolve(engine, browser_major, os).map(|resolved| resolved.choice.name().to_owned())
}

/// The client of a persona on the http path: its profile, its languages,
/// its exact `User-Agent` and brand list, and its own cookie jar.
///
/// # Errors
///
/// [`HttpPathOff`] when the persona has no profile, or when the profile it
/// has is not what its browser major resolves to (the profile-major rule of
/// spec §6.4).
pub fn http_spec(persona: &Persona) -> Result<ClientSpec, HttpPathOff> {
    let name = persona
        .wreq_profile
        .as_deref()
        .ok_or(HttpPathOff::NoProfile)?;
    let mismatch = || HttpPathOff::MajorMismatch {
        profile: name.to_owned(),
        engine: persona.engine,
        browser_major: persona.browser_major,
    };
    let (family, _) = profile::family_and_major(name)
        .ok_or_else(|| HttpPathOff::UnknownProfile(name.to_owned()))?;
    if family != Family::of_engine(persona.engine) {
        return Err(mismatch());
    }
    let resolved = profile::resolve(persona.engine, persona.browser_major, persona.claimed_os)
        .filter(|resolved| resolved.choice.name() == name)
        .ok_or_else(mismatch)?;
    let client_hint_brands = match family {
        Family::Chrome => Some(profile::chrome_brands(persona.browser_major)),
        Family::Firefox => None,
    };
    Ok(ClientSpec::new(resolved.choice)
        .with_languages(persona.languages.iter().cloned())
        .with_identity(IdentityOverride {
            user_agent: Some(persona.user_agent.clone()),
            client_hint_brands,
        })
        .for_persona(persona.id))
}

/// Which persona lives behind which proxy, for the one caller that cannot
/// ask the store: the health probe, which builds its client synchronously
/// (`kage_proxy::health::ProbeClientFactory`).
///
/// The persona manager writes it whenever a persona is generated, changed
/// or retired. It holds only personas that have an http path.
#[derive(Default)]
pub struct Directory {
    specs: RwLock<HashMap<ProxyId, ClientSpec>>,
}

impl fmt::Debug for Directory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Directory")
            .field("personas", &self.len())
            .finish()
    }
}

impl Directory {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the persona behind its proxy, or forgets the proxy when the
    /// persona is retired or has no http path.
    pub fn record(&self, persona: &Persona) {
        let spec = persona.is_live().then(|| http_spec(persona).ok()).flatten();
        let mut specs = self.specs.write().unwrap_or_else(PoisonError::into_inner);
        match spec {
            Some(spec) => {
                specs.insert(persona.proxy_id, spec);
            }
            None => {
                specs.remove(&persona.proxy_id);
            }
        }
    }

    /// Forgets the persona behind a proxy.
    pub fn forget(&self, proxy: ProxyId) {
        self.specs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&proxy);
    }

    /// The http-path identity of the persona behind a proxy, if it has one.
    pub fn spec(&self, proxy: ProxyId) -> Option<ClientSpec> {
        self.specs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&proxy)
            .cloned()
    }

    /// How many personas with an http path are recorded.
    pub fn len(&self) -> usize {
        self.specs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no persona is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The probe client once personas exist (spec §7.4): a probe through a
/// proxy carries the TLS, HTTP/2 and header fingerprint of the persona that
/// lives behind it, so the provider's echo sees the same browser as that
/// persona's real traffic. A proxy without a persona, or with a
/// browser-only one, is probed with the configured profile
/// (`proxies.health.profile`) as before.
///
/// Every probe is a fresh client without cookies: a probe opens a new
/// connection each time and leaves no trace in the persona's jar.
#[derive(Debug)]
pub struct PersonaProbeClients {
    directory: std::sync::Arc<Directory>,
    factory: ClientFactory,
    fallback: EmulatedClientFactory,
}

impl PersonaProbeClients {
    /// Probe clients for the personas of `directory`, with `settings` as
    /// the limits of a probe and `fallback` for proxies without one.
    pub fn new(
        directory: std::sync::Arc<Directory>,
        settings: HttpSettings,
        fallback: EmulatedClientFactory,
    ) -> Self {
        Self {
            directory,
            factory: ClientFactory::in_memory(settings),
            fallback,
        }
    }
}

impl ProbeClientFactory for PersonaProbeClients {
    fn client(&self, target: &ProxyEndpoint) -> Result<HttpClient, ProbeClientError> {
        match self.directory.spec(target.id) {
            // No error of the http path quotes the proxy's credentials.
            Some(spec) => self
                .factory
                .ephemeral(&spec, target)
                .map_err(|error| ProbeClientError::Build(error.to_string())),
            None => self.fallback.client(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use kage_core::domain::{ClaimedOs, Engine, PersonaState};
    use kage_http::profile::ProfileChoice;
    use kage_http::{Family, HttpSettings};
    use kage_proxy::health::{EmulatedClientFactory, ProbeClientFactory};

    use super::{Directory, HttpPathOff, PersonaProbeClients, http_spec, profile_for};
    use crate::test_support::{endpoint, persona};

    #[test]
    fn a_new_persona_gets_the_profile_of_its_own_major_or_none() {
        assert_eq!(
            profile_for(Engine::Chromium, 149, ClaimedOs::Linux).as_deref(),
            Some("chrome_149")
        );
        assert_eq!(
            profile_for(Engine::Firefox, 151, ClaimedOs::Windows).as_deref(),
            Some("firefox_151")
        );
        // The engines of this release: a capture proved that no profile
        // fits, so their personas are browser-only.
        assert_eq!(profile_for(Engine::Chromium, 155, ClaimedOs::Linux), None);
        assert_eq!(profile_for(Engine::Firefox, 152, ClaimedOs::Windows), None);
        assert_eq!(profile_for(Engine::Firefox, 152, ClaimedOs::Macos), None);
    }

    #[test]
    fn the_http_client_is_the_persona_with_its_own_identity_and_jar() {
        let chrome = persona(Engine::Chromium, 149);
        let spec = http_spec(&chrome).unwrap();

        assert_eq!(spec.profile.name(), "chrome_149");
        assert_eq!(spec.profile.os(), ClaimedOs::Linux);
        assert_eq!(spec.profile.family(), Family::Chrome);
        assert_eq!(spec.languages, chrome.languages);
        assert_eq!(
            spec.identity.user_agent.as_deref(),
            Some(chrome.user_agent.as_str())
        );
        // The brand list is the one Chrome 149 computes for itself.
        let brands = spec.identity.client_hint_brands.unwrap();
        assert!(brands.contains("\"Google Chrome\";v=\"149\""), "{brands}");
        assert_eq!(spec.persona, Some(chrome.id));

        let firefox = persona(Engine::Firefox, 151);
        let spec = http_spec(&firefox).unwrap();
        assert_eq!(spec.profile.name(), "firefox_151");
        assert_eq!(spec.profile.os(), ClaimedOs::Windows);
        assert_eq!(spec.identity.client_hint_brands, None);
        assert_eq!(spec.languages, ["de", "en-US", "en"]);
    }

    #[test]
    fn the_http_path_is_off_when_the_profile_is_not_the_browsers_major() {
        // Browser-only: no profile.
        let mut browser_only = persona(Engine::Chromium, 155);
        browser_only.wreq_profile = None;
        assert_eq!(http_spec(&browser_only), Err(HttpPathOff::NoProfile));

        // The engine image moved to a major the profile does not present.
        let mut upgraded = persona(Engine::Chromium, 149);
        upgraded.browser_major = 155;
        let error = http_spec(&upgraded).unwrap_err();
        assert_eq!(
            error,
            HttpPathOff::MajorMismatch {
                profile: "chrome_149".to_owned(),
                engine: Engine::Chromium,
                browser_major: 155,
            }
        );
        assert!(error.to_string().contains("chromium 155"), "{error}");

        // An older profile under a newer mapped major is no match either.
        let mut stale = persona(Engine::Chromium, 149);
        stale.wreq_profile = Some("chrome_148".to_owned());
        assert!(matches!(
            http_spec(&stale),
            Err(HttpPathOff::MajorMismatch { .. })
        ));

        // A profile of the other browser, and one nobody ships.
        let mut crossed = persona(Engine::Chromium, 149);
        crossed.wreq_profile = Some("firefox_151".to_owned());
        assert!(matches!(
            http_spec(&crossed),
            Err(HttpPathOff::MajorMismatch { .. })
        ));
        let mut unknown = persona(Engine::Chromium, 149);
        unknown.wreq_profile = Some("edge_148".to_owned());
        assert_eq!(
            http_spec(&unknown),
            Err(HttpPathOff::UnknownProfile("edge_148".to_owned()))
        );
    }

    #[test]
    fn the_directory_holds_live_personas_that_have_an_http_path() {
        let directory = Directory::new();
        assert!(directory.is_empty());
        let chrome = persona(Engine::Chromium, 149);
        let mut browser_only = persona(Engine::Firefox, 152);
        browser_only.wreq_profile = None;

        directory.record(&chrome);
        directory.record(&browser_only);
        assert_eq!(directory.len(), 1);
        assert_eq!(
            directory.spec(chrome.proxy_id).unwrap().persona,
            Some(chrome.id)
        );
        assert_eq!(directory.spec(browser_only.proxy_id), None);
        assert!(format!("{directory:?}").contains("personas: 1"));

        // Retired, or forgotten with its proxy: gone.
        let mut retired = chrome.clone();
        retired.state = PersonaState::Retired;
        directory.record(&retired);
        assert!(directory.is_empty());
        directory.record(&chrome);
        directory.forget(chrome.proxy_id);
        assert!(directory.is_empty());
    }

    #[test]
    fn a_probe_presents_as_the_persona_behind_the_proxy() {
        let directory = Arc::new(Directory::new());
        let firefox = persona(Engine::Firefox, 151);
        directory.record(&firefox);
        let fallback = EmulatedClientFactory::new(
            ProfileChoice::named("chrome_149", ClaimedOs::Windows).unwrap(),
            Duration::from_secs(20),
        )
        .unwrap();
        let clients =
            PersonaProbeClients::new(Arc::clone(&directory), HttpSettings::default(), fallback);

        // Behind the persona's proxy: its own browser, without its jar.
        let own = clients.client(&endpoint(firefox.proxy_id)).unwrap();
        assert_eq!(own.profile().name(), "firefox_151");
        assert_eq!(
            own.identity().user_agent().to_str().unwrap(),
            firefox.user_agent
        );
        assert!(own.jar().is_empty());
        // Behind a proxy without a persona: the configured profile.
        let other = clients
            .client(&endpoint(kage_core::ids::ProxyId::new()))
            .unwrap();
        assert_eq!(other.profile().name(), "chrome_149");
    }
}
