//! One fetch through the scheduler and the pipeline, with a scripted
//! executor in place of the two paths, on a paused clock: the path of each
//! level, retries with a persona switch, a plain client's exit, cooldowns,
//! the politeness pause, the breaker, the stops, and the hook for a
//! synchronous fetch that takes too long (spec §3.2, §8, §11, §12, §25).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use chrono::Duration as Span;
use common::{DEFAULT_DOMAINS, World, blocked, id, ok, persona, server_error, timed_out};
use kage_core::alert::AlertKind;
use kage_core::domain::{BreakerState, Exit, Level, Render, RenderMode};
use kage_core::ids::ProxyId;
use kage_core::outcome::Outcome;
use kage_core::pacing::{Distribution, PacingProfile, Uniform};
use kage_core::policy::PacingRequest;
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use kage_core::schedule::selection::Network;
use kage_scheduler::pipeline::attempt_seed;
use kage_scheduler::{
    AttemptOutput, FetchOrder, JobFetch, PlainRoute, SchedulerError, Synchronous,
};
use url::Url;

/// A host without a pool: a plain client leaves from its own connection.
const DIRECT: &[(&str, &str)] = &[("KAGE__EGRESS__MODE", "direct")];

fn get(url: &str) -> FetchOrder {
    FetchOrder::get(Url::parse(url).unwrap(), Priority::Sync)
}

fn browser(url: &str) -> FetchOrder {
    at(Level::Human, url)
}

fn at(level: Level, url: &str) -> FetchOrder {
    FetchOrder { level, ..get(url) }
}

fn span(duration: Duration) -> Span {
    Span::from_std(duration).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_page_is_fetched_by_one_persona_and_everything_about_it_is_stored() {
    let mut world = World::new(4, DEFAULT_DOMAINS);

    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.reason, None);
    assert_eq!(report.block, None);
    assert_eq!(report.pacing.applied, "human");
    assert!(!report.pacing.request_ignored);
    // Level 3, the default of an order: the browser, as a person.
    assert_eq!(world.script.renders(), [Render::Browser]);
    let asked = world.script.seen();
    let job = &asked[0].job;
    assert_eq!(job.level, Level::Human);
    assert_eq!(job.n, 1);
    assert_eq!(job.domain, "shop.example");
    assert!(job.starts_visit && job.keeps_visiting);
    assert_eq!(job.timeout, world.config.browser.timeout.get());

    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(attempt.n, 1);
    assert_eq!(attempt.persona_id, Some(job.persona.id));
    assert_eq!(attempt.engine, Some(job.persona.engine));
    assert_eq!(attempt.exit, None);
    assert_eq!(attempt.render, Render::Browser);
    assert_eq!(attempt.outcome, Outcome::Ok);
    assert_eq!(attempt.status, Some(200));
    assert_eq!(
        attempt.finished_at - attempt.started_at,
        Span::milliseconds(800)
    );
    // The seed of the attempt is the persona's, the request's and the
    // attempt's number: stored, so the attempt can be replayed.
    let seed = attempt_seed(&job.persona.seed, report.request, 1);
    assert_eq!(attempt.seed, Some(seed));
    assert_eq!(job.seed, seed);
    assert_ne!(seed, attempt_seed(&job.persona.seed, report.request, 2));

    assert_eq!(report.persona.as_ref().unwrap().id, job.persona.id);
    let requests = world.backend.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, report.request);
    assert_eq!(requests[0].domain, "shop.example");
    assert_eq!(requests[0].level, Some(Level::Human));
    assert_eq!(requests[0].render, RenderMode::Browser);
    assert_eq!(requests[0].outcome, Some(Outcome::Ok));
    assert!(requests[0].finished_at.is_some());
    assert_eq!(world.backend.attempts(), report.attempts);
    let records = world.backend.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, report.result);
    assert_eq!(records[0].status, Some(200));
    // The persona's page is counted on its own calendar and remembered.
    let state = world
        .backend
        .domain_state(job.persona.id, "shop.example")
        .unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_outcome, Some(Outcome::Ok));
    assert_eq!(state.cooldown_until, None);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 0);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn three_blocked_attempts_are_made_by_three_personas() {
    let mut world = World::new(6, DEFAULT_DOMAINS);
    for _ in 0..3 {
        world.script.then(blocked(Outcome::Forbidden, 1.0));
    }

    let report = world
        .pipeline
        .fetch(browser("https://shop.example/a"))
        .await
        .unwrap();

    // The policy's two retries: three attempts, each by another persona.
    assert_eq!(report.outcome, Outcome::Forbidden);
    assert_eq!(report.attempts.len(), 3);
    assert_eq!(
        report
            .attempts
            .iter()
            .map(|attempt| attempt.n)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let personas = world.script.personas();
    assert_eq!(personas.iter().collect::<BTreeSet<_>>().len(), 3);
    assert_eq!(report.persona.as_ref().unwrap().id, personas[2]);
    let block = report.block.as_ref().unwrap();
    assert_eq!(block.vendor.as_str(), "cloudflare");
    assert_eq!(world.backend.records()[0].block, report.block);

    // Each of them answers for its block: it stays away from the domain,
    // and the domain's breaker counted three.
    for persona in &personas {
        let state = world
            .backend
            .domain_state(*persona, "shop.example")
            .unwrap();
        assert!(
            state
                .cooldown_until
                .is_some_and(|until| until > world.now())
        );
        assert_eq!(state.last_outcome, Some(Outcome::Forbidden));
        assert_eq!(state.block_count_24h, 1);
    }
    let breaker = world.backend.breaker("shop.example").unwrap();
    assert_eq!(breaker.block_count_1h, 3);
    assert_eq!(breaker.state, BreakerState::Closed);

    // A retry does not leave the moment the block arrived: the next
    // persona comes after a pause of its own, never under 1.6 seconds
    // (the shortest pause at the pace of the fastest persona).
    let seen = world.script.seen();
    for (before, after) in report.attempts.iter().zip(&seen[1..]) {
        let pause = after.at - before.finished_at;
        assert!(pause >= Span::milliseconds(1600), "{pause}");
        assert!(pause < Span::minutes(5), "{pause}");
    }
    // The pause counts as time the request waited.
    assert!(report.pacing.waited >= Duration::from_millis(3200));
    world.shut_down().await;
}

/// QA round two's pool-01 world (F168): five personas whose exits lie in
/// three /24s and four autonomous systems.
fn five_exits() -> Vec<Network> {
    [
        ([203, 0, 113, 10], "AS64501"),
        ([203, 0, 113, 20], "AS64501"),
        ([198, 51, 100, 30], "AS64502"),
        ([192, 0, 2, 40], "AS64501"),
        ([198, 51, 100, 50], "AS64503"),
    ]
    .iter()
    .map(|(octets, asn)| Network::of(Some(IpAddr::V4(Ipv4Addr::from(*octets))), Some(asn)))
    .collect()
}

#[tokio::test(start_paused = true)]
async fn after_a_block_no_retry_leaves_from_its_subnet_or_its_system_while_another_is_untried() {
    // F168 (pool-01): the retries of a blocked request went to whichever
    // persona's own pause ended first, in the /24 of the block or its
    // autonomous system while personas of others were untried.
    let networks = five_exits();
    for round in 0..8 {
        let mut world = World::with(5, false, DEFAULT_DOMAINS, &[]);
        for (number, network) in (0_u8..).zip(&networks) {
            world
                .backend
                .put_persona(persona(number, false), true, network.clone());
        }
        for _ in 0..3 {
            world.script.then(blocked(Outcome::Forbidden, 1.0));
        }
        let report = world
            .pipeline
            .fetch(browser(&format!("https://pool{round}.example/denied")))
            .await
            .unwrap();
        assert_eq!(report.attempts.len(), 3);
        let tried: Vec<usize> = world
            .script
            .personas()
            .iter()
            .map(|persona| (0_u8..5).position(|number| id(number) == *persona).unwrap())
            .collect();
        for (n, taker) in tried.iter().enumerate().skip(1) {
            let before = &tried[..n];
            let theirs = &networks[*taker];
            assert!(
                before
                    .iter()
                    .all(|blocked| !networks[*blocked].shares_subnet(theirs)),
                "round {round}: {tried:?}, attempt {} in the /24 of a block",
                n + 1
            );
            // Another /24 and another system, untried: then not this one's
            // system either.
            let better = (0..5).any(|other| {
                !tried[..=n].contains(&other)
                    && before.iter().all(|blocked| {
                        !networks[*blocked].shares_subnet(&networks[other])
                            && !networks[*blocked].shares_asn(&networks[other])
                    })
            });
            if better {
                assert!(
                    before
                        .iter()
                        .all(|blocked| !networks[*blocked].shares_asn(theirs)),
                    "round {round}: {tried:?}, attempt {} in the system of a block",
                    n + 1
                );
            }
        }
        world.shut_down().await;
    }
}

#[tokio::test(start_paused = true)]
async fn each_level_is_one_path_and_a_block_never_moves_a_request_up() {
    let mut world = World::new(6, DEFAULT_DOMAINS);
    for _ in 0..2 {
        world.script.then(blocked(Outcome::ChallengeFailed, 0.9));
    }

    let report = world
        .pipeline
        .fetch(at(Level::Emulated, "https://shop.example/a"))
        .await
        .unwrap();

    // Level 1 is the http path, however the site answers: the block is
    // another persona's turn on it, after a pause.
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(world.script.renders(), [Render::Http; 3]);
    let seen = world.script.seen();
    assert!(seen.iter().all(|seen| seen.job.level == Level::Emulated));
    assert_eq!(seen[0].job.timeout, world.config.http.timeout.get());
    let personas = world.script.personas();
    assert_eq!(personas.iter().collect::<BTreeSet<_>>().len(), 3);
    let pause = seen[1].at - report.attempts[0].finished_at;
    assert!(pause >= Span::milliseconds(1600), "{pause}");
    // Each blocked persona answers for its block at once.
    for persona in &personas[..2] {
        let state = world
            .backend
            .domain_state(*persona, "shop.example")
            .unwrap();
        assert_eq!(state.last_outcome, Some(Outcome::ChallengeFailed));
        assert!(state.cooldown_until.is_some());
    }
    assert_eq!(
        world
            .backend
            .breaker("shop.example")
            .unwrap()
            .block_count_1h,
        2
    );
    assert_eq!(world.backend.requests()[0].level, Some(Level::Emulated));
    assert_eq!(world.backend.requests()[0].render, RenderMode::Http);

    // Level 2 and 3 are the browser.
    for level in [Level::Fast, Level::Human] {
        let report = world
            .pipeline
            .fetch(at(level, "https://shop.example/b"))
            .await
            .unwrap();
        assert_eq!(report.outcome, Outcome::Ok);
        let seen = world.script.seen();
        let last = &seen.last().unwrap().job;
        assert_eq!((last.level, last.render), (level, Render::Browser));
        assert_eq!(last.timeout, world.config.browser.timeout.get());
    }
    assert!(world.script.plain_seen().is_empty());
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_level_zero_fetch_is_nobodys_and_leaves_from_the_hosts_connection() {
    let mut world = World::with(4, true, DEFAULT_DOMAINS, DIRECT);

    let report = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.persona, None);
    // No persona was asked for anything.
    assert!(world.script.seen().is_empty());
    let [seen] = world.script.plain_seen().try_into().unwrap();
    assert_eq!(seen.job.route, PlainRoute::Host);
    assert_eq!(seen.job.url.as_str(), "https://shop.example/a");
    assert_eq!(seen.job.timeout, world.config.http.timeout.get());
    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(attempt.persona_id, None);
    assert_eq!(attempt.engine, None);
    assert_eq!(attempt.exit, Some(Exit::Host));
    assert_eq!(attempt.render, Render::Http);
    assert_eq!(attempt.seed, None);
    assert_eq!(world.backend.attempts(), report.attempts);
    let requests = world.backend.requests();
    assert_eq!(requests[0].level, Some(Level::Plain));
    assert_eq!(requests[0].render, RenderMode::Http);
    // Nobody's calendar counted the page; the site has no scout for it.
    for number in 0..4 {
        assert_eq!(world.backend.domain_state(id(number), "shop.example"), None);
    }
    assert_eq!(world.backend.scout_of("shop.example"), None);
    assert_eq!(world.scheduler.in_flight(Render::Http), 0);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_level_zero_block_is_the_answer_and_nobody_answers_for_it() {
    let mut world = World::with(4, true, DEFAULT_DOMAINS, DIRECT);
    world.script.then_plain(blocked(Outcome::Forbidden, 1.0));

    let report = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/a"))
        .await
        .unwrap();

    // The policy's retries do not apply to a block at level 0.
    assert_eq!(report.outcome, Outcome::Forbidden);
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(world.script.plain_seen().len(), 1);
    assert!(report.block.is_some());
    // No persona cools and the site's breaker counts nothing.
    assert_eq!(world.backend.breaker("shop.example"), None);
    for number in 0..4 {
        assert_eq!(world.backend.domain_state(id(number), "shop.example"), None);
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_level_zero_failure_is_tried_again_from_the_same_exit_after_a_pause() {
    let mut world = World::new(4, DEFAULT_DOMAINS);
    world.script.then_plain(timed_out());

    let report = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok);
    let seen = world.script.plain_seen();
    assert_eq!(seen.len(), 2);
    // Under the pool, the site's plain exit is its scout's proxy: chosen
    // for the first attempt, the same for the second.
    let scout = world.backend.scout_of("shop.example").unwrap();
    let number = (0..4).find(|number| id(*number) == scout).unwrap();
    let route = PlainRoute::Pool(persona(number, true).proxy_id);
    assert_eq!(seen[0].job.route, route);
    assert_eq!(seen[1].job.route, route);
    assert!(
        report
            .attempts
            .iter()
            .all(|attempt| attempt.exit == Some(Exit::Pool) && attempt.persona_id.is_none())
    );
    let pause = seen[1].at - report.attempts[0].finished_at;
    assert!(pause >= Span::milliseconds(1600), "{pause}");
    // The scout itself read nothing.
    assert!(world.script.seen().is_empty());
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_level_zero_fetch_without_a_plain_exit_is_answered_with_the_reason() {
    // Nobody may scout the site, so the pool has no exit for it.
    let mut world = World::new(0, DEFAULT_DOMAINS);

    let report = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("plain_exit_unavailable"));
    assert!(report.retry_at.is_some());
    assert_eq!(report.attempts, []);
    assert!(world.script.plain_seen().is_empty());
    world.shut_down().await;
}

/// ADR 0013, consult 5a: a declared entry without a country carries no
/// persona and is the pool's own plain exit. A site's level 0 leaves by one
/// of them, chosen once and kept, before any scout's address; when it is
/// down the request waits, never from another address.
#[tokio::test(start_paused = true)]
async fn level_zero_leaves_by_a_plain_exit_of_the_pools_own_and_keeps_it() {
    let mut world = World::new(4, DEFAULT_DOMAINS);
    let exits = [ProxyId::new(), ProxyId::new(), ProxyId::new()];
    for exit in exits {
        world.backend.put_plain_exit(exit, true);
    }

    let report = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok);
    let chosen = world.backend.site_exit_of("shop.example").unwrap();
    assert!(exits.contains(&chosen));
    let [seen] = world.script.plain_seen().try_into().unwrap();
    assert_eq!(seen.job.route, PlainRoute::Pool(chosen));
    assert_eq!(report.attempts[0].exit, Some(Exit::Pool));
    // Nobody was made the site's scout, and no persona was asked anything.
    assert_eq!(world.backend.scout_of("shop.example"), None);
    assert!(world.script.seen().is_empty());

    // The same site again, from another page: the same exit.
    world
        .pipeline
        .fetch(at(Level::Plain, "https://www.shop.example/b"))
        .await
        .unwrap();
    assert_eq!(
        world.script.plain_seen()[1].job.route,
        PlainRoute::Pool(chosen)
    );

    // Its exit goes down: the request is answered with the reason, and
    // neither another exit nor a scout's address carries it.
    world.backend.put_plain_exit(chosen, false);
    let waiting = world
        .pipeline
        .fetch(at(Level::Plain, "https://shop.example/c"))
        .await
        .unwrap();
    assert_eq!(waiting.reason.as_deref(), Some("plain_exit_unavailable"));
    assert_eq!(world.script.plain_seen().len(), 2);
    assert_eq!(world.backend.site_exit_of("shop.example"), Some(chosen));
    assert_eq!(world.backend.scout_of("shop.example"), None);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_is_another_personas_turn_on_the_same_path() {
    let mut world = World::new(5, DEFAULT_DOMAINS);
    world.script.then(blocked(Outcome::RateLimited, 1.0));

    let report = world
        .pipeline
        .fetch(at(Level::Emulated, "https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(world.script.renders(), [Render::Http, Render::Http]);
    let personas = world.script.personas();
    assert_ne!(personas[0], personas[1]);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_for_the_http_path_stays_on_it() {
    let mut world = World::new(5, DEFAULT_DOMAINS);
    for _ in 0..3 {
        world.script.then(blocked(Outcome::ChallengeFailed, 1.0));
    }
    let order = at(Level::Emulated, "https://shop.example/a");

    let report = world.pipeline.fetch(order).await.unwrap();

    assert_eq!(report.outcome, Outcome::ChallengeFailed);
    assert_eq!(world.script.renders(), [Render::Http; 3]);
    assert_eq!(
        world
            .script
            .personas()
            .iter()
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_cooldown_is_the_policys_scaled_by_how_sure_the_signature_is() {
    let mut world = World::new(8, DEFAULT_DOMAINS);
    let hours = |hours: f64| Duration::from_secs_f64(hours * 3600.0);
    let cases = [
        (Outcome::Forbidden, 1.0, hours(24.0)),
        (Outcome::Forbidden, 0.5, hours(12.0)),
        (Outcome::ChallengeFailed, 0.6, hours(14.4)),
        (Outcome::Captcha, 0.25, hours(6.0)),
        (Outcome::RateLimited, 0.9, hours(1.8)),
        (Outcome::SoftBlock, 1.0, hours(6.0)),
        // Never under the floor of five minutes.
        (Outcome::Forbidden, 0.001, Duration::from_mins(5)),
    ];

    for (number, (outcome, confidence, expected)) in cases.into_iter().enumerate() {
        let domain = format!("site-{number}.example");
        world.script.then(blocked(outcome, confidence));
        let order = FetchOrder {
            max_retries: Some(0),
            ..browser(&format!("https://{domain}/"))
        };

        let report = world.pipeline.fetch(order).await.unwrap();

        assert_eq!(report.outcome, outcome);
        let attempt = &report.attempts[0];
        let state = world
            .backend
            .domain_state(attempt.persona_id.unwrap(), &domain)
            .unwrap();
        let cooldown = (state.cooldown_until.unwrap() - attempt.finished_at)
            .to_std()
            .unwrap();
        // The scaled cooldown, moved by at most a tenth either way so that
        // personas blocked together do not come back together.
        assert!(
            cooldown >= expected.mul_f64(0.9) && cooldown <= expected.mul_f64(1.1),
            "{outcome} at {confidence}: {cooldown:?}, expected about {expected:?}"
        );
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn server_errors_in_a_row_pause_the_domain_and_cool_nobody() {
    // The retry pause is a log-normal without a ceiling: by default one
    // pause in twenty is longer than 20 s, and some longer than the half
    // minute this test tells apart from it (3 % of runs failed on that).
    // Here nineteen in twenty are below 8 s and none comes near 30 s.
    let mut world = World::with(
        8,
        true,
        DEFAULT_DOMAINS,
        &[
            ("KAGE__SCHEDULER__RETRY_PAUSE__MIN", "2s"),
            ("KAGE__SCHEDULER__RETRY_PAUSE__MEDIAN", "6s"),
            ("KAGE__SCHEDULER__RETRY_PAUSE__P95", "8s"),
        ],
    );
    for _ in 0..4 {
        world.script.then(server_error(503));
    }
    let order = FetchOrder {
        max_retries: Some(3),
        ..browser("https://down.example/a")
    };

    let report = world.pipeline.fetch(order).await.unwrap();

    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.attempts.len(), 4);
    let personas = world.script.personas();
    assert_eq!(personas.iter().collect::<BTreeSet<_>>().len(), 4);
    let seen = world.script.seen();
    // The first retries come after their own pause; the third error in a
    // row leaves the site alone for half a minute.
    let pause = seen[3].at - report.attempts[2].finished_at;
    assert!(pause >= Span::seconds(30), "{pause}");
    assert!(pause < Span::seconds(60), "{pause}");
    for pair in [(0, 1), (1, 2)] {
        let pause = seen[pair.1].at - report.attempts[pair.0].finished_at;
        assert!(pause < Span::seconds(30), "{pause}");
    }
    // A server error says nothing about a persona or about bot defences.
    for persona in &personas {
        let state = world
            .backend
            .domain_state(*persona, "down.example")
            .unwrap();
        assert_eq!(state.cooldown_until, None);
        assert_eq!(state.block_count_24h, 0);
    }
    assert_eq!(world.backend.breaker("down.example"), None);

    // The fourth error doubled the pause: the next request for the site
    // waits it out, while another site is served meanwhile.
    let failed_at = report.attempts[3].finished_at;
    let elsewhere = world
        .pipeline
        .fetch(browser("https://up.example/a"))
        .await
        .unwrap();
    assert_eq!(elsewhere.outcome, Outcome::Ok);
    assert!(elsewhere.attempts[0].started_at - failed_at < Span::seconds(30));

    let again = world
        .pipeline
        .fetch(browser("https://down.example/b"))
        .await
        .unwrap();
    assert_eq!(again.outcome, Outcome::Ok);
    let waited = again.attempts[0].started_at - failed_at;
    assert!(waited >= Span::seconds(60), "{waited}");
    assert!(waited < Span::seconds(120), "{waited}");

    // An answer ended the row: the site is asked again without a pause.
    let answered_at = again.attempts[0].finished_at;
    let after = world
        .pipeline
        .fetch(browser("https://down.example/c"))
        .await
        .unwrap();
    assert!(after.attempts[0].started_at - answered_at < Span::seconds(30));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn nothing_leaves_while_egress_is_unsafe() {
    let mut world = World::new(4, DEFAULT_DOMAINS);
    world.backend.set_egress_unsafe(true);

    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("egress_unsafe"));
    assert_eq!(report.attempts, []);
    assert!(report.last.is_none() && report.persona.is_none());
    assert_eq!(world.script.seen().len(), 0);
    // The request and its answer are on record all the same.
    let requests = world.backend.requests();
    assert_eq!(requests[0].outcome, Some(Outcome::ProviderError));
    let records = world.backend.records();
    assert_eq!(records[0].reason.as_deref(), Some("egress_unsafe"));
    assert_eq!(records[0].persona, None);

    // The flag is read before every dispatch: cleared, work flows.
    world.backend.set_egress_unsafe(false);
    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);

    // Raised in the middle of a request, it stops the retry.
    let backend = std::sync::Arc::clone(&world.backend);
    world.script.then_with(move |_| {
        backend.set_egress_unsafe(true);
        blocked(Outcome::Forbidden, 1.0)
    });
    let report = world
        .pipeline
        .fetch(browser("https://other.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Forbidden);
    assert_eq!(report.attempts.len(), 1);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn nothing_leaves_once_the_bandwidth_budget_is_used_up() {
    let mut world = World::new(4, DEFAULT_DOMAINS);
    world.scheduler.set_bandwidth_stop(true);

    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("bandwidth_exhausted"));
    assert_eq!(world.script.seen().len(), 0);

    world.scheduler.set_bandwidth_stop(false);
    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_domain_that_blocks_is_paused_and_one_trial_decides_whether_it_stays_paused() {
    let rules = "defaults: { breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\ndomains: {}\n";
    let mut world = World::new(8, rules);
    world
        .script
        .then(blocked(Outcome::Forbidden, 1.0))
        .then(blocked(Outcome::Forbidden, 1.0));

    let report = world
        .pipeline
        .fetch(browser("https://wall.example/a"))
        .await
        .unwrap();

    // Two blocks within the hour: the breaker opened and said so.
    assert_eq!(report.attempts.len(), 2);
    let opened_at = report.attempts[1].finished_at;
    let breaker = world.backend.breaker("wall.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Open);
    assert_eq!(breaker.trips, 1);
    assert_eq!(breaker.paused_until, Some(opened_at + Span::minutes(30)));
    assert_eq!(world.alerts.kinds(), [AlertKind::DomainPaused]);
    let alert = &world.alerts.alerts()[0];
    assert_eq!(alert.details["domain"], "wall.example");
    assert_eq!(alert.details["threshold"], 2);

    // While it is paused every request is answered at once, without an
    // attempt, and is told when to come back.
    let refused = world
        .pipeline
        .fetch(browser("https://wall.example/b"))
        .await
        .unwrap();
    assert_eq!(refused.outcome, Outcome::ProviderError);
    assert_eq!(refused.reason.as_deref(), Some("domain_paused"));
    assert_eq!(refused.retry_at, breaker.paused_until);
    assert_eq!(refused.attempts, []);
    assert_eq!(world.now(), opened_at);
    // Another domain is not touched by it.
    let elsewhere = world
        .pipeline
        .fetch(browser("https://open.example/a"))
        .await
        .unwrap();
    assert_eq!(elsewhere.outcome, Outcome::Ok);

    // After the pause one request is the trial; it is blocked, and the
    // domain is paused again for twice as long. The retry is refused.
    tokio::time::sleep(Duration::from_mins(31)).await;
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    let trial = world
        .pipeline
        .fetch(browser("https://wall.example/c"))
        .await
        .unwrap();
    assert_eq!(trial.outcome, Outcome::Forbidden);
    assert_eq!(trial.attempts.len(), 1);
    let breaker = world.backend.breaker("wall.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Open);
    assert_eq!(breaker.trips, 2);
    assert_eq!(
        breaker.paused_until,
        Some(trial.attempts[0].finished_at + Span::hours(1))
    );
    assert_eq!(trial.retry_at, breaker.paused_until);
    assert_eq!(world.alerts.kinds(), [AlertKind::DomainPaused; 2]);

    // After that pause two requests arrive together: one is the trial, the
    // other is answered at once. The trial is served and the domain flows.
    tokio::time::sleep(Duration::from_mins(61)).await;
    let (one, other) = tokio::join!(
        world.pipeline.fetch(browser("https://wall.example/d")),
        world.pipeline.fetch(browser("https://wall.example/e")),
    );
    let (one, other) = (one.unwrap(), other.unwrap());
    let outcomes = [one.outcome, other.outcome];
    assert!(outcomes.contains(&Outcome::Ok), "{outcomes:?}");
    assert!(outcomes.contains(&Outcome::ProviderError), "{outcomes:?}");
    let turned_away = if one.outcome == Outcome::Ok {
        &other
    } else {
        &one
    };
    assert_eq!(turned_away.reason.as_deref(), Some("domain_paused"));
    assert_eq!(turned_away.attempts, []);
    let breaker = world.backend.breaker("wall.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);
    assert_eq!(breaker.trips, 0);
    assert_eq!(breaker.paused_until, None);

    let flowing = world
        .pipeline
        .fetch(browser("https://wall.example/f"))
        .await
        .unwrap();
    assert_eq!(flowing.outcome, Outcome::Ok);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_synchronous_fetch_that_outlasts_its_wall_time_runs_on() {
    let mut world = World::new(4, DEFAULT_DOMAINS);

    // Within the wall time it simply finishes.
    let quick = world
        .pipeline
        .fetch_sync(get("https://shop.example/a"))
        .await
        .unwrap();
    let Synchronous::Finished(report) = quick else {
        panic!("a quick fetch finishes: {quick:?}");
    };
    assert_eq!(report.outcome, Outcome::Ok);

    // One that takes longer than three minutes is handed back still
    // running, to be picked up as a job.
    world.script.takes(Duration::from_secs(200));
    let asked_at = world.now();
    let slow = world
        .pipeline
        .fetch_sync(get("https://slow.example/a"))
        .await
        .unwrap();
    let Synchronous::Overflowed(running) = slow else {
        panic!("a slow fetch overflows: {slow:?}");
    };
    assert_eq!(
        world.now() - asked_at,
        span(world.config.scheduler.sync_wall_time.get())
    );
    // Its request is stored and has no result yet.
    let stored = world
        .backend
        .requests()
        .into_iter()
        .find(|request| request.id == running.request)
        .unwrap();
    assert_eq!(stored.outcome, None);
    let request = running.request;

    let JobFetch::Finished(report) = running.wait().await.unwrap() else {
        panic!("the fetch that ran on finishes");
    };

    assert_eq!(report.request, request);
    assert_eq!(report.outcome, Outcome::Ok);
    assert!(
        world
            .backend
            .records()
            .iter()
            .any(|record| record.request_id == request)
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_persona_of_a_session_is_never_replaced() {
    let mut world = World::new(5, DEFAULT_DOMAINS);
    let pinned = |url: &str| FetchOrder {
        pin: Some(id(2)),
        ..browser(url)
    };

    // A failure that is not the site's verdict is tried again by the same
    // persona.
    world.script.then(timed_out());
    let report = world
        .pipeline
        .fetch(pinned("https://shop.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(world.script.personas(), [id(2), id(2)]);

    // Turned away, it cannot be replaced: the request ends with what it
    // got, whatever retries are left.
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    let report = world
        .pipeline
        .fetch(pinned("https://other.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Forbidden);
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.attempts[0].persona_id, Some(id(2)));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_can_ask_to_be_refused_instead_of_waiting() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    let back_at = world.now() + Span::hours(2);
    for number in 0..3 {
        world
            .backend
            .with_domain_state(id(number), "shop.example", |state| {
                state.cooldown_until = Some(back_at + Span::minutes(i64::from(number)));
            });
    }

    // Everyone is cooling: a caller with ten seconds of patience is told
    // when the first of them is back.
    let report = world
        .pipeline
        .fetch_within(browser("https://shop.example/a"), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("no_persona"));
    assert_eq!(report.retry_at, Some(back_at));
    assert_eq!(report.attempts, []);
    assert!(world.now() < back_at);

    // One with patience enough waits for it.
    let report = world
        .pipeline
        .fetch_within(browser("https://shop.example/a"), Duration::from_hours(3))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.attempts[0].persona_id, Some(id(0)));
    assert!(report.attempts[0].started_at >= back_at);
    assert!(report.pacing.waited >= Duration::from_mins(119));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn without_an_http_path_level_one_is_refused_and_the_browser_is_not() {
    // No persona of this pool has a profile for its browser's major.
    let mut world = World::with(4, false, DEFAULT_DOMAINS, &[]);

    let report = world
        .pipeline
        .fetch(get("https://shop.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(world.script.renders(), [Render::Browser]);

    let order = at(Level::Emulated, "https://shop.example/b");
    let report = world.pipeline.fetch(order).await.unwrap();
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("no_persona"));
    assert_eq!(report.attempts, []);
    assert_eq!(world.script.seen().len(), 1);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_url_the_guard_refuses_never_reaches_a_persona() {
    let mut world = World::new(3, DEFAULT_DOMAINS);

    for url in [
        "http://127.0.0.1/admin",
        "http://localhost:8080/",
        "http://10.0.0.7/",
        "http://169.254.169.254/latest/meta-data/",
    ] {
        let report = world.pipeline.fetch(get(url)).await.unwrap();

        assert_eq!(report.outcome, Outcome::SsrfDenied, "{url}");
        assert_eq!(report.reason.as_deref(), Some("ssrf_denied"));
        assert_eq!(report.attempts, []);
    }
    assert_eq!(world.script.seen().len(), 0);
    assert_eq!(world.backend.records().len(), 4);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_that_cannot_be_accepted_is_an_error_and_leaves_no_trace() {
    let mut world = World::new(3, DEFAULT_DOMAINS);

    let no_host = world.pipeline.fetch(get("data:text/plain,hello")).await;
    assert!(
        matches!(no_host, Err(SchedulerError::NoHost(_))),
        "{no_host:?}"
    );

    let order = FetchOrder {
        pacing: Some(PacingRequest::Profile("warp".to_owned())),
        ..get("https://shop.example/a")
    };
    let unknown = world.pipeline.fetch(order).await;
    assert!(
        matches!(unknown, Err(SchedulerError::Pacing(_))),
        "{unknown:?}"
    );
    assert_eq!(world.backend.requests().len(), 0);

    // Pacing that exists is applied and reported.
    let order = FetchOrder {
        pacing: Some(PacingRequest::Profile("fast".to_owned())),
        ..get("https://shop.example/a")
    };
    let report = world.pipeline.fetch(order).await.unwrap();
    assert_eq!(report.pacing.applied, "fast");
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_request_names_engines_countries_and_its_own_limits() {
    let rules = "defaults: { engines: [chromium] }\ndomains: {}\n";
    let mut world = World::new(3, rules);

    // The request accepts only an engine no persona of the policy has.
    let order = FetchOrder {
        engines: vec![kage_core::domain::Engine::Firefox],
        ..browser("https://shop.example/a")
    };
    let report = world.pipeline.fetch(order).await.unwrap();
    assert_eq!(report.reason.as_deref(), Some("no_persona"));

    // Its own timeout is the attempt's, up to the longest an attempt may
    // take; its own retries replace the policy's.
    world.script.then(timed_out()).then(timed_out());
    let order = FetchOrder {
        timeout: Some(Duration::from_hours(1)),
        max_retries: Some(1),
        ..browser("https://shop.example/a")
    };
    let report = world.pipeline.fetch(order).await.unwrap();
    assert_eq!(report.outcome, Outcome::Timeout);
    assert_eq!(report.reason.as_deref(), Some("timeout"));
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(
        world.script.seen()[0].job.timeout,
        kage_core::config::MAX_ATTEMPT_TIMEOUT
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn shutting_down_answers_what_waits_and_what_comes_after() {
    let mut world = World::new(1, DEFAULT_DOMAINS);
    world.script.takes(Duration::from_mins(30));
    let pipeline = world.pipeline.clone();
    let first = tokio::spawn(async move { pipeline.fetch(browser("https://a.example/")).await });
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The only persona is busy: a second request waits for it.
    let pipeline = world.pipeline.clone();
    let second = tokio::spawn(async move { pipeline.fetch(browser("https://b.example/")).await });
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(world.scheduler.depth(Priority::Sync), 1);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 1);

    world.shut_down().await;

    let second = second.await.unwrap().unwrap();
    assert_eq!(second.outcome, Outcome::ProviderError);
    assert_eq!(second.reason.as_deref(), Some("shutting_down"));
    let late = world
        .pipeline
        .fetch(browser("https://c.example/"))
        .await
        .unwrap();
    assert_eq!(late.reason.as_deref(), Some("shutting_down"));
    // The attempt that was under way is not cut off.
    let first = first.await.unwrap().unwrap();
    assert_eq!(first.outcome, Outcome::Ok);
}

/// The shipped `fast` profile with one seat on the domain and a gap of
/// `gap_ms`, as a request asks for it.
fn one_seat(world: &World, gap_ms: f64) -> PacingRequest {
    let fast = *world.policies.current().pacing.require("fast").unwrap();
    PacingRequest::Custom(PacingProfile {
        gap_ms: Distribution::Uniform(Uniform {
            min: gap_ms,
            max: gap_ms,
        }),
        domain_concurrency: 1,
        ..fast
    })
}

fn sync_page(url: &str) -> FetchOrder {
    FetchOrder {
        level: Level::Human,
        ..FetchOrder::get(Url::parse(url).unwrap(), Priority::Sync)
    }
}

#[tokio::test(start_paused = true)]
async fn synchronous_fetches_that_wait_together_keep_to_the_domains_concurrency() {
    // F106 (sched-01): eight synchronous fetches of one site, 50 ms apart,
    // one seat on the domain, a gap of 2 s and no break. One address reads
    // them all at its gap; before, every persona took a turn within a
    // second.
    let mut world = World::with(3, false, DEFAULT_DOMAINS, &[]);
    world.script.takes(Duration::from_millis(500));
    let pacing = one_seat(&world, 2000.0);
    let mut fetches = Vec::new();
    for number in 0..8 {
        let order = FetchOrder {
            pacing: Some(pacing.clone()),
            ..sync_page(&format!("https://one.example/{number}"))
        };
        let pipeline = world.pipeline.clone();
        fetches.push(tokio::spawn(async move { pipeline.fetch(order).await }));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for fetch in fetches {
        let report = fetch.await.unwrap().unwrap();
        assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    }
    let readers: BTreeSet<_> = world.script.personas().into_iter().collect();
    assert_eq!(readers.len(), 1, "{readers:?}");
    let seen = world.script.seen();
    assert_eq!(seen.len(), 8);
    for pair in seen.windows(2) {
        // The gap at the reader's own pace: 0.8 to 1.25 of 2 s.
        assert!(
            pair[1].at - pair[0].at >= Span::milliseconds(1600),
            "{} then {}",
            pair[0].at,
            pair[1].at
        );
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_proxy_that_fails_is_the_proxys_failure_and_never_the_sites() {
    // F108 (sched-05): a session's persona whose proxy is down. Its
    // attempts fail before the site has a say; the site is not paused,
    // and the next page goes at once to a persona whose proxy works.
    let mut world = World::with(3, false, DEFAULT_DOMAINS, &[]);
    world.script.takes(Duration::from_millis(50));
    let dead = id(0);
    world.script.when(move |job| {
        (job.persona.id == dead).then(|| {
            AttemptOutput::failed(Outcome::ProviderError, "proxy_failed", Courtesy::NotTheSite)
        })
    });
    let pinned = FetchOrder {
        pin: Some(dead),
        ..sync_page("https://one.example/dead")
    };
    let report = world.pipeline.fetch(pinned).await.unwrap();
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("proxy_failed"));
    assert_eq!(report.attempts.len(), 3);
    assert!(
        report
            .attempts
            .iter()
            .all(|attempt| attempt.reason.as_deref() == Some("proxy_failed"))
    );

    let asked = world.now();
    let next = world
        .pipeline
        .fetch(sync_page("https://one.example/next"))
        .await
        .unwrap();
    assert_eq!(next.outcome, Outcome::Ok);
    let reader = world.script.seen().last().unwrap().job.persona.id;
    assert_ne!(reader, dead);
    assert!(
        world.now() - asked < Span::seconds(5),
        "waited {}",
        world.now() - asked
    );
    world.shut_down().await;

    // The control: the same three failures as the site's own (no answer
    // from it) pause the site for 30 s, for everybody.
    let mut world = World::with(3, false, DEFAULT_DOMAINS, &[]);
    world.script.takes(Duration::from_millis(50));
    world.script.when(move |job| {
        (job.persona.id == dead).then(|| {
            AttemptOutput::failed(
                Outcome::ProviderError,
                "navigation_failed",
                Courtesy::ServerError,
            )
        })
    });
    let pinned = FetchOrder {
        pin: Some(dead),
        ..sync_page("https://one.example/dead")
    };
    world.pipeline.fetch(pinned).await.unwrap();
    let asked = world.now();
    world
        .pipeline
        .fetch(sync_page("https://one.example/next"))
        .await
        .unwrap();
    assert!(world.now() - asked >= Span::seconds(25));
    world.shut_down().await;
}

/// QA round one's policy-01 (F104): a host with one trailing dot was
/// another site to the breaker, the cooldowns, the pace and the plain exit.
/// It is the same site: blocks under one spelling pause the other, the
/// personas that were turned away cool for the site, level 0 leaves by the
/// site's one exit, and the request is stored and asked for without the
/// dot. A host with two dots is refused before anything is sent.
#[tokio::test(start_paused = true)]
async fn a_host_with_a_trailing_dot_is_the_same_site_and_one_with_two_is_refused() {
    let rules = "defaults: { breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\ndomains: {}\n";
    let mut world = World::new(8, rules);
    world
        .script
        .then(blocked(Outcome::Forbidden, 1.0))
        .then(blocked(Outcome::Forbidden, 1.0));

    let report = world
        .pipeline
        .fetch(browser("https://wall.example./a"))
        .await
        .unwrap();
    assert_eq!(report.attempts.len(), 2);
    let breaker = world.backend.breaker("wall.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Open);
    assert_eq!(world.backend.breaker("wall.example."), None);
    // The two personas that were turned away cool for the site.
    for attempt in &report.attempts {
        let persona = attempt.persona_id.unwrap();
        let state = world.backend.domain_state(persona, "wall.example").unwrap();
        assert!(state.cooldown_until.is_some());
        assert_eq!(world.backend.domain_state(persona, "wall.example."), None);
    }
    // What was asked for and stored has no dot.
    let urls = world.script.urls();
    assert!(
        urls.iter().all(|url| url == "https://wall.example/a"),
        "{urls:?}"
    );
    let requests = world.backend.requests();
    assert_eq!(requests[0].domain, "wall.example");
    assert_eq!(requests[0].url, "https://wall.example/a");

    // Either spelling, at any level, is paused.
    for (level, url) in [
        (Level::Human, "https://wall.example/b"),
        (Level::Human, "https://wall.example./c"),
        (Level::Plain, "https://WALL.example./d"),
    ] {
        let refused = world.pipeline.fetch(at(level, url)).await.unwrap();
        assert_eq!(refused.reason.as_deref(), Some("domain_paused"), "{url}");
        assert_eq!(refused.attempts, [], "{url}");
    }
    assert_eq!(world.script.seen().len(), 2);
    assert!(world.script.plain_seen().is_empty());

    // Level 0 of another site leaves by that site's one exit, however the
    // host is spelled.
    for url in ["https://shop.example/a", "https://shop.example./b"] {
        let read = world.pipeline.fetch(at(Level::Plain, url)).await.unwrap();
        assert_eq!(read.outcome, Outcome::Ok, "{url}");
    }
    let seen = world.script.plain_seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].job.route, seen[1].job.route);
    assert_eq!(seen[1].job.url.as_str(), "https://shop.example/b");

    // Two dots, a leading dot, two in a row: no host; nothing is sent.
    for url in [
        "http://localhost../",
        "http://127.0.0.1../",
        "https://shop.example../a",
        "https://.shop.example/a",
        "https://shop..example/a",
    ] {
        for level in [Level::Plain, Level::Human] {
            let refused = world.pipeline.fetch(at(level, url)).await.unwrap();
            assert_eq!(refused.outcome, Outcome::SsrfDenied, "{url}");
            assert_eq!(refused.attempts, [], "{url}");
        }
    }
    assert_eq!(world.script.seen().len(), 2);
    assert_eq!(world.script.plain_seen().len(), 2);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_fetch_redirected_to_another_sites_wall_books_the_block_to_that_site() {
    // F187 (flows-03): the shop redirects to another site's wall. The
    // block is that site's, as a flow's is since F101: the persona cools
    // there, its breaker counts it, and the shop that only redirected is
    // left as it was.
    let rules = "defaults: { pacing: fast, active_hours: ignore, breaker_threshold: 1, \
                 breaker_pause: 30m, retries: 0 }\ndomains: {}\n";
    let mut world = World::with(2, false, rules, &[]);
    let mut wall = blocked(Outcome::Forbidden, 1.0);
    wall.final_url = Some("https://wall.example/denied".to_owned());
    world.script.then(wall);

    let report = world
        .pipeline
        .fetch(browser("https://shop.example/r"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Forbidden);
    let persona = report.attempts[0].persona_id.unwrap();
    let there = world.backend.domain_state(persona, "wall.example").unwrap();
    assert!(there.cooldown_until.is_some());
    assert_eq!(there.block_count_24h, 1);
    assert_eq!(
        world
            .backend
            .breaker("wall.example")
            .map(|breaker| breaker.state),
        Some(BreakerState::Open)
    );
    assert!(world.backend.breaker("shop.example").is_none());
    let shop = world.backend.domain_state(persona, "shop.example").unwrap();
    assert_eq!(shop.cooldown_until, None);
    assert_eq!(shop.block_count_24h, 0);
    assert_eq!(shop.last_outcome, Some(Outcome::Ok));

    // The shop is not paused: its next page goes at once.
    let next = world
        .pipeline
        .fetch(browser("https://shop.example/b"))
        .await
        .unwrap();
    assert_eq!(next.outcome, Outcome::Ok);
    world.shut_down().await;
}

/// A flow that starts on the shop and goes on to `pay.example`.
fn across() -> FetchOrder {
    let goto = |url: &str| kage_browser::contract::FlowAction::Goto {
        human: None,
        timeout_ms: None,
        url: url.to_owned(),
    };
    FetchOrder {
        level: Level::Human,
        max_retries: Some(0),
        flow: Some(kage_scheduler::FlowPlan {
            actions: vec![
                goto("https://shop.example/start"),
                goto("https://pay.example/checkout"),
            ],
            typos: false,
        }),
        ..get("https://shop.example/start")
    }
}

/// A flow whose `goto` to its later site ran out of time: no page came.
fn later_site_timed_out() -> AttemptOutput {
    let step = |index: u32, ok: bool| kage_scheduler::FlowStep {
        index,
        action: "goto".to_owned(),
        ok,
        duration_ms: 1000,
        paced_ms: None,
        error: (!ok).then(|| "timeout: page.goto: Timeout 30000ms exceeded".to_owned()),
        capture: None,
        opened_tabs: Vec::new(),
    };
    let mut output = timed_out();
    output.flow = Some(kage_scheduler::FlowRun {
        steps: vec![step(0, true), step(1, false)],
        ok: false,
    });
    output
}

#[tokio::test(start_paused = true)]
async fn a_flows_later_site_is_booked_its_own_timeouts() {
    // F162: three flows whose `goto` to the later site timed out. The
    // timeouts are that site's: its pause after server errors holds the
    // next flow that goes there, and the shop, which answered every time,
    // is not paused for them.
    let mut world = World::with(4, false, DEFAULT_DOMAINS, &[]);
    for _ in 0..3 {
        world.script.then(later_site_timed_out());
        let report = world.pipeline.fetch(across()).await.unwrap();
        assert_eq!(report.outcome, Outcome::Timeout, "{:?}", report.reason);
    }
    let paused_at = world.now();
    // The shop's own page goes at once.
    let shop = world
        .pipeline
        .fetch(browser("https://shop.example/page"))
        .await
        .unwrap();
    assert_eq!(shop.outcome, Outcome::Ok);
    let shop_at = world.script.seen().last().unwrap().at;
    assert!(
        shop_at - paused_at < Span::seconds(30),
        "{shop_at} {paused_at}"
    );
    // The next flow to the later site waits for the end of its pause.
    let onward = world.pipeline.fetch(across()).await.unwrap();
    assert_eq!(onward.outcome, Outcome::Ok);
    let onward_at = world.script.seen().last().unwrap().at;
    assert!(
        onward_at - paused_at >= Span::seconds(29),
        "{onward_at} {paused_at}"
    );
    world.shut_down().await;
}

/// QA round three, flows-01 and lock-01: an attempt whose persona's context
/// ran another operation in its sidecar for as long as it waited asked the
/// site nothing. It spends no retry (the request has none here), it is no
/// page of the persona's day on the site, and the persona, a job's visitor
/// or a session's, takes the next attempt, which is made.
#[tokio::test(start_paused = true)]
async fn an_attempt_a_busy_context_refused_is_no_page_and_spends_no_retry() {
    let mut world = World::new(1, DEFAULT_DOMAINS);
    world
        .script
        .then(AttemptOutput::failed(
            Outcome::ProviderError,
            "context_busy",
            Courtesy::NotTheSite,
        ))
        .then(ok());
    let report = world
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..browser("https://shop.example/a")
        })
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let reasons: Vec<Option<&str>> = report
        .attempts
        .iter()
        .map(|attempt| attempt.reason.as_deref())
        .collect();
    assert_eq!(reasons, [Some("context_busy"), None]);
    assert_eq!(
        report
            .attempts
            .iter()
            .map(|attempt| attempt.n)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let personas: BTreeSet<_> = world
        .script
        .seen()
        .iter()
        .map(|seen| seen.job.persona.id)
        .collect();
    assert_eq!(personas.len(), 1);
    let persona = *personas.first().unwrap();
    let state = world.backend.domain_state(persona, "shop.example").unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_outcome, Some(Outcome::Ok));

    // A context that stays busy through every wait: past the bound the
    // refusals count as failures, and with no retry left the request ends.
    // The persona is then held until its context is free, as a cut flow's
    // is (#78's review): the next request has it only after that.
    assert_eq!(world.script.settled(), []);
    let busy =
        || AttemptOutput::failed(Outcome::ProviderError, "context_busy", Courtesy::NotTheSite);
    for _ in 0..4 {
        world.script.then(busy());
    }
    world.script.settle_takes(Duration::from_secs(90));
    let report = world
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..browser("https://shop.example/b")
        })
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("context_busy"));
    assert_eq!(report.attempts.len(), 4);
    let ended = report.attempts[3].finished_at;
    let next = world
        .pipeline
        .fetch(browser("https://shop.example/c"))
        .await
        .unwrap();
    assert_eq!(next.outcome, Outcome::Ok, "{:?}", next.reason);
    // Waited for once, after the fourth refusal only.
    assert_eq!(world.script.settled(), [persona]);
    assert!(
        next.attempts[0].started_at - ended >= Span::seconds(90),
        "{:?}",
        next.attempts[0].started_at - ended
    );
    world.shut_down().await;
}

/// QA round three, lock-01: a context that another server opened with other
/// parameters (`context_conflict` after a kill and a restart, or a
/// hand-over) is the persona still busy as well: no retry spent, no page.
/// Asking it again is refused the same at once, so the next attempt goes to
/// another persona (#78's review).
#[tokio::test(start_paused = true)]
async fn an_attempt_that_met_another_servers_context_spends_no_retry() {
    let mut world = World::new(2, DEFAULT_DOMAINS);
    world
        .script
        .then(AttemptOutput::failed(
            Outcome::ProviderError,
            "context_conflict",
            Courtesy::NotTheSite,
        ))
        .then(ok());
    let report = world
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..browser("https://shop.example/a")
        })
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 2);
    let conflicting = report.attempts[0].persona_id.unwrap();
    let persona = report.attempts[1].persona_id.unwrap();
    assert_ne!(persona, conflicting);
    let state = world.backend.domain_state(persona, "shop.example").unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(
        world
            .backend
            .domain_state(conflicting, "shop.example")
            .map_or(0, |state| state.requests_today),
        0
    );
    world.shut_down().await;

    // With no other persona the request is not given the conflicting one
    // again: it gets no second attempt.
    let mut alone = World::new(1, DEFAULT_DOMAINS);
    alone
        .script
        .then(AttemptOutput::failed(
            Outcome::ProviderError,
            "context_conflict",
            Courtesy::NotTheSite,
        ))
        .then(ok());
    let report = alone
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..browser("https://shop.example/a")
        })
        .await
        .unwrap();
    assert_eq!(alone.script.seen().len(), 1, "{report:?}");
    assert_ne!(report.outcome, Outcome::Ok, "{report:?}");
    alone.shut_down().await;
}
