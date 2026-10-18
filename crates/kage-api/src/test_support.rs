//! Fixtures for this crate's unit tests.

use chrono::Utc;
use kage_core::domain::{ClaimedOs, Engine, Geolocation, Persona, PersonaState, Screen};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::persona;
use kage_persona::identity::profile_for;

/// A ready Berlin persona of `engine` at `major`: chromium on Linux,
/// firefox on Windows, with the http profile its major resolves to (none
/// for the majors of this release: Chrome 155, Firefox 156).
pub(crate) fn persona(engine: Engine, major: u32) -> Persona {
    let id = PersonaId::new();
    let (claimed_os, user_agent, languages) = match engine {
        Engine::Chromium => (
            ClaimedOs::Linux,
            format!(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{major}.0.0.0 Safari/537.36"
            ),
            vec!["de-DE", "de", "en-US", "en"],
        ),
        Engine::Firefox => (
            ClaimedOs::Windows,
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{major}.0) Gecko/20100101 \
                 Firefox/{major}.0"
            ),
            vec!["de", "en-US", "en"],
        ),
    };
    Persona {
        id,
        proxy_id: ProxyId::new(),
        engine,
        claimed_os,
        browser_major: major,
        seed: persona::seed_for(id),
        fingerprint: serde_json::json!({ "schema": 1, "engine": engine.as_str() }),
        user_agent,
        hardware_concurrency: 8,
        wreq_profile: profile_for(engine, major, claimed_os),
        country: "DE".parse().ok(),
        locale: "de-DE".to_owned(),
        languages: languages.into_iter().map(str::to_owned).collect(),
        timezone: "Europe/Berlin".to_owned(),
        geolocation: Some(Geolocation {
            latitude: 52.52,
            longitude: 13.405,
            accuracy_m: 35.0,
        }),
        screen: Screen {
            width: 1920,
            height: 1080,
            dpr: 1.0,
            color_depth: 24,
        },
        tables_version: persona::TABLES_VERSION.to_owned(),
        profile_dir: persona::profile_dir(engine, id),
        state: PersonaState::Active,
        cooling_reason: None,
        warmup_visits: 3,
        warmed_at: Some(Utc::now()),
        created_at: Utc::now(),
        retired_at: None,
        retire_reason: None,
        profile_archived_at: None,
        profile_archive: None,
        profile_deleted_at: None,
    }
}
