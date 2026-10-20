//! Runs the shipped signatures (`policies/block-signatures.yaml`) against
//! every recorded block page in `tests/fixtures/blocks/` (spec §12).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Observation, SignatureSet};
use crate::outcome::{BlockType, CooldownClass, Outcome, Vendor};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/blocks");
const SIGNATURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../policies/block-signatures.yaml"
);

/// One file under `tests/fixtures/blocks/`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// `true` when the page was written by hand from public knowledge of the
    /// vendor's markup, `false` when it is a sanitized recording.
    synthetic: bool,
    /// Where the page comes from or what it is modelled on.
    source: String,
    observation: Observation,
    expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    outcome: Outcome,
    /// Required for every outcome that carries a block report.
    #[serde(default)]
    vendor: Option<Vendor>,
    #[serde(default, rename = "type")]
    block_type: Option<BlockType>,
    /// The rule that must decide, when the fixture pins one.
    #[serde(default)]
    rule: Option<String>,
    /// The exact list of vendors seen, when the fixture pins it.
    #[serde(default)]
    vendors: Option<Vec<Vendor>>,
    /// What the verdict tells the scheduler, when the fixture pins it.
    #[serde(default)]
    retryable: Option<bool>,
    #[serde(default)]
    cooldown: Option<CooldownClass>,
}

fn signatures() -> SignatureSet {
    let text = std::fs::read_to_string(SIGNATURES).unwrap();
    SignatureSet::from_yaml(&text).unwrap()
}

fn fixture_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(FIXTURE_DIR)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    files.sort();
    files
}

fn name_of(path: &Path) -> String {
    path.file_stem().unwrap().to_str().unwrap().to_owned()
}

fn fixtures() -> BTreeMap<String, Fixture> {
    fixture_files()
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let fixture = crate::yaml::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            (name_of(&path), fixture)
        })
        .collect()
}

#[test]
fn every_fixture_classifies_as_expected() {
    let signatures = signatures();
    let mut failures = Vec::new();

    for (name, fixture) in fixtures() {
        let verdict = signatures.classify(&fixture.observation);
        let expect = &fixture.expect;
        let mut problems = Vec::new();

        if verdict.outcome != expect.outcome {
            problems.push(format!(
                "outcome {} (expected {})",
                verdict.outcome, expect.outcome
            ));
        }
        match &verdict.block {
            Some(block) => {
                if Some(&block.vendor) != expect.vendor.as_ref() {
                    problems.push(format!(
                        "vendor {} (expected {:?})",
                        block.vendor, expect.vendor
                    ));
                }
                if Some(&block.block_type) != expect.block_type.as_ref() {
                    problems.push(format!(
                        "type {} (expected {:?})",
                        block.block_type, expect.block_type
                    ));
                }
                if block.evidence.is_empty() {
                    problems.push("a block without evidence".to_owned());
                }
            }
            None => {
                if expect.vendor.is_some() || expect.block_type.is_some() {
                    problems.push("no block report, but the fixture expects one".to_owned());
                }
            }
        }
        if expect.rule.is_some() && verdict.rule != expect.rule {
            problems.push(format!(
                "rule {:?} (expected {:?})",
                verdict.rule, expect.rule
            ));
        }
        if expect
            .retryable
            .is_some_and(|retryable| retryable != verdict.retryable)
        {
            problems.push(format!("retryable {}", verdict.retryable));
        }
        if expect
            .cooldown
            .is_some_and(|cooldown| cooldown != verdict.cooldown)
        {
            problems.push(format!("cooldown {}", verdict.cooldown));
        }
        if let Some(vendors) = &expect.vendors
            && &verdict.vendors != vendors
        {
            problems.push(format!(
                "vendors {:?} (expected {vendors:?})",
                verdict.vendors
            ));
        }

        if !problems.is_empty() {
            failures.push(format!(
                "{name}: {} [rule {:?}]",
                problems.join("; "),
                verdict.rule
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} fixture(s) misclassified:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Outcomes that carry a block report must say which vendor and type they
/// expect, and good outcomes must not.
#[test]
fn fixtures_state_their_expectation_completely() {
    for (name, fixture) in fixtures() {
        let expect = &fixture.expect;
        let has_report = expect.vendor.is_some() && expect.block_type.is_some();
        let has_none = expect.vendor.is_none() && expect.block_type.is_none();
        if expect.outcome.is_success() {
            assert!(
                has_none,
                "{name}: a good outcome has no vendor or type to expect"
            );
        } else {
            assert!(
                has_report,
                "{name}: expect.vendor and expect.type are required"
            );
        }
        assert!(!fixture.source.trim().is_empty(), "{name}: source is empty");
        assert!(
            !fixture.observation.url.is_empty(),
            "{name}: the observation needs a URL"
        );
    }
}

/// Spec §12: the initial rule set must cover these vendors and block kinds,
/// and each needs at least one recorded page.
#[test]
fn every_vendor_and_block_kind_named_by_the_spec_has_a_fixture() {
    let covered: BTreeSet<(String, String)> = fixtures()
        .values()
        .filter_map(|fixture| {
            let expect = &fixture.expect;
            Some((
                expect.vendor.as_ref()?.to_string(),
                expect.block_type.as_ref()?.to_string(),
            ))
        })
        .collect();
    let vendors: BTreeSet<&str> = covered.iter().map(|(vendor, _)| vendor.as_str()).collect();

    for vendor in [
        "cloudflare",
        "akamai",
        "datadome",
        "human",
        "kasada",
        "imperva",
        "aws_waf",
        "f5_shape",
        "recaptcha",
        "hcaptcha",
        "generic",
    ] {
        assert!(vendors.contains(vendor), "no fixture for vendor {vendor}");
    }

    for (vendor, block_type) in [
        // Cloudflare: managed challenge, Turnstile, 403/503 "Just a moment".
        ("cloudflare", "managed_challenge"),
        ("cloudflare", "turnstile"),
        // Akamai: "Access Denied" reference pages.
        ("akamai", "access_denied"),
        // DataDome: captcha-delivery.com.
        ("datadome", "captcha"),
        // HUMAN/PerimeterX: "Press & Hold".
        ("human", "press_and_hold"),
        // Kasada: x-kpsdk-*, ips.js/p.js.
        ("kasada", "kpsdk_challenge"),
        // Imperva/Incapsula.
        ("imperva", "incident"),
        // AWS WAF: challenge.js.
        ("aws_waf", "challenge"),
        ("aws_waf", "captcha"),
        // reCAPTCHA and hCaptcha iframes.
        ("recaptcha", "recaptcha"),
        ("hcaptcha", "hcaptcha"),
        // Generic 403/429/5xx.
        ("generic", "http_403"),
        ("generic", "http_429"),
        ("generic", "server_error"),
        // Soft blocks: empty or boilerplate body, login wall, consent wall.
        ("generic", "empty_body"),
        ("generic", "js_required"),
        ("generic", "blank_page"),
        ("generic", "login_wall"),
        ("generic", "consent_wall"),
    ] {
        assert!(
            covered.contains(&(vendor.to_owned(), block_type.to_owned())),
            "no fixture for {vendor}/{block_type}"
        );
    }
}

/// What a verdict tells the scheduler, for the cases the planning decisions
/// name explicitly.
#[test]
fn verdicts_carry_the_right_scheduler_hints() {
    let signatures = signatures();
    let fixtures = fixtures();
    let hints = |name: &str| {
        let verdict = signatures.classify(&fixtures[name].observation);
        (verdict.retryable, verdict.cooldown)
    };

    // Another persona would get the same page: do not retry, cool nobody
    // down.
    for name in ["soft-js-required", "soft-login-wall", "soft-consent-wall"] {
        assert_eq!(hints(name), (false, CooldownClass::None), "{name}");
    }
    // A failing server: retry elsewhere, no cooldown.
    assert_eq!(hints("generic-503"), (true, CooldownClass::None));
    // A bare status with no marker: a real block, weakly attributed.
    assert_eq!(hints("generic-403"), (true, CooldownClass::Short));
    assert_eq!(hints("generic-429"), (true, CooldownClass::Short));
    // Vendor blocks keep what the spec says about their outcome.
    assert_eq!(
        hints("cloudflare-managed-challenge"),
        (true, CooldownClass::Long)
    );
    assert_eq!(hints("datadome-captcha"), (true, CooldownClass::Long));
    assert_eq!(hints("akamai-access-denied"), (true, CooldownClass::Long));
    assert_eq!(
        hints("cloudflare-rate-limit-1015"),
        (true, CooldownClass::Long)
    );
    assert_eq!(hints("soft-empty-body"), (true, CooldownClass::Long));
    // A fetched page has nothing to retry or cool down.
    for name in [
        "ok-cloudflare-site",
        "ok-404",
        "cloudflare-challenge-passed",
    ] {
        assert_eq!(hints(name), (false, CooldownClass::None), "{name}");
    }
}

/// The scheduler scales cooldowns by confidence, so the numbers have to mean
/// something: a vendor marker is at least 0.9, a bare status about 0.6.
#[test]
fn confidence_separates_vendor_markers_from_bare_statuses() {
    for rule in signatures().rules() {
        let confidence = rule.confidence;
        if rule.vendor.as_str() != Vendor::GENERIC {
            assert!(confidence >= 0.9_f32, "{}: {confidence}", rule.id);
            continue;
        }
        assert!(confidence < 0.9_f32, "{}: {confidence}", rule.id);
        match rule.block_type.as_str() {
            "http_403" | "http_429" => {
                assert!(
                    (0.55_f32..=0.65).contains(&confidence),
                    "{}: {confidence}",
                    rule.id
                );
                assert_eq!(rule.cooldown, CooldownClass::Short, "{}", rule.id);
            }
            "server_error" => assert_eq!(rule.cooldown, CooldownClass::None),
            _ => {}
        }
    }
}

/// Every verdict says which markers fired, good pages included, so a field
/// report can show why a page was or was not treated as a block.
#[test]
fn every_verdict_carries_its_evidence() {
    let signatures = signatures();
    let fixtures = fixtures();

    for (name, fixture) in &fixtures {
        let verdict = signatures.classify(&fixture.observation);
        if let Some(block) = &verdict.block {
            assert!(!block.evidence.is_empty(), "{name}");
            // The block's own evidence comes first in the verdict's.
            assert_eq!(
                verdict.evidence[..block.evidence.len()],
                block.evidence,
                "{name}"
            );
        }
        // Whenever a vendor was seen, the marker that showed it is recorded.
        if !verdict.vendors.is_empty() {
            assert!(
                !verdict.evidence.is_empty(),
                "{name}: vendors without evidence"
            );
        }
    }

    let evidence_of = |name: &str| signatures.classify(&fixtures[name].observation).evidence;
    let good = evidence_of("ok-cloudflare-site");
    for marker in [
        "cookie:__cf_bm",
        "header:cf-ray=4b33cc22dd11aa00-FRA",
        "header:server=cloudflare",
    ] {
        assert!(
            good.contains(&marker.to_owned()),
            "{marker} missing from {good:?}"
        );
    }
    assert!(evidence_of("ok-akamai-site").contains(&"cookie:_abck".to_owned()));
    // A page with no marker at all has no evidence, and says so by being empty.
    assert_eq!(evidence_of("ok-short-page"), Vec::<String>::new());
    // A blocked page lists the hints after the block's own evidence.
    let blocked = evidence_of("cloudflare-managed-challenge");
    assert_eq!(blocked[0], "header:cf-mitigated=challenge");
    assert!(
        blocked.contains(&"cookie:__cf_bm".to_owned()),
        "{blocked:?}"
    );
}

/// A rule nobody tests is a rule nobody knows still works.
#[test]
fn every_rule_decides_at_least_one_fixture() {
    let signatures = signatures();
    let deciding: BTreeSet<String> = fixtures()
        .values()
        .filter_map(|fixture| signatures.classify(&fixture.observation).rule)
        .collect();

    let untested: Vec<String> = signatures
        .rules()
        .into_iter()
        .map(|rule| rule.id)
        .filter(|id| !deciding.contains(id))
        .collect();

    assert_eq!(untested, Vec::<String>::new(), "rules without a fixture");
}

/// Protected sites set their vendor's cookies and scripts on good pages too.
/// Those pages must stay `ok`, with the vendor only recorded as a hint.
#[test]
fn good_pages_with_vendor_markers_stay_ok() {
    let signatures = signatures();
    let mut hinted_vendors = BTreeSet::new();

    for (name, fixture) in fixtures() {
        if fixture.expect.outcome != Outcome::Ok {
            continue;
        }
        let verdict = signatures.classify(&fixture.observation);
        assert_eq!(verdict.outcome, Outcome::Ok, "{name}");
        assert_eq!(verdict.block, None, "{name}");
        hinted_vendors.extend(verdict.vendors.iter().map(ToString::to_string));
    }

    for vendor in [
        "cloudflare",
        "akamai",
        "datadome",
        "human",
        "imperva",
        "recaptcha",
    ] {
        assert!(
            hinted_vendors.contains(vendor),
            "no good-page fixture carries {vendor} markers"
        );
    }
}

/// The README beside the fixtures lists every one of them and says which are
/// synthetic.
#[test]
fn the_readme_lists_every_fixture_and_marks_synthetic_ones() {
    let readme = std::fs::read_to_string(Path::new(FIXTURE_DIR).join("README.md")).unwrap();

    for (name, fixture) in fixtures() {
        let row = readme
            .lines()
            .find(|line| line.starts_with(&format!("| `{name}.yaml` |")))
            .unwrap_or_else(|| panic!("README.md does not list {name}.yaml"));
        let marked_synthetic = row.contains("synthetic");
        assert_eq!(
            marked_synthetic, fixture.synthetic,
            "{name}: README.md and the fixture disagree on whether it is synthetic"
        );
    }
}
