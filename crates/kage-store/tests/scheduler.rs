//! What the scheduler keeps in the store (M7): the breaker under its row
//! lock, the bytes of the forwarder, and the question "was this already
//! raised in this period".

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{TestDb, now};
use kage_core::alert::AlertSeverity;
use kage_core::domain::{
    BandwidthUsage, BreakerState, DomainBreaker, Exit, HealthEvent, IDLE_DOMAIN, SubjectKind,
    hour_of,
};
use kage_core::ids::{HealthEventId, PersonaId};
use kage_core::schedule::breaker::{Admission, BreakerChange, BreakerRules};
use kage_store::repo::Traffic;

fn rules() -> BreakerRules {
    BreakerRules {
        threshold: 5,
        pause: StdDuration::from_mins(30),
        max_pause: StdDuration::from_hours(6),
        trial_timeout: StdDuration::from_mins(10),
    }
}

#[tokio::test]
async fn a_breaker_keeps_its_trips_and_its_trial() {
    let db = TestDb::migrated().await;
    let breakers = db.store.domain_breakers();
    let at = now();

    let mut breaker = DomainBreaker::closed("example.org", at);
    breaker.state = BreakerState::Open;
    breaker.paused_until = Some(at + Duration::hours(2));
    breaker.trips = 3;
    breaker.trial_started_at = Some(at + Duration::minutes(1));
    breakers.upsert(&breaker).await.unwrap();

    assert_eq!(
        breakers.get("example.org").await.unwrap(),
        Some(breaker.clone())
    );
    assert_eq!(breakers.list_open().await.unwrap(), [breaker]);
}

#[tokio::test]
async fn blocks_counted_at_the_same_moment_are_all_counted_and_one_request_is_the_trial() {
    let db = TestDb::migrated().await;
    let at = now();
    let rules = rules();

    // A domain without a breaker gets a closed one, as of the moment asked.
    let fresh = db
        .store
        .domain_breakers()
        .modify("example.org", at, |breaker| breaker.clone())
        .await
        .unwrap();
    assert_eq!(fresh, DomainBreaker::closed("example.org", at));

    // Twelve blocks at once: every one is counted, and exactly one of them
    // is told that it opened the breaker.
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let store = db.store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .domain_breakers()
                .modify("example.org", at, |breaker| {
                    breaker.record_block(at, &rules)
                })
                .await
                .unwrap()
        }));
    }
    let mut opened = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), BreakerChange::Opened { .. }) {
            opened += 1;
        }
    }
    assert_eq!(opened, 1);
    let stored = db
        .store
        .domain_breakers()
        .get("example.org")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.block_count_1h, 12);
    assert_eq!(stored.state, BreakerState::Open);
    assert_eq!(stored.trips, 1);
    assert_eq!(stored.paused_until, Some(at + Duration::minutes(30)));

    // The pause is over and eight requests ask at once: one is the trial.
    let later = at + Duration::minutes(31);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let store = db.store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .domain_breakers()
                .modify("example.org", later, |breaker| {
                    let admission = breaker.admission(later, &rules);
                    if admission == Admission::Trial {
                        breaker.begin_trial(later);
                    }
                    admission
                })
                .await
                .unwrap()
        }));
    }
    let mut trials = 0;
    for task in tasks {
        match task.await.unwrap() {
            Admission::Trial => trials += 1,
            Admission::Paused { until } => {
                assert_eq!(until, Some(later + Duration::minutes(10)));
            }
            Admission::Flow => panic!("an open breaker let a request flow"),
        }
    }
    assert_eq!(trials, 1);

    // The trial succeeds: closed, as new.
    let change = db
        .store
        .domain_breakers()
        .modify("example.org", later, |breaker| {
            breaker.record_success(later)
        })
        .await
        .unwrap();
    assert_eq!(change, BreakerChange::Closed);
    assert_eq!(
        db.store.domain_breakers().get("example.org").await.unwrap(),
        Some(DomainBreaker::closed("example.org", later))
    );
    assert_eq!(db.store.domain_breakers().list_open().await.unwrap(), []);
}

fn hour(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
}

fn usage(
    at: DateTime<Utc>,
    persona: PersonaId,
    domain: &str,
    up: u64,
    down: u64,
) -> BandwidthUsage {
    BandwidthUsage {
        hour: at,
        persona_id: Some(persona),
        egress: Exit::Pool,
        domain: domain.to_owned(),
        bytes_up: up,
        bytes_down: down,
    }
}

#[tokio::test]
async fn bytes_add_up_per_hour_persona_and_domain() {
    let db = TestDb::migrated().await;
    let bandwidth = db.store.bandwidth();
    let first = common::seed_persona(&db.store).await;
    let second = common::seed_persona(&db.store).await;

    assert_eq!(
        bandwidth.total_since(hour(1, 0), Exit::Pool).await.unwrap(),
        Traffic::default()
    );
    bandwidth.add(&[]).await.unwrap();

    bandwidth
        .add(&[
            usage(hour(10, 9), first.id, "example.org", 1_000, 90_000),
            usage(hour(10, 9), first.id, IDLE_DOMAIN, 200, 300),
            usage(hour(10, 9), second.id, "example.org", 50, 5_000),
        ])
        .await
        .unwrap();
    // The same hour, persona and domain again: added, not replaced.
    bandwidth
        .add(&[
            usage(hour(10, 9), first.id, "example.org", 500, 10_000),
            usage(hour(10, 10), first.id, "example.net", 7, 70),
            usage(hour(3, 23), second.id, "old.example", 1, 1),
        ])
        .await
        .unwrap();

    // Everything from the 10th on; the row of the 3rd is before it.
    assert_eq!(
        bandwidth
            .total_since(hour(10, 0), Exit::Pool)
            .await
            .unwrap(),
        Traffic {
            bytes_up: 1_757,
            bytes_down: 105_370,
        }
    );
    assert_eq!(
        bandwidth
            .total_since(hour(1, 0), Exit::Pool)
            .await
            .unwrap()
            .total(),
        107_129
    );
    // From an hour on means that hour included.
    assert_eq!(
        bandwidth
            .total_since(hour(10, 10), Exit::Pool)
            .await
            .unwrap()
            .total(),
        77
    );
    assert_eq!(
        bandwidth
            .total_since(hour(10, 11), Exit::Pool)
            .await
            .unwrap(),
        Traffic::default()
    );

    assert_eq!(
        bandwidth.by_persona_since(hour(10, 0)).await.unwrap(),
        [
            (
                first.id,
                Traffic {
                    bytes_up: 1_707,
                    bytes_down: 100_370
                }
            ),
            (
                second.id,
                Traffic {
                    bytes_up: 50,
                    bytes_down: 5_000
                }
            ),
        ]
    );
    let by_domain = bandwidth.by_domain_since(hour(1, 0), 10).await.unwrap();
    let names: Vec<&str> = by_domain
        .iter()
        .map(|(domain, _)| domain.as_str())
        .collect();
    assert_eq!(
        names,
        ["example.org", IDLE_DOMAIN, "example.net", "old.example"]
    );
    assert_eq!(by_domain[0].1.total(), 106_550);
    assert_eq!(
        bandwidth
            .by_domain_since(hour(1, 0), 2)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(bandwidth.by_persona_since(hour(11, 0)).await.unwrap(), []);
}

#[tokio::test]
async fn bytes_are_only_counted_for_a_real_persona_and_a_whole_hour() {
    let db = TestDb::migrated().await;
    let bandwidth = db.store.bandwidth();
    let persona = common::seed_persona(&db.store).await;

    // Nothing of a batch is stored when one row of it is refused.
    let nobody = bandwidth
        .add(&[
            usage(hour(10, 9), persona.id, "example.org", 1, 1),
            usage(hour(10, 9), PersonaId::new(), "example.org", 1, 1),
        ])
        .await;
    assert!(nobody.is_err());
    let mid_hour = bandwidth
        .add(&[usage(
            hour(10, 9) + Duration::minutes(5),
            persona.id,
            "example.org",
            1,
            1,
        )])
        .await;
    assert!(mid_hour.is_err());
    assert_eq!(
        bandwidth.total_since(hour(1, 0), Exit::Pool).await.unwrap(),
        Traffic::default()
    );

    // `hour_of` gives the hour a moment belongs to.
    let moment = hour(10, 9) + Duration::minutes(59);
    bandwidth
        .add(&[usage(hour_of(moment), persona.id, "example.org", 2, 3)])
        .await
        .unwrap();
    assert_eq!(
        bandwidth
            .total_since(hour(10, 9), Exit::Pool)
            .await
            .unwrap()
            .total(),
        5
    );
    // A count no column holds is refused before the query.
    let too_much = bandwidth
        .add(&[usage(hour(10, 9), persona.id, "example.org", u64::MAX, 0)])
        .await;
    assert!(too_much.is_err());
}

/// A plain client's bytes belong to no persona, and the host's own bytes
/// are not the provider's: each is added up apart (spec §25).
#[tokio::test]
async fn a_plain_clients_bytes_and_the_hosts_are_counted_apart() {
    let db = TestDb::migrated().await;
    let bandwidth = db.store.bandwidth();
    let persona = common::seed_persona(&db.store).await;
    let plain = |egress, up, down| BandwidthUsage {
        hour: hour(10, 9),
        persona_id: None,
        egress,
        domain: "example.org".to_owned(),
        bytes_up: up,
        bytes_down: down,
    };
    bandwidth
        .add(&[
            usage(hour(10, 9), persona.id, "example.org", 10, 100),
            plain(Exit::Pool, 1, 2),
            plain(Exit::Host, 1_000, 2_000),
            BandwidthUsage {
                egress: Exit::Host,
                ..usage(hour(10, 9), persona.id, "example.org", 5, 50)
            },
        ])
        .await
        .unwrap();
    // Again: added to the rows of no persona as to a persona's.
    bandwidth
        .add(&[plain(Exit::Pool, 1, 2), plain(Exit::Host, 1, 1)])
        .await
        .unwrap();

    // The pool's: the persona's and the plain client's through it.
    assert_eq!(
        bandwidth
            .total_since(hour(10, 0), Exit::Pool)
            .await
            .unwrap(),
        Traffic {
            bytes_up: 12,
            bytes_down: 104,
        }
    );
    // The host's: never the provider's.
    assert_eq!(
        bandwidth
            .total_since(hour(10, 0), Exit::Host)
            .await
            .unwrap(),
        Traffic {
            bytes_up: 1_006,
            bytes_down: 2_051,
        }
    );
    // Per persona: a plain client is nobody; one persona's two egresses.
    assert_eq!(
        bandwidth.by_persona_since(hour(10, 0)).await.unwrap(),
        [(
            persona.id,
            Traffic {
                bytes_up: 15,
                bytes_down: 150
            }
        )]
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM bandwidth_usage")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 4);
}

#[tokio::test]
async fn an_event_of_a_kind_is_found_from_a_moment_on() {
    let db = TestDb::migrated().await;
    let events = db.store.health_events();
    let at = now();
    events
        .insert(&HealthEvent {
            id: HealthEventId::new(),
            at,
            subject_kind: SubjectKind::System,
            subject_id: None,
            kind: "bandwidth_budget_warning".to_owned(),
            severity: AlertSeverity::Warning,
            details: serde_json::json!({ "share": 0.81 }),
        })
        .await
        .unwrap();

    assert!(
        events
            .exists_since("bandwidth_budget_warning", at - Duration::days(1))
            .await
            .unwrap()
    );
    // At the moment itself counts; a moment later it is before the period.
    assert!(
        events
            .exists_since("bandwidth_budget_warning", at)
            .await
            .unwrap()
    );
    assert!(
        !events
            .exists_since("bandwidth_budget_warning", at + Duration::microseconds(1))
            .await
            .unwrap()
    );
    assert!(
        !events
            .exists_since("bandwidth_budget_exhausted", at - Duration::days(1))
            .await
            .unwrap()
    );
}
