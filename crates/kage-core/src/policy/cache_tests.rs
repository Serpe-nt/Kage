//! The policy's `cache` section and the default of `robots` (spec §14,
//! §15).

use std::time::Duration;

use super::{CachePolicy, DomainPolicies, PolicyError, RobotsMode};
use crate::duration::HumanDuration;

const DAY: Duration = Duration::from_hours(24);

#[test]
fn robots_txt_is_not_asked_for_unless_a_policy_says_so() {
    let nothing = DomainPolicies::default().resolve("example.org");
    assert_eq!(nothing.robots, RobotsMode::Ignore);
    let policies = DomainPolicies::from_yaml(
        "defaults: {}\ndomains:\n  example.org: { robots: enforce }\n  report.example: { robots: report }\n",
    )
    .unwrap();
    assert_eq!(
        policies.resolve("www.example.org").robots,
        RobotsMode::Enforce
    );
    assert_eq!(
        policies.resolve("report.example").robots,
        RobotsMode::Report
    );
    assert_eq!(policies.resolve("other.example").robots, RobotsMode::Ignore);
    // The defaults block may say it for every site.
    let all = DomainPolicies::from_yaml("defaults: { robots: report }\n").unwrap();
    assert_eq!(all.resolve("any.example").robots, RobotsMode::Report);
}

#[test]
fn the_cache_of_a_site_is_its_keys_alone_unless_the_policy_shares_it() {
    let nothing = DomainPolicies::default().resolve("example.org");
    assert_eq!(nothing.cache, CachePolicy::default());
    assert!(!nothing.cache.shared);
    assert_eq!((nothing.cache.max_ttl, nothing.cache.ttl), (None, None));

    // Every field cascades by itself: the nearest entry that sets it.
    let policies = DomainPolicies::from_yaml(
        "defaults:\n  cache: { max_ttl: 2h }\ndomains:\n  example.org:\n    cache: { shared: true, ttl: 30m }\n  docs.example.org:\n    cache: { ttl: 10m }\n  news.example.org:\n    cache: { shared: false, max_ttl: 5m }\n",
    )
    .unwrap();
    let site = policies.resolve("example.org").cache;
    assert!(site.shared);
    assert_eq!(site.max_ttl, Some(HumanDuration::from_hours(2)));
    assert_eq!(site.ttl, Some(HumanDuration::from_mins(30)));
    let docs = policies.resolve("docs.example.org").cache;
    assert!(docs.shared);
    assert_eq!(docs.ttl, Some(HumanDuration::from_mins(10)));
    assert_eq!(docs.max_ttl, Some(HumanDuration::from_hours(2)));
    let news = policies.resolve("news.example.org").cache;
    assert!(!news.shared);
    assert_eq!(news.max_ttl, Some(HumanDuration::from_mins(5)));
    assert_eq!(news.ttl, Some(HumanDuration::from_mins(30)));
    let other = policies.resolve("other.example").cache;
    assert!(!other.shared);
    assert_eq!(other.max_ttl, Some(HumanDuration::from_hours(2)));
    assert_eq!(other.ttl, None);
}

#[test]
fn a_cache_section_out_of_range_is_named() {
    for (written, field) in [
        ("cache: { max_ttl: 0s }", "cache.max_ttl"),
        ("cache: { ttl: 0s }", "cache.ttl"),
        ("cache: { ttl: 2h, max_ttl: 1h }", "cache.ttl"),
    ] {
        let error =
            DomainPolicies::from_yaml(&format!("domains:\n  example.org:\n    {written}\n"))
                .unwrap_err();
        assert!(
            matches!(&error, PolicyError::InvalidField { field: named, .. } if *named == field),
            "{written}: {error}"
        );
    }
    assert!(
        DomainPolicies::from_yaml("domains:\n  example.org:\n    cache: { shard: true }\n")
            .is_err()
    );
    // At its bound a default is in range.
    assert!(
        DomainPolicies::from_yaml(
            "domains:\n  example.org:\n    cache: { ttl: 1h, max_ttl: 1h }\n"
        )
        .is_ok()
    );
}

#[test]
fn how_long_a_request_is_served_from_the_cache() {
    let secs = Duration::from_secs;
    let open = CachePolicy::default();
    // Nothing asked and no default: never.
    assert_eq!(open.ttl_for(None, DAY), Ok(None));
    // What a request asks for, up to the bound and no further.
    assert_eq!(open.ttl_for(Some(secs(60)), DAY), Ok(Some(secs(60))));
    assert_eq!(open.ttl_for(Some(DAY), DAY), Ok(Some(DAY)));
    assert_eq!(open.ttl_for(Some(DAY + secs(1)), DAY), Err(DAY));
    // Zero is "never from the cache", whatever the policy's default.
    let with_default = CachePolicy {
        ttl: Some(HumanDuration::from_mins(30)),
        ..CachePolicy::default()
    };
    assert_eq!(with_default.ttl_for(Some(Duration::ZERO), DAY), Ok(None));
    assert_eq!(with_default.ttl_for(None, DAY), Ok(Some(secs(1800))));
    assert_eq!(with_default.ttl_for(Some(secs(5)), DAY), Ok(Some(secs(5))));
    // A default beyond the configuration's bound is the bound.
    assert_eq!(with_default.ttl_for(None, secs(600)), Ok(Some(secs(600))));
    // The site's own bound, where it is the shorter one.
    let short = CachePolicy {
        max_ttl: Some(HumanDuration::from_mins(5)),
        ..CachePolicy::default()
    };
    assert_eq!(short.longest(DAY), secs(300));
    assert_eq!(short.longest(secs(60)), secs(60));
    assert_eq!(short.ttl_for(Some(secs(300)), DAY), Ok(Some(secs(300))));
    assert_eq!(short.ttl_for(Some(secs(301)), DAY), Err(secs(300)));
    assert_eq!(open.longest(DAY), DAY);
}
