//! Configuration of jobs and of the webhooks that report them (spec
//! §16.6).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::duration::HumanDuration;

/// Most items a job may ever be allowed to have.
pub const MAX_JOB_ITEMS: u32 = 100_000;

/// Most retries a webhook delivery may ever be given.
pub const MAX_WEBHOOK_RETRIES: u32 = 10;

/// Jobs: batches and flows worked in the background (spec §16.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsConfig {
    /// Most items one job may have.
    #[serde(default = "default_max_items")]
    pub max_items: u32,
    /// Jobs worked at once; the others wait in the order they came.
    #[serde(default = "default_max_running")]
    pub max_running: u32,
    /// Items of all jobs that wait in the scheduler or are being fetched at
    /// once.
    #[serde(default = "default_max_items_in_flight")]
    pub max_items_in_flight: u32,
    /// How long after it was accepted a job stops waiting: the items left
    /// then fail with what they were waiting for.
    #[serde(default = "default_max_age")]
    pub max_age: HumanDuration,
    /// Days a finished job, its items and their results are kept.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// How long a worker's claim on a job holds without being renewed. A
    /// job whose worker stopped is taken up again after this long.
    #[serde(default = "default_lease")]
    pub lease: HumanDuration,
    /// How often the worker looks for jobs to take up.
    #[serde(default = "default_poll_interval")]
    pub poll_interval: HumanDuration,
    /// How long an `Idempotency-Key` names the job it made.
    #[serde(default = "default_idempotency_ttl")]
    pub idempotency_ttl: HumanDuration,
}

const fn default_max_items() -> u32 {
    10_000
}

const fn default_max_running() -> u32 {
    8
}

const fn default_max_items_in_flight() -> u32 {
    24
}

const fn default_max_age() -> HumanDuration {
    HumanDuration::from_hours(7 * 24)
}

const fn default_retention_days() -> u32 {
    14
}

const fn default_lease() -> HumanDuration {
    HumanDuration::from_secs(60)
}

const fn default_poll_interval() -> HumanDuration {
    HumanDuration::from_secs(2)
}

const fn default_idempotency_ttl() -> HumanDuration {
    HumanDuration::from_hours(24)
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            max_items: default_max_items(),
            max_running: default_max_running(),
            max_items_in_flight: default_max_items_in_flight(),
            max_age: default_max_age(),
            retention_days: default_retention_days(),
            lease: default_lease(),
            poll_interval: default_poll_interval(),
            idempotency_ttl: default_idempotency_ttl(),
        }
    }
}

impl JobsConfig {
    /// How long a finished job is kept.
    pub fn retention(&self) -> Duration {
        Duration::from_secs(u64::from(self.retention_days) * 24 * 60 * 60)
    }
}

/// Webhooks: the events of a job, posted to the receiver the job names
/// (spec §16.6). They are the one request Kage makes from its own address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhooksConfig {
    /// The only receivers a job may name, each an origin
    /// (`scheme://host[:port]`). Empty, the default: no job may have a
    /// webhook. The host's own address posts to nothing the operator did
    /// not list here (decision D17 extends to webhooks).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Those of `allowed_origins` that sit on the operator's own network
    /// (the compose network, the LAN, this host) and may therefore resolve
    /// to a private or loopback address, which the SSRF guard refuses for
    /// every other receiver.
    #[serde(default)]
    pub private_origins: Vec<String>,
    /// How long one delivery attempt may take.
    #[serde(default = "default_webhook_timeout")]
    pub timeout: HumanDuration,
    /// Retries after the first attempt (spec §16.6).
    #[serde(default = "default_webhook_retries")]
    pub max_retries: u32,
    /// The wait before the first retry; every further one is
    /// `backoff_factor` times as long.
    #[serde(default = "default_first_retry")]
    pub first_retry: HumanDuration,
    /// How much longer each wait is than the one before.
    #[serde(default = "default_backoff_factor")]
    pub backoff_factor: u32,
    /// The share by which a wait is moved either way, so that events that
    /// failed together do not come back together.
    #[serde(default = "default_jitter")]
    pub jitter: f64,
    /// The longest a receiver's `Retry-After` is honoured for.
    #[serde(default = "default_max_retry_after")]
    pub max_retry_after: HumanDuration,
    /// How often the sender looks for events that are due.
    #[serde(default = "default_webhook_poll")]
    pub poll_interval: HumanDuration,
}

const fn default_webhook_timeout() -> HumanDuration {
    HumanDuration::from_secs(10)
}

const fn default_webhook_retries() -> u32 {
    5
}

const fn default_first_retry() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_backoff_factor() -> u32 {
    4
}

const fn default_jitter() -> f64 {
    0.2
}

const fn default_max_retry_after() -> HumanDuration {
    HumanDuration::from_hours(1)
}

const fn default_webhook_poll() -> HumanDuration {
    HumanDuration::from_secs(1)
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            private_origins: Vec::new(),
            timeout: default_webhook_timeout(),
            max_retries: default_webhook_retries(),
            first_retry: default_first_retry(),
            backoff_factor: default_backoff_factor(),
            jitter: default_jitter(),
            max_retry_after: default_max_retry_after(),
            poll_interval: default_webhook_poll(),
        }
    }
}

/// The origin of a URL as `webhooks.allowed_origins` compares one:
/// `scheme://host:port`, the port always written. `None` for a URL without
/// a host or a known port.
pub fn origin_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    Some(format!("{}://{host}:{port}", url.scheme()))
}

/// Whether a list of origins names the origin of a URL.
fn names(origins: &[String], url: &Url) -> bool {
    origin_of(url).is_some_and(|origin| {
        origins.iter().any(|listed| {
            Url::parse(listed)
                .ok()
                .and_then(|listed| origin_of(&listed))
                .is_some_and(|listed| listed == origin)
        })
    })
}

impl WebhooksConfig {
    /// Whether the operator listed the receiver of this URL: a job may
    /// name no other.
    pub fn allows(&self, url: &Url) -> bool {
        names(&self.allowed_origins, url)
    }

    /// Whether the receiver of this URL is one the operator said sits on
    /// its own network.
    pub fn is_private(&self, url: &Url) -> bool {
        self.allows(url) && names(&self.private_origins, url)
    }

    /// The wait before retry number `retry` (1 for the first), before it is
    /// moved by the jitter: `first_retry` times `backoff_factor` to the
    /// power of `retry - 1`.
    pub fn backoff(&self, retry: u32) -> Duration {
        let factor = u64::from(self.backoff_factor).saturating_pow(retry.saturating_sub(1));
        let millis = u64::try_from(self.first_retry.get().as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(factor);
        Duration::from_millis(millis)
    }
}

/// What can be judged about the `jobs` and `webhooks` sections alone.
pub(super) fn validate(jobs: &JobsConfig, webhooks: &WebhooksConfig) -> Result<(), String> {
    if jobs.max_items == 0 || jobs.max_items > MAX_JOB_ITEMS {
        return Err(format!(
            "jobs.max_items must be between 1 and {MAX_JOB_ITEMS}"
        ));
    }
    if jobs.max_running == 0 {
        return Err("jobs.max_running must be at least 1".to_owned());
    }
    if jobs.max_items_in_flight == 0 {
        return Err("jobs.max_items_in_flight must be at least 1".to_owned());
    }
    if jobs.max_age.get() < Duration::from_secs(60) {
        return Err("jobs.max_age must be at least 1m".to_owned());
    }
    if jobs.retention_days == 0 {
        return Err("jobs.retention_days must be at least 1".to_owned());
    }
    if jobs.lease.get() < Duration::from_secs(1) {
        return Err("jobs.lease must be at least 1s".to_owned());
    }
    if jobs.poll_interval.get() < Duration::from_millis(10) {
        return Err("jobs.poll_interval must be at least 10ms".to_owned());
    }
    if jobs.poll_interval.get() >= jobs.lease.get() {
        return Err("jobs.poll_interval must be shorter than jobs.lease".to_owned());
    }
    if jobs.idempotency_ttl.get() < Duration::from_secs(1) {
        return Err("jobs.idempotency_ttl must be at least 1s".to_owned());
    }

    for (key, origins) in [
        ("webhooks.allowed_origins", &webhooks.allowed_origins),
        ("webhooks.private_origins", &webhooks.private_origins),
    ] {
        for origin in origins {
            let exact = Url::parse(origin).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
            });
            if exact.is_none() {
                return Err(format!(
                    "{key}: {origin:?} is not an origin (scheme://host[:port], nothing else)"
                ));
            }
        }
    }
    for origin in &webhooks.private_origins {
        let listed = Url::parse(origin)
            .ok()
            .is_some_and(|url| webhooks.allows(&url));
        if !listed {
            return Err(format!(
                "webhooks.private_origins: {origin:?} is not in webhooks.allowed_origins"
            ));
        }
    }
    if webhooks.timeout.get() < Duration::from_millis(100) {
        return Err("webhooks.timeout must be at least 100ms".to_owned());
    }
    if webhooks.max_retries > MAX_WEBHOOK_RETRIES {
        return Err(format!(
            "webhooks.max_retries must be at most {MAX_WEBHOOK_RETRIES}"
        ));
    }
    if webhooks.first_retry.get() < Duration::from_secs(1) {
        return Err("webhooks.first_retry must be at least 1s".to_owned());
    }
    if !(1..=10).contains(&webhooks.backoff_factor) {
        return Err("webhooks.backoff_factor must be between 1 and 10".to_owned());
    }
    if !(0.0..=0.5).contains(&webhooks.jitter) {
        return Err("webhooks.jitter must be between 0 and 0.5".to_owned());
    }
    if webhooks.poll_interval.get() < Duration::from_millis(10) {
        return Err("webhooks.poll_interval must be at least 10ms".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use url::Url;

    use super::{JobsConfig, WebhooksConfig, origin_of, validate};
    use crate::duration::HumanDuration;

    #[test]
    fn the_defaults_are_the_specs() {
        let jobs = JobsConfig::default();
        let webhooks = WebhooksConfig::default();

        assert_eq!(validate(&jobs, &webhooks), Ok(()));
        assert_eq!(jobs.max_items, 10_000);
        assert_eq!(jobs.max_age.get(), Duration::from_hours(168));
        assert_eq!(jobs.retention(), Duration::from_hours(14 * 24));
        assert_eq!(jobs.idempotency_ttl.get(), Duration::from_hours(24));
        assert_eq!(webhooks.max_retries, 5);
        // No receiver is listed, so no job may have a webhook.
        assert!(webhooks.allowed_origins.is_empty() && webhooks.private_origins.is_empty());
        assert!(!webhooks.allows(&Url::parse("https://hooks.example.org/kage").unwrap()));
    }

    #[test]
    fn the_waits_between_retries_grow_fourfold_from_half_a_minute() {
        let webhooks = WebhooksConfig::default();

        let waits: Vec<u64> = (1..=5)
            .map(|retry| webhooks.backoff(retry).as_secs())
            .collect();

        assert_eq!(waits, [30, 120, 480, 1920, 7680]);
        // A number of retries nobody configures does not overflow.
        assert_eq!(webhooks.backoff(0), Duration::from_secs(30));
        assert!(webhooks.backoff(200) > Duration::from_hours(24 * 365));
    }

    #[test]
    fn a_listed_origin_is_matched_by_scheme_host_and_port() {
        let webhooks = WebhooksConfig {
            allowed_origins: vec![
                "http://app:3000".to_owned(),
                "https://hooks.example.org".to_owned(),
            ],
            private_origins: vec!["http://app:3000".to_owned()],
            ..WebhooksConfig::default()
        };
        assert_eq!(validate(&JobsConfig::default(), &webhooks), Ok(()));
        let allows = |text: &str| webhooks.allows(&Url::parse(text).unwrap());
        let private = |text: &str| webhooks.is_private(&Url::parse(text).unwrap());

        assert!(allows("http://app:3000/hooks/kage?x=1"));
        assert!(allows("https://hooks.example.org/kage"));
        // The default port is the port.
        assert!(allows("https://hooks.example.org:443/kage"));
        // Another port, another scheme, another host: another origin.
        assert!(!allows("http://app:3001/hooks"));
        assert!(!allows("https://app:3000/hooks"));
        assert!(!allows("http://hooks.example.org/kage"));
        assert!(!allows("http://app.evil.example:3000/"));
        assert!(!allows("https://target.example/"));
        // Only what is listed as the operator's own may be private.
        assert!(private("http://app:3000/hooks"));
        assert!(!private("https://hooks.example.org/kage"));
        assert!(!private("http://other:3000/"));

        assert_eq!(
            origin_of(&Url::parse("https://Example.org/a?b").unwrap()).as_deref(),
            Some("https://example.org:443")
        );
        assert_eq!(origin_of(&Url::parse("data:text/plain,x").unwrap()), None);
    }

    #[test]
    fn what_cannot_work_is_refused() {
        let jobs = JobsConfig::default();
        let webhooks = WebhooksConfig::default();
        let refused =
            |jobs: JobsConfig, webhooks: &WebhooksConfig| validate(&jobs, webhooks).unwrap_err();

        assert!(
            refused(
                JobsConfig {
                    max_items: 0,
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.max_items")
        );
        assert!(
            refused(
                JobsConfig {
                    max_items: 1_000_000,
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.max_items")
        );
        assert!(
            refused(
                JobsConfig {
                    max_running: 0,
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.max_running")
        );
        assert!(
            refused(
                JobsConfig {
                    max_items_in_flight: 0,
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.max_items_in_flight")
        );
        assert!(
            refused(
                JobsConfig {
                    max_age: HumanDuration::from_secs(5),
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.max_age")
        );
        assert!(
            refused(
                JobsConfig {
                    retention_days: 0,
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.retention_days")
        );
        assert!(
            refused(
                JobsConfig {
                    lease: HumanDuration::new(Duration::from_millis(500)),
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.lease")
        );
        assert!(
            refused(
                JobsConfig {
                    poll_interval: HumanDuration::new(Duration::from_millis(1)),
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.poll_interval")
        );
        assert!(
            refused(
                JobsConfig {
                    poll_interval: HumanDuration::from_mins(5),
                    ..jobs
                },
                &webhooks
            )
            .contains("shorter than jobs.lease")
        );
        assert!(
            refused(
                JobsConfig {
                    idempotency_ttl: HumanDuration::new(Duration::ZERO),
                    ..jobs
                },
                &webhooks
            )
            .contains("jobs.idempotency_ttl")
        );

        for origin in [
            "app:3000",
            "http://app:3000/hooks",
            "ftp://files.lan",
            "http://user@app:3000",
            "http://app:3000/?x=1",
        ] {
            let listed = WebhooksConfig {
                allowed_origins: vec![origin.to_owned()],
                ..webhooks.clone()
            };
            assert!(
                refused(jobs, &listed).contains("webhooks.allowed_origins"),
                "{origin}"
            );
        }
        // A private origin that no job may name anyway is a mistake.
        let unlisted = WebhooksConfig {
            allowed_origins: vec!["https://hooks.example.org".to_owned()],
            private_origins: vec!["http://app:3000".to_owned()],
            ..webhooks.clone()
        };
        assert!(refused(jobs, &unlisted).contains("is not in webhooks.allowed_origins"));
        let malformed = WebhooksConfig {
            private_origins: vec!["app".to_owned()],
            ..webhooks.clone()
        };
        assert!(refused(jobs, &malformed).contains("webhooks.private_origins"));
        let with = |change: fn(&mut WebhooksConfig)| {
            let mut changed = WebhooksConfig::default();
            change(&mut changed);
            refused(jobs, &changed)
        };
        assert!(
            with(|webhooks| webhooks.timeout = HumanDuration::new(Duration::from_millis(5)))
                .contains("webhooks.timeout")
        );
        assert!(with(|webhooks| webhooks.max_retries = 50).contains("webhooks.max_retries"));
        assert!(
            with(|webhooks| webhooks.first_retry = HumanDuration::new(Duration::ZERO))
                .contains("webhooks.first_retry")
        );
        assert!(with(|webhooks| webhooks.backoff_factor = 0).contains("webhooks.backoff_factor"));
        assert!(with(|webhooks| webhooks.jitter = 0.9).contains("webhooks.jitter"));
        assert!(
            with(|webhooks| webhooks.poll_interval = HumanDuration::new(Duration::ZERO))
                .contains("webhooks.poll_interval")
        );
    }
}
