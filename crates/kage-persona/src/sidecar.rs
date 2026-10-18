//! The two sidecars as the persona manager sees them, and the conversions
//! between a stored persona and what the sidecar contract takes.

use kage_browser::contract::{
    self, EngineName, OpenContextRequest, Proxy as ContextProxy, ScreenConstraints,
};
use kage_browser::{BrowserError, SidecarClient, SidecarSettings};
use kage_core::config::Config;
use kage_core::domain::{ClaimedOs, Engine, Geolocation, Persona, ProxyEndpoint, Screen};
use kage_core::persona::screen::ScreenClass;

use crate::PersonaError;

/// One client per engine's sidecar.
#[derive(Debug, Clone)]
pub struct Sidecars {
    chromium: SidecarClient,
    firefox: SidecarClient,
}

impl Sidecars {
    /// The sidecars the configuration names.
    ///
    /// # Errors
    ///
    /// Fails when a client cannot be built from its URL.
    pub fn from_config(config: &Config) -> Result<Self, BrowserError> {
        Ok(Self {
            chromium: SidecarClient::new(&SidecarSettings::from_config(config, Engine::Chromium))?,
            firefox: SidecarClient::new(&SidecarSettings::from_config(config, Engine::Firefox))?,
        })
    }

    /// The client of an engine's sidecar.
    pub const fn of(&self, engine: Engine) -> &SidecarClient {
        match engine {
            Engine::Chromium => &self.chromium,
            Engine::Firefox => &self.firefox,
        }
    }
}

/// A claimed system as the contract writes it.
pub(crate) const fn contract_os(os: ClaimedOs) -> contract::ClaimedOs {
    match os {
        ClaimedOs::Windows => contract::ClaimedOs::Windows,
        ClaimedOs::Macos => contract::ClaimedOs::Macos,
        ClaimedOs::Linux => contract::ClaimedOs::Linux,
    }
}

/// A claimed system of the contract as the domain writes it.
pub(crate) const fn domain_os(os: contract::ClaimedOs) -> ClaimedOs {
    match os {
        contract::ClaimedOs::Windows => ClaimedOs::Windows,
        contract::ClaimedOs::Macos => ClaimedOs::Macos,
        contract::ClaimedOs::Linux => ClaimedOs::Linux,
    }
}

/// An engine of the contract as the domain writes it.
pub(crate) const fn domain_engine(engine: EngineName) -> Engine {
    match engine {
        EngineName::Chromium => Engine::Chromium,
        EngineName::Firefox => Engine::Firefox,
    }
}

/// A screen of the contract as the domain stores it; `None` when a number
/// does not fit, which a screen never does not.
pub(crate) fn domain_screen(screen: &contract::Screen) -> Option<Screen> {
    // A device pixel ratio is a small number; f32 holds it exactly enough.
    #[allow(clippy::cast_possible_truncation)]
    let dpr = screen.dpr as f32;
    Some(Screen {
        width: u32::try_from(screen.width).ok()?,
        height: u32::try_from(screen.height).ok()?,
        dpr,
        color_depth: u8::try_from(screen.color_depth).ok()?,
    })
}

/// A stored screen as the contract takes it.
pub(crate) fn contract_screen(screen: Screen) -> contract::Screen {
    contract::Screen {
        color_depth: u64::from(screen.color_depth),
        dpr: f64::from(screen.dpr),
        height: u64::from(screen.height),
        width: u64::from(screen.width),
    }
}

/// The bounds that ask a generator for exactly one screen class.
pub(crate) fn exact_bounds(class: ScreenClass) -> ScreenConstraints {
    ScreenConstraints {
        max_height: Some(u64::from(class.height)),
        max_width: Some(u64::from(class.width)),
        min_height: Some(u64::from(class.height)),
        min_width: Some(u64::from(class.width)),
    }
}

/// Everything a persona's context is opened with: the persisted
/// fingerprint exactly as it was generated, the persona's place and
/// languages, its profile directory, and its own proxy.
///
/// # Errors
///
/// Fails when the stored fingerprint is not a JSON object.
pub(crate) fn open_request(
    persona: &Persona,
    endpoint: &ProxyEndpoint,
) -> Result<OpenContextRequest, PersonaError> {
    let fingerprint = persona
        .fingerprint
        .as_object()
        .cloned()
        .ok_or(PersonaError::Fingerprint(persona.id))?;
    Ok(OpenContextRequest {
        fingerprint,
        geolocation: persona
            .geolocation
            .map(|position: Geolocation| contract::Geolocation {
                accuracy_m: position.accuracy_m,
                latitude: position.latitude,
                longitude: position.longitude,
            }),
        languages: persona.languages.clone(),
        locale: persona.locale.clone(),
        persona_id: persona.id.as_uuid(),
        profile_dir: persona.profile_dir.clone(),
        proxy: ContextProxy {
            password: endpoint.password.expose().to_owned(),
            server: endpoint.url(),
            username: endpoint.username.expose().to_owned(),
        },
        screen: contract_screen(persona.screen),
        timezone: persona.timezone.clone(),
    })
}

#[cfg(test)]
mod tests {
    use kage_browser::contract;
    use kage_core::domain::{ClaimedOs, Engine};
    use kage_core::persona::screen::ScreenClass;

    use super::{
        contract_os, contract_screen, domain_engine, domain_os, domain_screen, exact_bounds,
        open_request,
    };
    use crate::PersonaError;
    use crate::test_support::{endpoint, persona};

    #[test]
    fn systems_engines_and_screens_convert_both_ways() {
        for os in ClaimedOs::ALL {
            assert_eq!(domain_os(contract_os(*os)), *os);
        }
        assert_eq!(
            domain_engine(contract::EngineName::Chromium),
            Engine::Chromium
        );
        assert_eq!(
            domain_engine(contract::EngineName::Firefox),
            Engine::Firefox
        );

        let stored = persona(Engine::Firefox, 156).screen;
        let sent = contract_screen(stored);
        assert_eq!(
            (sent.width, sent.height, sent.color_depth),
            (1920, 1080, 24)
        );
        assert_eq!(domain_screen(&sent), Some(stored));
        let impossible = contract::Screen {
            width: u64::MAX,
            ..sent
        };
        assert_eq!(domain_screen(&impossible), None);
    }

    #[test]
    fn a_screen_class_is_asked_for_by_exact_bounds() {
        let bounds = exact_bounds(ScreenClass {
            width: 2560,
            height: 1440,
            weight: 18,
        });
        assert_eq!(
            (bounds.min_width, bounds.max_width),
            (Some(2560), Some(2560))
        );
        assert_eq!(
            (bounds.min_height, bounds.max_height),
            (Some(1440), Some(1440))
        );
    }

    #[test]
    fn a_context_is_opened_with_the_persona_as_it_is_stored() {
        let persona = persona(Engine::Firefox, 156);
        let endpoint = endpoint(persona.proxy_id);

        let request = open_request(&persona, &endpoint).unwrap();

        assert_eq!(request.persona_id, persona.id.as_uuid());
        assert_eq!(
            serde_json::Value::Object(request.fingerprint),
            persona.fingerprint
        );
        assert_eq!(request.locale, "de-DE");
        assert_eq!(request.languages, ["de", "en-US", "en"]);
        assert_eq!(request.timezone, "Europe/Berlin");
        assert_eq!(request.profile_dir, persona.profile_dir);
        assert_eq!(request.proxy.server, "http://127.0.0.1:8080");
        assert_eq!(request.proxy.username, "user");
        assert_eq!(request.screen.width, 1920);
        let position = request.geolocation.unwrap();
        assert!((position.latitude - 52.52).abs() < 1e-9);

        // A fingerprint that is no document is refused before any call.
        let mut broken = persona.clone();
        broken.fingerprint = serde_json::json!("not a document");
        assert!(matches!(
            open_request(&broken, &endpoint),
            Err(PersonaError::Fingerprint(id)) if id == persona.id
        ));
    }
}
