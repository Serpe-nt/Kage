//! Emulation profiles: which browser a client presents as at the TLS and
//! HTTP/2 layers, and the values it puts into its identity headers.
//!
//! A profile comes from wreq-util and is named like `chrome_149`. The http
//! path supports the desktop Chrome and Firefox profiles only, because those
//! are the two browsers the sidecar engines run (spec §2).
//!
//! wreq-util supplies the TLS options, the HTTP/2 options and the *values*
//! of the identity headers (`User-Agent`, the `sec-ch-ua` triple, `Accept`,
//! `Accept-Encoding`). Which headers a request carries, and in which order,
//! is decided in [`crate::headers`], not by the profile.
//!
//! # The mapping from a persona to a profile (spec §6.1 step 4)
//!
//! [`resolve`] maps (engine, browser major, claimed OS) to a profile whose
//! major is exactly the engine's. When this build of wreq-util has no such
//! profile the answer is `None` and the persona is browser-only (risk R7).
//! The table is derived from the profiles wreq-util enumerates, so it cannot
//! drift from the pin; a test fails when the pin moves and the newest
//! profiles are no longer the ones `docs/versions.md` records.

use std::fmt;

use kage_core::domain::{ClaimedOs, Engine};
use wreq::header::{ACCEPT, ACCEPT_ENCODING, HeaderValue, USER_AGENT};
use wreq_util::{Emulation, Platform, Profile};

/// The browser family a profile belongs to. It decides the header order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Google Chrome.
    Chrome,
    /// Mozilla Firefox.
    Firefox,
}

impl Family {
    /// The family the engine's browser belongs to.
    pub const fn of_engine(engine: Engine) -> Self {
        match engine {
            Engine::Chromium => Self::Chrome,
            Engine::Firefox => Self::Firefox,
        }
    }

    /// The sidecar engine that runs this family.
    pub const fn engine(self) -> Engine {
        match self {
            Self::Chrome => Engine::Chromium,
            Self::Firefox => Engine::Firefox,
        }
    }

    /// The start of this family's profile names in wreq-util.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Chrome => "chrome_",
            Self::Firefox => "firefox_",
        }
    }

    /// The family's name, as in a profile name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Firefox => "firefox",
        }
    }
}

impl fmt::Display for Family {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a profile cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// The name is not a profile this build of wreq-util ships.
    #[error("unknown emulation profile {name:?}; this build knows: {known}")]
    Unknown {
        /// The name asked for.
        name: String,
        /// The Chrome and Firefox profiles this build knows.
        known: String,
    },
    /// The profile exists but is not desktop Chrome or Firefox.
    #[error(
        "emulation profile {0:?} is not a desktop Chrome or Firefox profile; the http path supports only those"
    )]
    Unsupported(String),
    /// The platform is not one a persona can claim.
    #[error("unknown platform {0:?}; use windows, macos or linux")]
    UnknownPlatform(String),
    /// The profile does not carry a header every browser sends.
    #[error("emulation profile {profile} carries no {header} header")]
    MissingHeader {
        /// The profile.
        profile: String,
        /// The header.
        header: &'static str,
    },
}

/// The name of every emulation profile this build of wreq-util ships.
pub fn profile_names() -> Vec<String> {
    Profile::VARIANTS
        .iter()
        .filter_map(|profile| match serde_json::to_value(profile) {
            Ok(serde_json::Value::String(name)) => Some(name),
            _ => None,
        })
        .collect()
}

/// The family and browser major a profile name stands for: `chrome_149` is
/// Chrome 149. `None` for every other kind of name (Edge, Opera, Safari,
/// `OkHttp`, and the private-window and Android variants of Firefox).
pub fn family_and_major(name: &str) -> Option<(Family, u32)> {
    [Family::Chrome, Family::Firefox]
        .into_iter()
        .find_map(|family| {
            let digits = name.strip_prefix(family.prefix())?;
            // `parse` alone would accept a sign.
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| digits.parse().ok())
                .flatten()
                .map(|major| (family, major))
        })
}

/// The majors of a family that this build of wreq-util has a profile for,
/// in ascending order.
pub fn majors(family: Family) -> Vec<u32> {
    let mut majors: Vec<u32> = profile_names()
        .iter()
        .filter_map(|name| family_and_major(name))
        .filter(|(found, _)| *found == family)
        .map(|(_, major)| major)
        .collect();
    majors.sort_unstable();
    majors.dedup();
    majors
}

/// The newest major of a family that this build of wreq-util has a profile
/// for.
pub fn newest_major(family: Family) -> Option<u32> {
    majors(family).last().copied()
}

/// The operating system named in configuration: `windows`, `macos` or
/// `linux`.
///
/// # Errors
///
/// Fails for any other name.
pub fn os_named(name: &str) -> Result<ClaimedOs, ProfileError> {
    name.parse()
        .map_err(|_| ProfileError::UnknownPlatform(name.to_owned()))
}

const fn platform_of(os: ClaimedOs) -> Platform {
    match os {
        ClaimedOs::Windows => Platform::Windows,
        ClaimedOs::Macos => Platform::MacOS,
        ClaimedOs::Linux => Platform::Linux,
    }
}

/// A wreq-util profile on one operating system: everything the TLS and
/// HTTP/2 layers of a client are built from.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileChoice {
    name: String,
    profile: Profile,
    family: Family,
    major: u32,
    os: ClaimedOs,
}

impl fmt::Debug for ProfileChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileChoice")
            .field("name", &self.name)
            .field("os", &self.os.as_str())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ProfileChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on {}", self.name, self.os)
    }
}

impl ProfileChoice {
    /// The profile with this name, for example `chrome_149`, claiming `os`.
    ///
    /// # Errors
    ///
    /// Fails when this build of wreq-util has no such profile (the error
    /// lists the Chrome and Firefox profiles it has), or when the profile is
    /// not desktop Chrome or Firefox.
    pub fn named(name: &str, os: ClaimedOs) -> Result<Self, ProfileError> {
        let profile: Profile = serde_json::from_value(serde_json::Value::String(name.to_owned()))
            .map_err(|_| ProfileError::Unknown {
            name: name.to_owned(),
            known: profile_names()
                .into_iter()
                .filter(|known| family_and_major(known).is_some())
                .collect::<Vec<_>>()
                .join(", "),
        })?;
        let (family, major) =
            family_and_major(name).ok_or_else(|| ProfileError::Unsupported(name.to_owned()))?;
        Ok(Self {
            name: name.to_owned(),
            profile,
            family,
            major,
            os,
        })
    }

    /// The wreq-util name, for example `chrome_149`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The browser family.
    pub const fn family(&self) -> Family {
        self.family
    }

    /// The browser major the profile presents.
    pub const fn major(&self) -> u32 {
        self.major
    }

    /// The operating system the profile claims.
    pub const fn os(&self) -> ClaimedOs {
        self.os
    }

    /// The file-name stem of this choice's fingerprint fixture:
    /// `chrome_149-windows`.
    pub fn fixture_stem(&self) -> String {
        format!("{}-{}", self.name, self.os)
    }

    /// The wreq emulation: TLS options, HTTP/2 options and the profile's own
    /// default headers.
    pub(crate) fn emulation(&self) -> wreq::Emulation {
        wreq::IntoEmulation::into_emulation(
            Emulation::builder()
                .profile(self.profile)
                .platform(platform_of(self.os))
                .build(),
        )
    }

    /// What a client's transport is built from: the TLS and HTTP/2 options
    /// without the profile's default headers, which `crate::headers`
    /// replaces.
    pub(crate) fn transport(&self) -> wreq::Emulation {
        wreq::IntoEmulation::into_emulation(
            Emulation::builder()
                .profile(self.profile)
                .platform(platform_of(self.os))
                .headers(false)
                .build(),
        )
    }

    /// The values this profile puts into its identity headers.
    ///
    /// # Errors
    ///
    /// Fails when the profile lacks a header every browser sends, which
    /// would mean wreq-util changed under the pin.
    pub fn base_headers(&self) -> Result<BaseHeaders, ProfileError> {
        let headers = self.emulation().headers;
        let required = |name: &'static str, value: Option<&HeaderValue>| {
            value.cloned().ok_or_else(|| ProfileError::MissingHeader {
                profile: self.name.clone(),
                header: name,
            })
        };
        let client_hints = match self.family {
            Family::Chrome => Some(ClientHints {
                brands: required("sec-ch-ua", headers.get("sec-ch-ua"))?,
                mobile: required("sec-ch-ua-mobile", headers.get("sec-ch-ua-mobile"))?,
                platform: required("sec-ch-ua-platform", headers.get("sec-ch-ua-platform"))?,
            }),
            Family::Firefox => None,
        };
        Ok(BaseHeaders {
            user_agent: required("user-agent", headers.get(USER_AGENT))?,
            client_hints,
            accept_document: required("accept", headers.get(ACCEPT))?,
            accept_encoding: required("accept-encoding", headers.get(ACCEPT_ENCODING))?,
        })
    }

    /// The profile's complete default header map, as wreq-util defines it.
    /// Used by tests that pin what the profile ships.
    #[cfg(test)]
    pub(crate) fn default_headers(&self) -> wreq::header::HeaderMap {
        self.emulation().headers
    }
}

/// Chrome's low-entropy client hints, sent with every request to a
/// trustworthy origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHints {
    /// `sec-ch-ua`: the brand list.
    pub brands: HeaderValue,
    /// `sec-ch-ua-mobile`.
    pub mobile: HeaderValue,
    /// `sec-ch-ua-platform`.
    pub platform: HeaderValue,
}

/// The identity values a profile supplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseHeaders {
    /// `User-Agent`.
    pub user_agent: HeaderValue,
    /// The `sec-ch-ua` triple; `None` for Firefox, which sends none.
    pub client_hints: Option<ClientHints>,
    /// `Accept` of a document navigation.
    pub accept_document: HeaderValue,
    /// `Accept-Encoding` toward a trustworthy origin.
    pub accept_encoding: HeaderValue,
}

/// One row of the mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingRow {
    /// The persona's engine.
    pub engine: Engine,
    /// The major of the engine's bundled browser.
    pub browser_major: u32,
    /// The operating system the persona claims.
    pub os: ClaimedOs,
    /// The wreq-util profile.
    pub profile: String,
}

/// The operating systems a persona can claim.
pub const CLAIMED_OSES: [ClaimedOs; 3] = [ClaimedOs::Windows, ClaimedOs::Macos, ClaimedOs::Linux];

/// How many of a family's newest majors the mapping covers.
///
/// The header tables in `crate::headers` describe the current generation of
/// each browser (Chrome since 132 and Firefox since 135 share one TLS and
/// HTTP/2 configuration and one header set in wreq-util). A browser updates
/// itself, so an engine never bundles a major far behind the newest
/// profile; older profiles are left out rather than mapped to tables that
/// were not written for them.
pub const MAPPED_MAJORS: usize = 8;

/// The majors of a family the mapping covers: its newest
/// [`MAPPED_MAJORS`], in ascending order.
pub fn mapped_majors(family: Family) -> Vec<u32> {
    let all = majors(family);
    all[all.len().saturating_sub(MAPPED_MAJORS)..].to_vec()
}

/// The whole table of exact matches (spec §6.1 step 4): every (engine,
/// major, OS) this build of wreq-util has a profile of the current
/// generation for. Each row has a fingerprint fixture.
pub fn mapping_table() -> Vec<MappingRow> {
    let mut rows = Vec::new();
    for family in [Family::Chrome, Family::Firefox] {
        for major in mapped_majors(family) {
            for os in CLAIMED_OSES {
                rows.push(MappingRow {
                    engine: family.engine(),
                    browser_major: major,
                    os,
                    profile: format!("{}{major}", family.prefix()),
                });
            }
        }
    }
    rows
}

/// Browser majors beyond wreq-util's newest profile for which a capture
/// from the real browser has proven that its TLS fingerprint, its HTTP/2
/// fingerprint and its navigation headers are what the newest profile and
/// the header tables produce. Only for these is a profile derived.
///
/// **Empty today.** Milestones M4 (Chrome) and M5 (Camoufox) add a major
/// here, each together with the capture that proves it:
/// `tests/fixtures/fingerprints/browser/<engine>-<major>.json`, recorded
/// from the engine against the fingerprint echo. A test compares every
/// capture named here with the derived profile's own fixture, on JA4, on the
/// Akamai string and on the header names of a navigation in order, and fails
/// when a major is listed without an equal capture. If the real browser's
/// headers differ, the header tables get an entry for that major first.
pub const VERIFIED_MAJORS: &[(Engine, u32)] = &[];

/// Chrome's `sec-ch-ua` brand list for a major version.
///
/// The list holds the browser, the engine and one made-up brand. Chromium
/// derives the made-up brand's name, its version and the order of the three
/// from the major version alone, so that servers cannot learn to expect one
/// fixed string. This is the same computation.
pub fn chrome_brands(major: u32) -> String {
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    const SEPARATORS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const VERSIONS: [&str; 3] = ["8", "99", "24"];
    let seed = usize::try_from(major).unwrap_or(0);
    let made_up = format!(
        "\"Not{}A{}Brand\";v=\"{}\"",
        SEPARATORS[seed % SEPARATORS.len()],
        SEPARATORS[(seed + 1) % SEPARATORS.len()],
        VERSIONS[seed % VERSIONS.len()]
    );
    let order = ORDERS[seed % ORDERS.len()];
    let mut list = [String::new(), String::new(), String::new()];
    list[order[0]] = made_up;
    list[order[1]] = format!("\"Chromium\";v=\"{major}\"");
    list[order[2]] = format!("\"Google Chrome\";v=\"{major}\"");
    list.join(", ")
}

/// A profile for a persona, with the identity values that go with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// The wreq-util profile: the TLS and HTTP/2 fingerprint.
    pub choice: ProfileChoice,
    /// The browser major the client presents in `User-Agent` and
    /// `sec-ch-ua`: the engine's.
    pub browser_major: u32,
    /// The `User-Agent` for [`Self::browser_major`] when the profile's own
    /// is for another major; `None` for an exact match.
    pub user_agent: Option<String>,
    /// The `sec-ch-ua` brand list, likewise; always `None` for Firefox.
    pub client_hint_brands: Option<String>,
}

impl ResolvedProfile {
    /// Whether the profile's major is the engine's, as opposed to a
    /// profile derived for a verified newer major.
    pub const fn is_exact(&self) -> bool {
        self.choice.major() == self.browser_major
    }
}

/// The profile for a persona (spec §6.1 step 4).
///
/// 1. An exact match: the profile whose major is the engine's, when it is
///    of the current generation ([`mapping_table`]).
/// 2. A derived profile, only for a major in [`VERIFIED_MAJORS`]: the
///    newest profile's TLS and HTTP/2 with `User-Agent` and `sec-ch-ua`
///    written for the engine's major, so the persona is one browser on both
///    paths.
/// 3. Otherwise `None`: the persona is browser-only (risk R7) and the alert
///    `http_profile_missing` is raised.
pub fn resolve(engine: Engine, browser_major: u32, os: ClaimedOs) -> Option<ResolvedProfile> {
    let family = Family::of_engine(engine);
    if mapped_majors(family).contains(&browser_major) {
        let name = format!("{}{browser_major}", family.prefix());
        return Some(ResolvedProfile {
            choice: ProfileChoice::named(&name, os).ok()?,
            browser_major,
            user_agent: None,
            client_hint_brands: None,
        });
    }
    if !VERIFIED_MAJORS.contains(&(engine, browser_major)) {
        return None;
    }
    derive(family, browser_major, os)
}

/// The newest profile of a family presenting as a newer major. Not gated:
/// [`resolve`] decides whether a major may be derived.
pub fn derive(family: Family, browser_major: u32, os: ClaimedOs) -> Option<ResolvedProfile> {
    let newest = newest_major(family)?;
    if browser_major <= newest {
        return None;
    }
    let choice = ProfileChoice::named(&format!("{}{newest}", family.prefix()), os).ok()?;
    let own = choice.base_headers().ok()?.user_agent;
    let own = own.to_str().ok()?;
    let (user_agent, client_hint_brands) = match family {
        Family::Chrome => (
            own.replace(
                &format!("Chrome/{newest}.0.0.0"),
                &format!("Chrome/{browser_major}.0.0.0"),
            ),
            Some(chrome_brands(browser_major)),
        ),
        Family::Firefox => (
            own.replace(&format!("rv:{newest}.0"), &format!("rv:{browser_major}.0"))
                .replace(
                    &format!("Firefox/{newest}.0"),
                    &format!("Firefox/{browser_major}.0"),
                ),
            None,
        ),
    };
    // The profile's own string must have held the version to replace.
    (user_agent != own).then_some(ResolvedProfile {
        choice,
        browser_major,
        user_agent: Some(user_agent),
        client_hint_brands,
    })
}

/// Checks that the browser majors the configuration names for the engines
/// resolve to a profile on every operating system. An engine upgrade must
/// not switch the http path off unnoticed: `kage config check` fails, and
/// the milestone that moves the major records the capture that unlocks it.
///
/// # Errors
///
/// A message naming the configuration key and what this build can serve.
pub fn check_engine_majors(chromium: Option<u32>, firefox: Option<u32>) -> Result<(), String> {
    for (engine, major) in [(Engine::Chromium, chromium), (Engine::Firefox, firefox)] {
        let Some(major) = major else {
            continue;
        };
        let family = Family::of_engine(engine);
        if CLAIMED_OSES
            .iter()
            .all(|os| resolve(engine, major, *os).is_some())
        {
            continue;
        }
        let exact = mapped_majors(family)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let verified = VERIFIED_MAJORS
            .iter()
            .filter(|(verified, _)| *verified == engine)
            .map(|(_, major)| major.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "engines.{engine}.browser_major: there is no emulation profile for {family} {major}. \
             This build has exact profiles for {exact} and verified captures for [{verified}]. \
             The http path would be off for every {engine} persona: record the browser capture \
             that verifies {major} (docs/runbooks/fingerprint-echo.md) or unset the key"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kage_core::domain::{ClaimedOs, Engine};

    use super::{
        CLAIMED_OSES, Family, MAPPED_MAJORS, ProfileChoice, ProfileError, VERIFIED_MAJORS,
        check_engine_majors, chrome_brands, derive, family_and_major, majors, mapped_majors,
        mapping_table, newest_major, os_named, profile_names, resolve,
    };

    /// The newest profiles of wreq-util 0.2.0, as `docs/versions.md` records
    /// them. When the pin moves, this test fails until both are updated.
    const NEWEST_CHROME: u32 = 149;
    const NEWEST_FIREFOX: u32 = 151;

    #[test]
    fn the_enumeration_matches_the_pin_recorded_in_versions_md() {
        let names = profile_names();
        assert!(names.len() > 100, "{} profiles", names.len());

        assert_eq!(
            newest_major(Family::Chrome),
            Some(NEWEST_CHROME),
            "wreq-util's newest Chrome profile changed: update docs/versions.md, the default of \
             proxies.health.profile, the fingerprint fixtures and this constant together"
        );
        assert_eq!(
            newest_major(Family::Firefox),
            Some(NEWEST_FIREFOX),
            "wreq-util's newest Firefox profile changed: update docs/versions.md, the \
             fingerprint fixtures and this constant together"
        );
        // Gaps are real: wreq-util does not ship every major.
        assert!(majors(Family::Chrome).contains(&143));
        assert!(!majors(Family::Chrome).contains(&150));
        assert!(!majors(Family::Firefox).contains(&152));
        assert!(!majors(Family::Firefox).contains(&141));
        assert_eq!(
            mapped_majors(Family::Chrome),
            [142, 143, 144, 145, 146, 147, 148, 149]
        );
        assert_eq!(
            mapped_majors(Family::Firefox),
            [144, 145, 146, 147, 148, 149, 150, 151]
        );
    }

    #[test]
    fn only_desktop_chrome_and_firefox_names_have_a_family() {
        assert_eq!(family_and_major("chrome_149"), Some((Family::Chrome, 149)));
        assert_eq!(
            family_and_major("firefox_151"),
            Some((Family::Firefox, 151))
        );
        for name in [
            "edge_148",
            "opera_131",
            "safari_26.4",
            "okhttp_5",
            "firefox_private_136",
            "firefox_android_135",
            "chrome_",
            "chrome_+149",
            "chrome_149a",
            "chromium_149",
            "",
        ] {
            assert_eq!(family_and_major(name), None, "{name}");
        }
        // Every name wreq-util ships is either one of ours or clearly not.
        let ours = profile_names()
            .iter()
            .filter(|name| family_and_major(name).is_some())
            .count();
        assert_eq!(
            ours,
            majors(Family::Chrome).len() + majors(Family::Firefox).len()
        );
    }

    /// The enumeration test of the plan: every row of the mapping table
    /// resolves to a profile wreq-util really ships, and that profile claims
    /// the browser, the major and the operating system the row says.
    #[test]
    fn every_row_of_the_mapping_table_resolves_and_claims_what_it_says() {
        let table = mapping_table();
        assert_eq!(table.len(), MAPPED_MAJORS * 2 * 3);
        let shipped = profile_names();

        for row in &table {
            assert!(
                shipped.contains(&row.profile),
                "{row:?}: wreq-util has no such profile"
            );
            let resolved = resolve(row.engine, row.browser_major, row.os)
                .unwrap_or_else(|| panic!("{row:?} does not resolve"));
            assert!(resolved.is_exact());
            assert_eq!(resolved.user_agent, None);
            assert_eq!(resolved.client_hint_brands, None);
            let choice = &resolved.choice;
            assert_eq!(choice.name(), row.profile);
            assert_eq!(choice.major(), row.browser_major);
            assert_eq!(choice.os(), row.os);
            assert_eq!(choice.family().engine(), row.engine);

            let base = choice.base_headers().unwrap();
            let user_agent = base.user_agent.to_str().unwrap();
            let os_token = match row.os {
                ClaimedOs::Windows => "Windows NT 10.0; Win64; x64",
                ClaimedOs::Macos => "Macintosh; Intel Mac OS X 10",
                ClaimedOs::Linux => "X11; ",
            };
            assert!(user_agent.contains(os_token), "{row:?}: {user_agent}");
            let version_token = match row.engine {
                Engine::Chromium => format!("Chrome/{}.0.0.0 Safari/537.36", row.browser_major),
                Engine::Firefox => format!("Firefox/{}.0", row.browser_major),
            };
            assert!(
                user_agent.ends_with(&version_token),
                "{row:?}: {user_agent}"
            );

            match (row.engine, &base.client_hints) {
                (Engine::Chromium, Some(hints)) => {
                    // Chromium's own computation gives the profile's list.
                    assert_eq!(
                        hints.brands.to_str().unwrap(),
                        chrome_brands(row.browser_major),
                        "{row:?}"
                    );
                    assert_eq!(hints.mobile, "?0");
                    let platform = match row.os {
                        ClaimedOs::Windows => "\"Windows\"",
                        ClaimedOs::Macos => "\"macOS\"",
                        ClaimedOs::Linux => "\"Linux\"",
                    };
                    assert_eq!(hints.platform, platform, "{row:?}");
                }
                (Engine::Firefox, None) => {}
                other => panic!("{row:?}: unexpected client hints {other:?}"),
            }
            assert!(
                base.accept_document
                    .to_str()
                    .unwrap()
                    .starts_with("text/html,application/xhtml+xml,application/xml;q=0.9,")
            );
            // Recorded with `emulation-compression` on (docs/versions.md).
            assert_eq!(base.accept_encoding, "gzip, deflate, br, zstd", "{row:?}");
        }
    }

    /// The header tables were written against one set of profile headers.
    /// Every mapped profile must carry exactly that set, or it belongs to
    /// another generation and must not be mapped.
    #[test]
    fn every_mapped_profile_is_of_the_generation_the_header_tables_describe() {
        let names = |choice: &ProfileChoice| -> Vec<String> {
            choice
                .default_headers()
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect()
        };
        for family in [Family::Chrome, Family::Firefox] {
            let newest = ProfileChoice::named(
                &format!("{}{}", family.prefix(), newest_major(family).unwrap()),
                ClaimedOs::Windows,
            )
            .unwrap();
            for major in mapped_majors(family) {
                for os in CLAIMED_OSES {
                    let choice =
                        ProfileChoice::named(&format!("{}{major}", family.prefix()), os).unwrap();
                    assert_eq!(names(&choice), names(&newest), "{choice}");
                }
            }
        }

        let chrome = ProfileChoice::named("chrome_149", ClaimedOs::Windows).unwrap();
        assert_eq!(
            names(&chrome),
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
                "accept-encoding",
                "accept-language",
                "priority",
            ]
        );
        let firefox = ProfileChoice::named("firefox_151", ClaimedOs::Windows).unwrap();
        assert_eq!(
            names(&firefox),
            [
                "user-agent",
                "accept",
                "accept-language",
                "accept-encoding",
                "upgrade-insecure-requests",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "sec-fetch-user",
                "priority",
                "te",
            ]
        );
        // The transport of a client carries none of them.
        assert!(chrome.transport().headers.is_empty());
        assert!(firefox.transport().headers.is_empty());
    }

    #[test]
    fn a_major_without_a_profile_resolves_to_nothing() {
        // The engines' real majors today: Camoufox 152 (D29) and Chrome
        // stable 155/156. wreq-util 0.2.0 has neither and no capture has
        // verified them, so those personas are browser-only (spec §6.1
        // step 4, risk R7).
        assert_eq!(VERIFIED_MAJORS, []);
        for (engine, major) in [
            (Engine::Firefox, 152),
            (Engine::Chromium, 150),
            (Engine::Chromium, 155),
            (Engine::Chromium, 156),
            (Engine::Chromium, 0),
            (Engine::Firefox, u32::MAX),
            // Shipped by wreq-util, but of an older generation.
            (Engine::Chromium, 120),
            (Engine::Chromium, 141),
            (Engine::Firefox, 143),
            (Engine::Firefox, 128),
        ] {
            for os in CLAIMED_OSES {
                assert_eq!(resolve(engine, major, os), None, "{engine} {major} {os}");
            }
        }
        // The neighbours that are mapped resolve.
        assert!(resolve(Engine::Firefox, 151, ClaimedOs::Linux).is_some());
        assert!(resolve(Engine::Chromium, 149, ClaimedOs::Macos).is_some());
        assert!(resolve(Engine::Chromium, 142, ClaimedOs::Windows).is_some());
        // A Chrome major never resolves to a Firefox profile or back.
        assert_eq!(
            resolve(Engine::Chromium, 151, ClaimedOs::Windows),
            None,
            "there is a firefox_151 but no chrome_151"
        );
    }

    #[test]
    fn a_derived_profile_presents_the_engines_major_on_the_newest_transport() {
        let chrome = derive(Family::Chrome, 156, ClaimedOs::Windows).unwrap();
        assert!(!chrome.is_exact());
        assert_eq!(chrome.choice.name(), "chrome_149");
        assert_eq!(chrome.browser_major, 156);
        assert_eq!(
            chrome.user_agent.as_deref(),
            Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/156.0.0.0 Safari/537.36"
            )
        );
        assert_eq!(chrome.client_hint_brands, Some(chrome_brands(156)));

        let firefox = derive(Family::Firefox, 152, ClaimedOs::Macos).unwrap();
        assert_eq!(firefox.choice.name(), "firefox_151");
        assert_eq!(
            firefox.user_agent.as_deref(),
            Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:152.0) Gecko/20100101 \
                 Firefox/152.0"
            )
        );
        assert_eq!(firefox.client_hint_brands, None);

        // Nothing is derived for a major the profiles already reach.
        assert_eq!(derive(Family::Chrome, 149, ClaimedOs::Windows), None);
        assert_eq!(derive(Family::Firefox, 100, ClaimedOs::Windows), None);
    }

    #[test]
    fn the_brand_list_is_chromiums_own_computation() {
        // The strings wreq-util records for real Chrome releases.
        for (major, expected) in [
            (
                145,
                r#""Not:A-Brand";v="99", "Google Chrome";v="145", "Chromium";v="145""#,
            ),
            (
                146,
                r#""Chromium";v="146", "Not-A.Brand";v="24", "Google Chrome";v="146""#,
            ),
            (
                147,
                r#""Google Chrome";v="147", "Not.A/Brand";v="8", "Chromium";v="147""#,
            ),
            (
                148,
                r#""Chromium";v="148", "Google Chrome";v="148", "Not/A)Brand";v="99""#,
            ),
            (
                149,
                r#""Google Chrome";v="149", "Chromium";v="149", "Not)A;Brand";v="24""#,
            ),
        ] {
            assert_eq!(chrome_brands(major), expected);
        }
        // Every list names the three brands once, with the major twice.
        for major in 150..170 {
            let list = chrome_brands(major);
            assert_eq!(list.matches("\"Chromium\"").count(), 1, "{list}");
            assert_eq!(list.matches("\"Google Chrome\"").count(), 1, "{list}");
            assert_eq!(list.matches("Brand\"").count(), 1, "{list}");
            assert_eq!(list.matches(&format!("v=\"{major}\"")).count(), 2, "{list}");
        }
    }

    #[test]
    fn a_configured_engine_major_must_resolve() {
        assert_eq!(check_engine_majors(None, None), Ok(()));
        assert_eq!(check_engine_majors(Some(149), Some(151)), Ok(()));
        assert_eq!(check_engine_majors(Some(142), None), Ok(()));

        // Today's real engines: the gate that makes M4 and M5 record their
        // captures before the http path is switched on for them.
        let chrome = check_engine_majors(Some(156), None).unwrap_err();
        assert!(
            chrome.starts_with("engines.chromium.browser_major:"),
            "{chrome}"
        );
        assert!(chrome.contains("chrome 156"), "{chrome}");
        assert!(
            chrome.contains("142, 143, 144, 145, 146, 147, 148, 149"),
            "{chrome}"
        );
        assert!(chrome.contains("fingerprint-echo.md"), "{chrome}");

        let firefox = check_engine_majors(Some(149), Some(152)).unwrap_err();
        assert!(
            firefox.starts_with("engines.firefox.browser_major:"),
            "{firefox}"
        );
        assert!(firefox.contains("firefox 152"), "{firefox}");
    }

    #[test]
    fn unknown_and_unsupported_names_are_told_apart() {
        let error = ProfileChoice::named("chrome_9000", ClaimedOs::Windows).unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, ProfileError::Unknown { .. }));
        assert!(message.contains("chrome_9000"), "{message}");
        assert!(message.contains("chrome_149"), "{message}");
        assert!(message.contains("firefox_151"), "{message}");
        assert!(!message.contains("okhttp"), "{message}");
        assert!(!message.contains("edge_"), "{message}");

        for name in ["edge_148", "safari_26.4", "okhttp_5", "firefox_private_136"] {
            assert_eq!(
                ProfileChoice::named(name, ClaimedOs::Windows).unwrap_err(),
                ProfileError::Unsupported(name.to_owned()),
            );
        }
    }

    #[test]
    fn platforms_are_the_three_a_persona_can_claim() {
        assert_eq!(os_named("windows"), Ok(ClaimedOs::Windows));
        assert_eq!(os_named("macos"), Ok(ClaimedOs::Macos));
        assert_eq!(os_named("linux"), Ok(ClaimedOs::Linux));
        for name in ["android", "ios", "Windows", ""] {
            assert_eq!(
                os_named(name),
                Err(ProfileError::UnknownPlatform(name.to_owned()))
            );
        }
    }

    #[test]
    fn a_choice_names_itself() {
        let choice = ProfileChoice::named("firefox_151", ClaimedOs::Macos).unwrap();

        assert_eq!(choice.to_string(), "firefox_151 on macos");
        assert_eq!(choice.fixture_stem(), "firefox_151-macos");
        assert_eq!(
            format!("{choice:?}"),
            "ProfileChoice { name: \"firefox_151\", os: \"macos\", .. }"
        );
        assert_eq!(Family::Chrome.to_string(), "chrome");
        assert_eq!(Family::of_engine(Engine::Firefox), Family::Firefox);
        assert_eq!(
            Family::of_engine(Engine::Chromium).engine(),
            Engine::Chromium
        );
    }
}
