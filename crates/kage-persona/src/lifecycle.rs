//! What happens to a persona during its life, as pure functions over a
//! [`Persona`] (spec §6.3). The manager calls them inside the store's
//! read-modify-write, so what they decide is what is stored.
//!
//! | Event | Effect |
//! |---|---|
//! | its proxy enters `backoff` or is being replaced | `cooling`, reason `proxy_backoff` |
//! | its proxy is usable again | `active`, unless it cools for another reason |
//! | a self-test fails | `cooling`, reason `selftest_failed` |
//! | its address moves to another country | `cooling`, reason `geo_changed` |
//! | a self-test passes | a persona that cools for a self-test's sake is `active` again |
//! | its proxy is retired, or an operator says so | `retired` |
//!
//! A reason that only a self-test ends is never replaced by the weaker
//! `proxy_backoff`: a persona whose self-test failed does not come back
//! just because its proxy did.

use chrono::{DateTime, Utc};
use kage_core::domain::{
    CoolingReason, CountryCode, Declaration, Engine, Geolocation, Persona, PersonaState, Proxy,
    ProxyState,
};
use kage_core::persona::locale::{self, PersonaLocale};
use kage_core::persona::place::{self, ZoneSource};
use kage_core::seed::Seed;

/// A position farther than this from the persona's own means the address
/// is in another place, not that a lookup moved a city's centre a little.
const SAME_PLACE_KM: f64 = 10.0;

/// A change of state: from, to.
pub type Transition = (PersonaState, PersonaState);

fn set(persona: &mut Persona, state: PersonaState, reason: Option<CoolingReason>) -> Transition {
    let from = persona.state;
    persona.state = state;
    persona.cooling_reason = reason;
    (from, state)
}

/// Brings a persona in step with the state of its proxy. Returns the
/// transition when the persona's state changed.
pub fn follow_proxy(persona: &mut Persona, proxy: ProxyState) -> Option<Transition> {
    match (persona.state, persona.cooling_reason, proxy) {
        (PersonaState::Active, _, ProxyState::Backoff | ProxyState::Replacing) => Some(set(
            persona,
            PersonaState::Cooling,
            Some(CoolingReason::ProxyBackoff),
        )),
        (
            PersonaState::Cooling,
            Some(CoolingReason::ProxyBackoff),
            ProxyState::Healthy | ProxyState::Degraded,
        ) => Some(set(persona, PersonaState::Active, None)),
        _ => None,
    }
}

/// Cools a persona for a reason only a self-test ends. Returns the
/// transition when the state changed; a persona that already cools keeps
/// cooling, now for this reason.
pub fn cool_until_selftest(persona: &mut Persona, reason: CoolingReason) -> Option<Transition> {
    match persona.state {
        PersonaState::Retired => None,
        PersonaState::Active => Some(set(persona, PersonaState::Cooling, Some(reason))),
        PersonaState::Cooling => {
            // The stronger reason replaces `proxy_backoff`; one that is
            // already there stays.
            if persona.cooling_reason == Some(CoolingReason::ProxyBackoff) {
                persona.cooling_reason = Some(reason);
            }
            None
        }
    }
}

/// A self-test passed: a persona that was cooling for a self-test's sake
/// is active again when its proxy can carry traffic, and otherwise goes on
/// cooling for its proxy. Returns the transition when the state changed.
pub fn selftest_passed(persona: &mut Persona, proxy_usable: bool) -> Option<Transition> {
    if persona.state != PersonaState::Cooling
        || !persona
            .cooling_reason
            .is_some_and(CoolingReason::needs_selftest)
    {
        return None;
    }
    if proxy_usable {
        Some(set(persona, PersonaState::Active, None))
    } else {
        persona.cooling_reason = Some(CoolingReason::ProxyBackoff);
        None
    }
}

/// Retires a persona. Returns the transition, or `None` when it was
/// retired already.
pub fn retire(persona: &mut Persona, now: DateTime<Utc>, reason: &str) -> Option<Transition> {
    if persona.state == PersonaState::Retired {
        return None;
    }
    let transition = set(persona, PersonaState::Retired, None);
    persona.retired_at = Some(now);
    persona.retire_reason = Some(reason.to_owned());
    Some(transition)
}

/// Where a persona is and what it speaks, as its proxy's geo record says.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// The country of the address: observed, else the provider's.
    pub country: Option<CountryCode>,
    /// The IANA time zone.
    pub timezone: String,
    /// Where the time zone came from.
    pub zone_source: ZoneSource,
    /// The locale and the language list for the country and the engine.
    pub locale: PersonaLocale,
    /// The position near the lookup's coordinates; `None` without them.
    pub geolocation: Option<Geolocation>,
}

/// The place of a persona of `engine` with `seed` behind `proxy` (spec
/// §6.1 step 1): everything derives from the observed geo record, and the
/// provider's country is the fallback only while no lookup has succeeded.
pub fn place_of(proxy: &Proxy, engine: Engine, seed: &Seed) -> Place {
    let geo = proxy.observed_geo();
    let country = proxy.effective_country();
    let (timezone, zone_source) =
        place::timezone(geo.and_then(|geo| geo.timezone.as_deref()), country);
    Place {
        country,
        timezone,
        zone_source,
        locale: locale::resolve(country, geo.and_then(|geo| geo.region.as_deref()), engine),
        geolocation: place::position(
            geo.and_then(kage_core::domain::GeoRecord::coordinates),
            seed,
        ),
    }
}

/// The place of a persona of `engine` with `seed` behind `proxy`, by the
/// one rule (spec §6.1, ADR 0013): what the operator declared, when the
/// proxy carries a declaration ([`declared_place`]); else what its geo
/// record says ([`place_of`]). The persona manager and the stand-in of a
/// proxy's self-test both place by it.
pub fn place_for(proxy: &Proxy, engine: Engine, seed: &Seed) -> Place {
    match proxy.declaration() {
        Some(declaration) => declared_place(proxy, &declaration, engine, seed),
        None => place_of(proxy, engine, seed),
    }
}

/// The place of a persona of `engine` with `seed` behind a proxy whose
/// exit the operator declared (ADR 0013, one rule for a list's entry and
/// the host's own rows): the declared country; the declared locale, else
/// the country's primary language (the region of a lookup that agrees on
/// the country picks the language of a region that speaks another); the
/// declared zone, else the zone of a lookup that agrees on the country,
/// else the country's main zone; a position near a lookup that agrees on
/// the country, else none. A lookup that names another country says
/// nothing of the declared place: it only counts as a mismatch.
pub fn declared_place(
    proxy: &Proxy,
    declaration: &Declaration,
    engine: Engine,
    seed: &Seed,
) -> Place {
    let country = declaration.country;
    let agreeing = proxy.observed_geo().filter(|geo| geo.country == country);
    let (timezone, zone_source) = match declaration
        .timezone
        .as_deref()
        .filter(|zone| place::is_zone(zone))
    {
        Some(zone) => (zone.to_owned(), ZoneSource::Declared),
        None => place::timezone(
            agreeing.and_then(|geo| geo.timezone.as_deref()),
            Some(country),
        ),
    };
    let locale = declaration
        .locale
        .as_deref()
        .and_then(|tag| locale::declared(tag, engine))
        .unwrap_or_else(|| {
            locale::resolve(
                Some(country),
                agreeing.and_then(|geo| geo.region.as_deref()),
                engine,
            )
        });
    Place {
        country: Some(country),
        timezone,
        zone_source,
        locale,
        geolocation: place::position(
            agreeing.and_then(kage_core::domain::GeoRecord::coordinates),
            seed,
        ),
    }
}

/// Whether a persona's identity is another than its proxy's declared place
/// gives: another country, or a locale in another language (ADR 0013,
/// consult 3c). Such a persona is made again; one whose zone or position
/// only moved follows.
pub fn declaration_moved(persona: &Persona, place: &Place) -> bool {
    persona.country != place.country
        || locale::language_of(&persona.locale) != locale::language_of(&place.locale.locale)
}

/// What a new geo record changed about a persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoChange {
    /// Nothing.
    None,
    /// The time zone or the position, inside the same country and
    /// language.
    Place,
    /// The country, or the language of its region: the locale and the
    /// languages changed with it.
    Identity,
}

/// Brings a persona in step with the place its proxy's geo record gives
/// (spec §6.3). The time zone always follows the address, because a target
/// compares the two. The position follows when the address is in another
/// place, not when a lookup moved a city's centre by a little.
pub fn follow_geo(persona: &mut Persona, place: &Place) -> GeoChange {
    let identity = persona.country != place.country
        || persona.locale != place.locale.locale
        || persona.languages != place.locale.languages;
    let zone = persona.timezone != place.timezone;
    let moved = match (persona.geolocation, place.geolocation) {
        (None, Some(_)) => true,
        (Some(old), Some(new)) => {
            place::distance_km((old.latitude, old.longitude), (new.latitude, new.longitude))
                > SAME_PLACE_KM
        }
        // A lookup that names no coordinates this time takes nothing away.
        (_, None) => false,
    };
    if !(identity || zone || moved) {
        return GeoChange::None;
    }
    persona.country = place.country;
    persona.timezone.clone_from(&place.timezone);
    persona.locale.clone_from(&place.locale.locale);
    persona.languages.clone_from(&place.locale.languages);
    if identity || moved {
        persona.geolocation = place.geolocation.or(persona.geolocation);
    }
    if identity {
        GeoChange::Identity
    } else {
        GeoChange::Place
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::domain::{
        CoolingReason, Engine, GeoRecord, PersonaState, Proxy, ProxyHealth, ProxyState,
    };
    use kage_core::persona::place::ZoneSource;

    use super::{
        GeoChange, cool_until_selftest, declaration_moved, declared_place, follow_geo,
        follow_proxy, place_of, retire, selftest_passed,
    };
    use crate::test_support::{persona, proxy};

    fn observed(proxy: &mut Proxy, country: &str, city: &str, zone: &str, loc: &str) {
        proxy.last_health = Some(ProxyHealth {
            geo: Some(GeoRecord {
                country: country.parse().unwrap(),
                city: Some(city.to_owned()),
                region: None,
                asn: None,
                org: None,
                timezone: Some(zone.to_owned()),
                loc: Some(loc.to_owned()),
                ip: "203.0.113.9".parse().unwrap(),
                checked_at: Utc::now(),
            }),
            ..ProxyHealth::new(Utc::now(), true)
        });
    }

    #[test]
    fn a_persona_cools_with_its_proxy_and_returns_with_it() {
        let mut persona = persona(Engine::Chromium, 155);

        assert_eq!(follow_proxy(&mut persona, ProxyState::Healthy), None);
        assert_eq!(
            follow_proxy(&mut persona, ProxyState::Backoff),
            Some((PersonaState::Active, PersonaState::Cooling))
        );
        assert_eq!(persona.cooling_reason, Some(CoolingReason::ProxyBackoff));
        // Still in backoff: nothing new.
        assert_eq!(follow_proxy(&mut persona, ProxyState::Backoff), None);
        assert_eq!(follow_proxy(&mut persona, ProxyState::Replacing), None);
        assert_eq!(
            follow_proxy(&mut persona, ProxyState::Degraded),
            Some((PersonaState::Cooling, PersonaState::Active))
        );
        assert_eq!(persona.cooling_reason, None);
        // A proxy that is being replaced cools it too.
        assert_eq!(
            follow_proxy(&mut persona, ProxyState::Replacing),
            Some((PersonaState::Active, PersonaState::Cooling))
        );
        // The proxy's own states `new` and `retired` are not the persona's
        // business here: generation and retirement are.
        for state in [ProxyState::New, ProxyState::Retired] {
            assert_eq!(follow_proxy(&mut persona, state), None);
        }
    }

    #[test]
    fn only_a_passing_selftest_ends_a_selftest_cooling() {
        let mut persona = persona(Engine::Firefox, 156);

        assert_eq!(
            cool_until_selftest(&mut persona, CoolingReason::SelftestFailed),
            Some((PersonaState::Active, PersonaState::Cooling))
        );
        // The proxy coming and going changes nothing about it.
        assert_eq!(follow_proxy(&mut persona, ProxyState::Backoff), None);
        assert_eq!(follow_proxy(&mut persona, ProxyState::Healthy), None);
        assert_eq!(persona.state, PersonaState::Cooling);
        assert_eq!(persona.cooling_reason, Some(CoolingReason::SelftestFailed));
        // Another reason of the same strength does not replace it.
        assert_eq!(
            cool_until_selftest(&mut persona, CoolingReason::GeoChanged),
            None
        );
        assert_eq!(persona.cooling_reason, Some(CoolingReason::SelftestFailed));

        // The self-test passes while the proxy is down: the persona goes
        // on cooling, now for its proxy, and returns with it.
        assert_eq!(selftest_passed(&mut persona, false), None);
        assert_eq!(persona.cooling_reason, Some(CoolingReason::ProxyBackoff));
        assert_eq!(selftest_passed(&mut persona, true), None);
        assert_eq!(
            follow_proxy(&mut persona, ProxyState::Healthy),
            Some((PersonaState::Cooling, PersonaState::Active))
        );

        // The stronger reason replaces the weaker one.
        follow_proxy(&mut persona, ProxyState::Backoff);
        assert_eq!(
            cool_until_selftest(&mut persona, CoolingReason::GeoChanged),
            None
        );
        assert_eq!(persona.cooling_reason, Some(CoolingReason::GeoChanged));
        assert_eq!(
            selftest_passed(&mut persona, true),
            Some((PersonaState::Cooling, PersonaState::Active))
        );
        assert_eq!(persona.cooling_reason, None);
        // A passing self-test of an active persona changes nothing.
        assert_eq!(selftest_passed(&mut persona, true), None);
    }

    #[test]
    fn retirement_is_final() {
        let mut persona = persona(Engine::Chromium, 155);
        let now = Utc::now();
        follow_proxy(&mut persona, ProxyState::Backoff);

        assert_eq!(
            retire(&mut persona, now, "proxy_retired"),
            Some((PersonaState::Cooling, PersonaState::Retired))
        );
        assert_eq!(persona.retired_at, Some(now));
        assert_eq!(persona.retire_reason.as_deref(), Some("proxy_retired"));
        assert_eq!(persona.cooling_reason, None);
        // Nothing moves a retired persona.
        assert_eq!(retire(&mut persona, now, "operator"), None);
        assert_eq!(persona.retire_reason.as_deref(), Some("proxy_retired"));
        assert_eq!(follow_proxy(&mut persona, ProxyState::Healthy), None);
        assert_eq!(
            cool_until_selftest(&mut persona, CoolingReason::SelftestFailed),
            None
        );
        assert_eq!(selftest_passed(&mut persona, true), None);
        assert_eq!(persona.state, PersonaState::Retired);
    }

    #[test]
    fn the_place_comes_from_the_observed_record_and_the_providers_is_the_fallback() {
        let seed = kage_core::seed::Seed::from_u64(1);
        // No lookup yet: the provider's country, its main zone, no position.
        let mut proxy = proxy("AT");
        let place = place_of(&proxy, Engine::Firefox, &seed);
        assert_eq!(place.country.unwrap().as_str(), "AT");
        assert_eq!(place.timezone, "Europe/Vienna");
        assert_eq!(place.zone_source, ZoneSource::Country);
        assert_eq!(place.locale.locale, "de-AT");
        assert_eq!(place.locale.languages, ["de", "en-US", "en"]);
        assert_eq!(place.geolocation, None);

        // The lookup says Marseille where the provider says Austria: the
        // observed record wins in everything.
        observed(
            &mut proxy,
            "FR",
            "Marseille",
            "Europe/Paris",
            "43.2965,5.3698",
        );
        let place = place_of(&proxy, Engine::Chromium, &seed);
        assert_eq!(place.country.unwrap().as_str(), "FR");
        assert_eq!(place.timezone, "Europe/Paris");
        assert_eq!(place.zone_source, ZoneSource::Observed);
        assert_eq!(place.locale.locale, "fr-FR");
        assert_eq!(place.locale.languages, ["fr-FR", "fr", "en-US", "en"]);
        let position = place.geolocation.unwrap();
        assert!((position.latitude - 43.2965).abs() < 0.05);
        assert!((position.longitude - 5.3698).abs() < 0.07);
    }

    /// ADR 0013: what the operator declared is the persona's identity, and
    /// a lookup only adds where it agrees.
    #[test]
    fn a_declared_place_is_the_declaration_and_what_an_agreeing_lookup_adds() {
        let seed = kage_core::seed::Seed::from_u64(3);
        let mut proxy = proxy("AT");
        proxy.provider = kage_core::domain::ProxyProviderKind::Static;
        let austria = proxy.declaration().unwrap();

        // No lookup yet: the country's language and main zone, no position.
        let place = declared_place(&proxy, &austria, Engine::Firefox, &seed);
        assert_eq!(place.country.unwrap().as_str(), "AT");
        assert_eq!(place.locale.locale, "de-AT");
        assert_eq!(place.locale.languages, ["de", "en-US", "en"]);
        assert_eq!(place.timezone, "Europe/Vienna");
        assert_eq!(place.zone_source, ZoneSource::Country);
        assert_eq!(place.geolocation, None);

        // A lookup that agrees gives its zone and a position near it.
        observed(&mut proxy, "AT", "Graz", "Europe/Vienna", "47.0707,15.4395");
        let place = declared_place(&proxy, &austria, Engine::Firefox, &seed);
        assert_eq!(place.zone_source, ZoneSource::Observed);
        assert!((place.geolocation.unwrap().latitude - 47.0707).abs() < 0.05);

        // One that places the address in France says nothing of it.
        observed(&mut proxy, "FR", "Paris", "Europe/Paris", "48.8566,2.3522");
        let place = declared_place(&proxy, &austria, Engine::Chromium, &seed);
        assert_eq!(place.country.unwrap().as_str(), "AT");
        assert_eq!(place.locale.locale, "de-AT");
        // Chrome in Austria ships the German of Germany (spec §6.1).
        assert_eq!(place.locale.languages, ["de-DE", "de", "en-US", "en"]);
        assert_eq!(place.timezone, "Europe/Vienna");
        assert_eq!(place.zone_source, ZoneSource::Country);
        assert_eq!(place.geolocation, None);

        // A declared locale overrides the country's language, and a
        // declared zone the lookup's.
        let swiss_french = kage_core::domain::Declaration {
            country: "CH".parse().unwrap(),
            locale: Some("fr-CH".to_owned()),
            timezone: Some("Europe/Zurich".to_owned()),
        };
        observed(
            &mut proxy,
            "CH",
            "Zurich",
            "Europe/Busingen",
            "47.3769,8.5417",
        );
        let place = declared_place(&proxy, &swiss_french, Engine::Chromium, &seed);
        assert_eq!(place.locale.locale, "fr-CH");
        assert_eq!(place.locale.languages, ["fr-FR", "fr", "en-US", "en"]);
        assert_eq!(place.timezone, "Europe/Zurich");
        assert_eq!(place.zone_source, ZoneSource::Declared);
        assert!(place.geolocation.is_some());
    }

    /// Consult 3c: another country, or another language, is another
    /// persona; a zone or a position is the same one moved.
    #[test]
    fn a_corrected_declaration_is_another_persona_when_the_identity_changes() {
        let seed = kage_core::seed::Seed::from_u64(4);
        let mut proxy = proxy("AT");
        proxy.provider = kage_core::domain::ProxyProviderKind::Static;
        let mut persona = persona(Engine::Chromium, 155);
        let place = declared_place(
            &proxy,
            &proxy.declaration().unwrap(),
            Engine::Chromium,
            &seed,
        );
        follow_geo(&mut persona, &place);
        assert!(!declaration_moved(&persona, &place));

        // Another zone in the same country and language: the same persona.
        proxy.timezone = Some("Europe/Berlin".to_owned());
        let zone = declared_place(
            &proxy,
            &proxy.declaration().unwrap(),
            Engine::Chromium,
            &seed,
        );
        assert!(!declaration_moved(&persona, &zone));
        // English in Austria: another language.
        proxy.locale = Some("en-AT".to_owned());
        let english = declared_place(
            &proxy,
            &proxy.declaration().unwrap(),
            Engine::Chromium,
            &seed,
        );
        assert!(declaration_moved(&persona, &english));
        // Germany: another country, though the language is the same.
        proxy.locale = None;
        proxy.country = Some("DE".parse().unwrap());
        let germany = declared_place(
            &proxy,
            &proxy.declaration().unwrap(),
            Engine::Chromium,
            &seed,
        );
        assert!(declaration_moved(&persona, &germany));
    }

    #[test]
    fn a_new_geo_record_moves_the_persona_and_says_what_changed() {
        let mut persona = persona(Engine::Chromium, 155);
        let seed = persona.seed;
        let mut proxy = proxy("DE");
        observed(&mut proxy, "DE", "Berlin", "Europe/Berlin", "52.52,13.405");
        // The fixture is a Berlin persona already, a little off the centre.
        persona.geolocation = place_of(&proxy, Engine::Chromium, &seed).geolocation;
        let berlin = persona.clone();
        assert_eq!(
            follow_geo(&mut persona, &place_of(&proxy, Engine::Chromium, &seed)),
            GeoChange::None
        );

        // The lookup moves the city's centre by a few hundred metres:
        // nothing changes.
        observed(
            &mut proxy,
            "DE",
            "Berlin",
            "Europe/Berlin",
            "52.5230,13.4100",
        );
        assert_eq!(
            follow_geo(&mut persona, &place_of(&proxy, Engine::Chromium, &seed)),
            GeoChange::None
        );
        assert_eq!(persona, berlin);

        // Another city of the same country: the position follows.
        observed(
            &mut proxy,
            "DE",
            "Munich",
            "Europe/Berlin",
            "48.1374,11.5755",
        );
        assert_eq!(
            follow_geo(&mut persona, &place_of(&proxy, Engine::Chromium, &seed)),
            GeoChange::Place
        );
        assert!((persona.geolocation.unwrap().latitude - 48.1374).abs() < 0.05);
        assert_eq!(persona.locale, "de-DE");
        assert_eq!(persona.languages, berlin.languages);

        // Another country: time zone, position, locale and languages.
        observed(&mut proxy, "IT", "Milan", "Europe/Rome", "45.4643,9.1895");
        assert_eq!(
            follow_geo(&mut persona, &place_of(&proxy, Engine::Chromium, &seed)),
            GeoChange::Identity
        );
        assert_eq!(persona.country.unwrap().as_str(), "IT");
        assert_eq!(persona.timezone, "Europe/Rome");
        assert_eq!(persona.locale, "it-IT");
        assert_eq!(persona.languages, ["it-IT", "it", "en-US", "en"]);
        assert!((persona.geolocation.unwrap().longitude - 9.1895).abs() < 0.07);
        // What makes the persona never changes.
        assert_eq!(persona.seed, berlin.seed);
        assert_eq!(persona.fingerprint, berlin.fingerprint);
        assert_eq!(persona.screen, berlin.screen);

        // A lookup without coordinates takes the position away from
        // nobody, and a zone alone is a change of place.
        let mut milan = proxy.clone();
        if let Some(geo) = milan
            .last_health
            .as_mut()
            .and_then(|health| health.geo.as_mut())
        {
            geo.loc = None;
            geo.timezone = Some("Europe/Vatican".to_owned());
        }
        assert_eq!(
            follow_geo(&mut persona, &place_of(&milan, Engine::Chromium, &seed)),
            GeoChange::Place
        );
        assert_eq!(persona.timezone, "Europe/Vatican");
        assert!(persona.geolocation.is_some());
    }
}
