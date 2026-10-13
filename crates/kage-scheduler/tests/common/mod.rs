//! What the scheduler's tests share:
//!
//! - [`persona`]: a persona that is ready for work, with or without an
//!   http path.
//! - [`policies`]: a policy directory with the shipped pacing profiles and
//!   block signatures and the domain rules a test writes.
//! - [`Script`]: an attempt executor that answers what a test scripted and
//!   records what it was asked.
//! - [`document`], [`file`]: a page as a browser brings it back, with a
//!   document or with the body of a file.
//! - [`World`]: a scheduler over the in-memory backend with a pipeline on
//!   top, on tokio's clock, so a test that pauses the timer steps through
//!   hours in milliseconds.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::struct_field_names,
    clippy::too_many_lines
)]

pub mod lab;
pub mod proxy;

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use kage_browser::contract::PageResult;
use kage_core::alert::{AlertSink, MemoryAlertSink};
use kage_core::classifier::Classification;
use kage_core::config::Config;
use kage_core::domain::{
    ClaimedOs, Engine, Geolocation, Persona, PersonaState, Render, Screen, Timing,
};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::outcome::{Block, BlockType, Outcome, Vendor};
use kage_core::policy::PolicyStore;
use kage_core::schedule::planner::Work;
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use kage_core::schedule::selection::Network;
use kage_core::seed::Seed;
use kage_proxy::{Clock, TokioClock};
use kage_scheduler::{
    AttemptExecutor, AttemptJob, AttemptOutput, Backend, MemoryBackend, Pipeline, PipelineSettings,
    Scheduler, Verdicts, cooldown_rules, planner_rules,
};
use tempfile::TempDir;
use tokio::sync::oneshot;
use uuid::Uuid;

/// The directory of the shipped policies.
const SHIPPED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies");

/// The domain rules of the shipped policy: every default, no domain.
pub const DEFAULT_DOMAINS: &str = "defaults: {}\ndomains: {}\n";

/// Noon in Berlin on a summer day: every persona there is awake.
pub fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap()
}

/// The configuration with every default, and the overrides a test names
/// as `KAGE__SECTION__KEY` pairs.
pub fn config(overrides: &[(&str, &str)]) -> Config {
    let base = [
        ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
        ("KAGE__SIDECAR__TOKEN", "token"),
        (
            "KAGE__SECRETS__MASTER_KEY",
            "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
        ),
    ];
    Config::load(
        None,
        base.iter()
            .chain(overrides)
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    )
    .unwrap()
}

/// The id of persona `number`.
pub fn id(number: u8) -> PersonaId {
    PersonaId::from_uuid(Uuid::from_u128(u128::from(number) + 1))
}

/// Where persona `number` sits: a /24 and an autonomous system of its own.
pub fn network(number: u8) -> Network {
    Network::of(
        Some(IpAddr::V4(Ipv4Addr::new(198, 51, number, 10))),
        Some(&format!("AS{}", 64_500 + u32::from(number))),
    )
}

/// Persona `number`, warmed up and in Berlin. With `http` it is chromium
/// 149 with the profile of that major; without, chromium 155, which no
/// profile exists for.
pub fn persona(number: u8, http: bool) -> Persona {
    let id = id(number);
    let major = if http { 149 } else { 155 };
    Persona {
        id,
        proxy_id: ProxyId::from_uuid(Uuid::from_u128(u128::from(number) + 1001)),
        engine: Engine::Chromium,
        claimed_os: ClaimedOs::Linux,
        browser_major: major,
        seed: kage_core::persona::seed_for(id),
        fingerprint: serde_json::json!({ "schema": 1 }),
        user_agent: format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{major}.0.0.0 Safari/537.36"
        ),
        hardware_concurrency: 8,
        wreq_profile: http.then(|| "chrome_149".to_owned()),
        country: "DE".parse().ok(),
        locale: "de-DE".to_owned(),
        languages: ["de-DE", "de", "en-US", "en"].map(str::to_owned).to_vec(),
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
        tables_version: kage_core::persona::TABLES_VERSION.to_owned(),
        profile_dir: kage_core::persona::profile_dir(Engine::Chromium, id),
        state: PersonaState::Active,
        cooling_reason: None,
        warmup_visits: 3,
        warmed_at: Some(noon() - chrono::Duration::days(2)),
        created_at: noon() - chrono::Duration::days(3),
        retired_at: None,
        retire_reason: None,
        profile_archived_at: None,
        profile_archive: None,
        profile_deleted_at: None,
    }
}

/// Writes a policy directory with the shipped pacing profiles and block
/// signatures and `domains` as its domain rules, and opens it.
pub fn policies(dir: &Path, domains: &str, alerts: Arc<MemoryAlertSink>) -> Arc<PolicyStore> {
    for file in ["pacing-profiles.yaml", "block-signatures.yaml"] {
        std::fs::copy(Path::new(SHIPPED).join(file), dir.join(file)).unwrap();
    }
    std::fs::write(dir.join("domains.yaml"), domains).unwrap();
    Arc::new(PolicyStore::open(dir, alerts).unwrap())
}

/// A page the site served.
pub fn ok() -> AttemptOutput {
    answered(Outcome::Ok, None, 200)
}

/// A block of `outcome` that a signature of `confidence` matched, with the
/// hints the outcome implies.
pub fn blocked(outcome: Outcome, confidence: f32) -> AttemptOutput {
    let block = Block {
        vendor: Vendor::new("cloudflare").unwrap(),
        block_type: BlockType::new("test").unwrap(),
        confidence,
        evidence: vec!["test".to_owned()],
    };
    let status = match outcome {
        Outcome::RateLimited => 429,
        Outcome::SoftBlock => 200,
        _ => 403,
    };
    answered(outcome, Some(block), status)
}

/// A page as a browser brings it back from `url`, before anything a test
/// lays over it.
fn page(url: &str, status: u16, content_type: &str, over: &serde_json::Value) -> AttemptOutput {
    let mut page = lab::example("PageResult");
    page["url"] = serde_json::json!(url);
    page["final_url"] = serde_json::json!(url);
    page["redirects"] = serde_json::json!([]);
    page["status"] = serde_json::json!(status);
    page["error"] = serde_json::Value::Null;
    page["headers"] = serde_json::json!([{ "name": "content-type", "value": content_type }]);
    page["dom_html"] = serde_json::Value::Null;
    page["raw_body"] = serde_json::Value::Null;
    page["links"] = serde_json::Value::Null;
    page["links_truncated"] = serde_json::json!(false);
    page["xhr"] = serde_json::json!([]);
    page["console"] = serde_json::json!([]);
    page["screenshot"] = serde_json::Value::Null;
    page["pdf"] = serde_json::Value::Null;
    if let serde_json::Value::Object(over) = over {
        for (key, value) in over {
            page[key.as_str()] = value.clone();
        }
    }
    let page: PageResult = serde_json::from_value(page).unwrap();
    let outcome = if status >= 500 {
        Outcome::ProviderError
    } else {
        Outcome::Ok
    };
    AttemptOutput::of_page(
        Box::new(page),
        answered(outcome, None, status).classification,
    )
}

/// The page of `url` with `html` as the document the browser had.
pub fn document(url: &str, html: &str) -> AttemptOutput {
    page(
        url,
        200,
        "text/html; charset=utf-8",
        &serde_json::json!({ "dom_html": html }),
    )
}

/// What a browser brings back of a file it shows as text (a
/// `robots.txt`): the body as the site sent it, with `status`.
pub fn file(url: &str, status: u16, body: &str) -> AttemptOutput {
    page(
        url,
        status,
        "text/plain",
        &serde_json::json!({
            "dom_html": format!("<html><head></head><body><pre>{body}</pre></body></html>"),
            "raw_body": {
                "data": body, "encoding": "utf8", "bytes": body.len(), "truncated": false,
            },
        }),
    )
}

/// A server error with no vendor marker.
pub fn server_error(status: u16) -> AttemptOutput {
    let mut output = answered(Outcome::ProviderError, None, status);
    output.courtesy = Courtesy::ServerError;
    output
}

/// An attempt that ran out of time.
pub fn timed_out() -> AttemptOutput {
    AttemptOutput::failed(Outcome::Timeout, "timeout", Courtesy::ServerError)
}

fn answered(outcome: Outcome, block: Option<Block>, status: u16) -> AttemptOutput {
    AttemptOutput {
        classification: Classification {
            outcome,
            vendors: block.iter().map(|block| block.vendor.clone()).collect(),
            rule: block.as_ref().map(|_| "test".to_owned()),
            block,
            retryable: outcome.is_retryable(),
            escalate: outcome.escalates_to_browser(),
            cooldown: outcome.default_cooldown(),
            evidence: Vec::new(),
        },
        courtesy: Courtesy::Answered,
        reason: None,
        status: Some(status),
        final_url: Some("https://example.org/".to_owned()),
        headers: Vec::new(),
        redirects: Vec::new(),
        timing: Timing {
            total_ms: 800,
            ..Timing::default()
        },
        page: None,
        fetched: None,
        cookies_to_browser: None,
        cookies_to_http: None,
        behavior: None,
        flow: None,
        reached: None,
    }
}

/// One attempt a [`Script`] was asked to make.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The attempt.
    pub job: AttemptJob,
    /// When it started.
    pub at: DateTime<Utc>,
}

type Reply = Box<dyn FnOnce(&AttemptJob) -> AttemptOutput + Send>;

type Rule = Box<dyn Fn(&AttemptJob) -> Option<AttemptOutput> + Send>;

/// An executor that answers what a test scripted, in order, and a served
/// page once the script is over. Every attempt takes `took` on the clock.
pub struct Script {
    rules: Mutex<Vec<Rule>>,
    replies: Mutex<VecDeque<Reply>>,
    seen: Mutex<Vec<Seen>>,
    took: Mutex<Duration>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Script {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Script").finish_non_exhaustive()
    }
}

impl Script {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            rules: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            took: Mutex::new(Duration::from_millis(800)),
            clock,
        }
    }

    /// The next attempt that has no answer yet gets this one.
    pub fn then(&self, output: AttemptOutput) -> &Self {
        self.then_with(move |_| output)
    }

    /// The next attempt that has no answer yet gets what `reply` makes of
    /// it.
    pub fn then_with(
        &self,
        reply: impl FnOnce(&AttemptJob) -> AttemptOutput + Send + 'static,
    ) -> &Self {
        self.replies.lock().unwrap().push_back(Box::new(reply));
        self
    }

    /// Every attempt `rule` has an answer for gets that answer, whenever
    /// it comes and before anything scripted in order: for what is asked
    /// for beside the pages of a test (a `robots.txt`).
    pub fn when(&self, rule: impl Fn(&AttemptJob) -> Option<AttemptOutput> + Send + 'static) {
        self.rules.lock().unwrap().push(Box::new(rule));
    }

    /// The addresses of the attempts asked for so far, in order.
    pub fn urls(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|seen| seen.job.url.to_string())
            .collect()
    }

    /// How long every later attempt takes.
    pub fn takes(&self, took: Duration) {
        *self.took.lock().unwrap() = took;
    }

    /// Every attempt asked for so far, in order.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The personas of the attempts asked for so far, in order.
    pub fn personas(&self) -> Vec<PersonaId> {
        self.seen().iter().map(|seen| seen.job.persona.id).collect()
    }

    /// The paths of the attempts asked for so far, in order.
    pub fn renders(&self) -> Vec<Render> {
        self.seen().iter().map(|seen| seen.job.render).collect()
    }
}

#[async_trait]
impl AttemptExecutor for Script {
    async fn run(&self, job: &AttemptJob) -> AttemptOutput {
        self.seen.lock().unwrap().push(Seen {
            job: job.clone(),
            at: self.clock.now(),
        });
        let took = *self.took.lock().unwrap();
        tokio::time::sleep(took).await;
        let ruled = self.rules.lock().unwrap().iter().find_map(|rule| rule(job));
        if let Some(output) = ruled {
            return output;
        }
        let reply = self.replies.lock().unwrap().pop_front();
        reply.map_or_else(ok, |reply| reply(job))
    }
}

/// A scheduler over the in-memory backend with a pipeline on a scripted
/// executor. Start it under a paused timer.
pub struct World {
    /// What is stored.
    pub backend: Arc<MemoryBackend>,
    /// The scheduler, running.
    pub scheduler: Arc<Scheduler>,
    /// What an attempt means.
    pub verdicts: Arc<Verdicts>,
    /// The alerts raised.
    pub alerts: Arc<MemoryAlertSink>,
    /// The executor.
    pub script: Arc<Script>,
    /// The fetch pipeline.
    pub pipeline: Pipeline,
    /// The clock: tokio's, starting at [`noon`].
    pub clock: Arc<dyn Clock>,
    /// The policies in force.
    pub policies: Arc<PolicyStore>,
    /// The configuration the rules were taken from.
    pub config: Config,
    stop: Option<oneshot::Sender<()>>,
    running: Option<tokio::task::JoinHandle<()>>,
    _dir: TempDir,
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World").finish_non_exhaustive()
    }
}

impl World {
    /// A world with `personas` personas that have an http path, the domain
    /// rules of `domains` and the default configuration.
    pub fn new(personas: u8, domains: &str) -> Self {
        Self::with(personas, true, domains, &[])
    }

    /// A world with `personas` personas, with or without an http path, the
    /// domain rules of `domains`, and the configuration `overrides`.
    pub fn with(personas: u8, http: bool, domains: &str, overrides: &[(&str, &str)]) -> Self {
        let config = config(overrides);
        let dir = TempDir::new().unwrap();
        let alerts = Arc::new(MemoryAlertSink::new());
        let policies = policies(dir.path(), domains, Arc::clone(&alerts));
        let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(noon()));
        let backend = Arc::new(MemoryBackend::new());
        for number in 0..personas {
            backend.put_persona(persona(number, http), true, network(number));
        }
        let stored: Arc<dyn Backend> = backend.clone();
        let sink: Arc<dyn AlertSink> = alerts.clone();
        let scheduler = Scheduler::new(
            planner_rules(&config),
            Seed::from_u64(7),
            Arc::clone(&stored),
            Arc::clone(&clock),
        );
        let verdicts = Arc::new(Verdicts::new(
            stored,
            Arc::clone(&policies),
            cooldown_rules(&config),
            config.scheduler.breaker_max_pause.get(),
            config.scheduler.breaker_trial_timeout.get(),
            sink,
            Arc::clone(&clock),
        ));
        let script = Arc::new(Script::new(Arc::clone(&clock)));
        let executor: Arc<dyn AttemptExecutor> = script.clone();
        let pipeline = Pipeline::new(
            Arc::clone(&scheduler),
            executor,
            Arc::clone(&verdicts),
            PipelineSettings::from_config(&config),
        );
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(Arc::clone(&scheduler).run(async move {
            let _ = stopped.await;
        }));
        Self {
            backend,
            scheduler,
            verdicts,
            alerts,
            script,
            pipeline,
            clock,
            policies,
            config,
            stop: Some(stop),
            running: Some(running),
            _dir: dir,
        }
    }

    /// The time on the world's clock.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// One attempt for `domain` as the pipeline would ask for it: any
    /// persona the policy allows, at the pace of the `human` profile.
    pub fn work(&self, domain: &str, render: Render, priority: Priority) -> Work {
        let policies = self.policies.current();
        let policy = policies.domains.resolve(domain);
        let rules = self.verdicts.breaker_rules(domain);
        Work {
            domain: domain.to_owned(),
            priority,
            render,
            pin: None,
            exclude: Vec::new(),
            avoid: Vec::new(),
            engines: policy.engines.clone(),
            countries: policy.countries.clone(),
            prefer_countries: Vec::new(),
            visitors: None,
            pacing: *policies.pacing.require("human").unwrap(),
            active_hours: policy.active_hours,
            breaker_threshold: rules.threshold,
            breaker_pause: rules.pause,
            follow_up: false,
            retry_pause: None,
            not_before: None,
            give_up_at: None,
            gap_floor: None,
        }
    }

    /// Stops the scheduler and waits until it has refused what waited.
    pub async fn shut_down(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(running) = self.running.take() {
            running.await.unwrap();
        }
    }
}
