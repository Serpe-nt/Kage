//! Operator alerts: conditions that need a human, raised by any subsystem
//! and delivered through an [`AlertSink`].
//!
//! The first alert was a policy file that fails validation on reload (spec
//! §17). The proxy pool adds its own kinds (spec §7); later milestones add
//! more (missing wreq profile, self-test regressions).

use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How urgent an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Worth recording; no action needed.
    Info,
    /// Degraded but working; look at it soon.
    Warning,
    /// Something is not working as configured; act now.
    Critical,
}

impl AlertSeverity {
    /// The stable lowercase name used in logs and the store.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// What an alert is about. The name is stable: dashboards and alert rules
/// match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlertKind {
    /// A policy file changed on disk but failed to parse or validate. The
    /// last good policy stays in force.
    PolicyReloadFailed,
    /// A proxy let the host's own address through. The proxy is parked in
    /// backoff.
    ProxyLeak,
    /// At least one proxy leaked the host address: nothing may be fetched
    /// until a probe through that proxy shows a distinct exit address again
    /// or an operator clears the flag.
    EgressUnsafe,
    /// A geo lookup placed a proxy's exit address in another country than
    /// the provider states, twice in a row.
    ProxyGeoMismatch,
    /// The reputation provider scored a proxy's address as bad.
    ProxyReputationBad,
    /// The reputation provider classifies a residential proxy's address as
    /// data center or hosting space.
    ReputationUsageTypeSuspicious,
    /// A proxy qualifies for replacement but this month's budget, or the
    /// plan's own quota, is used up. The proxy stays where it is.
    ReplacementBudgetExhausted,
    /// A replacement could not be requested or the provider failed it.
    ReplacementFailed,
    /// More than half of the pool failed its latest probe at once: the
    /// cause is on the path, not in the addresses. Failure counters are
    /// frozen while it lasts.
    PoolOutageSuspected,
    /// A quarter or more of the pool is unusable (spec risk R4).
    PoolCapacityLow,
    /// The provider's list came back empty or much shorter than the known
    /// pool. Nothing was retired on its word.
    ProviderListSuspect,
    /// The provider replaced a proxy by itself.
    ProviderReplacedProxy,
    /// The provider account is set to replace proxies automatically, so an
    /// address can change without Kage deciding it.
    ProviderAutoReplaceEnabled,
    /// The provider account needs attention: paused, throttled, replacement
    /// quota used up, or fewer proxies than the plan promises.
    ProviderAccount,
    /// The provider API could not be reached or refused the request.
    ProviderSyncFailed,
    /// The host's own public address could not be learned, so the leak
    /// check cannot run.
    HostIpUnknown,
    /// There is no emulation profile for an engine's browser major, so its
    /// personas cannot use the http path (spec risk R7).
    HttpProfileMissing,
    /// The fingerprint self-test of the http path saw another JA4, HTTP/2
    /// fingerprint or header order than the one recorded for the profile.
    HttpSelftestFailed,
}

impl AlertKind {
    /// Every kind, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::PolicyReloadFailed,
        Self::ProxyLeak,
        Self::EgressUnsafe,
        Self::ProxyGeoMismatch,
        Self::ProxyReputationBad,
        Self::ReputationUsageTypeSuspicious,
        Self::ReplacementBudgetExhausted,
        Self::ReplacementFailed,
        Self::PoolOutageSuspected,
        Self::PoolCapacityLow,
        Self::ProviderListSuspect,
        Self::ProviderReplacedProxy,
        Self::ProviderAutoReplaceEnabled,
        Self::ProviderAccount,
        Self::ProviderSyncFailed,
        Self::HostIpUnknown,
        Self::HttpProfileMissing,
        Self::HttpSelftestFailed,
    ];

    /// The stable `snake_case` name used in logs and the store.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PolicyReloadFailed => "policy_reload_failed",
            Self::ProxyLeak => "proxy_leak",
            Self::EgressUnsafe => "egress_unsafe",
            Self::ProxyGeoMismatch => "proxy_geo_mismatch",
            Self::ProxyReputationBad => "proxy_reputation_bad",
            Self::ReputationUsageTypeSuspicious => "reputation_usage_type_suspicious",
            Self::ReplacementBudgetExhausted => "replacement_budget_exhausted",
            Self::ReplacementFailed => "replacement_failed",
            Self::PoolOutageSuspected => "pool_outage_suspected",
            Self::PoolCapacityLow => "pool_capacity_low",
            Self::ProviderListSuspect => "provider_list_suspect",
            Self::ProviderReplacedProxy => "provider_replaced_proxy",
            Self::ProviderAutoReplaceEnabled => "provider_auto_replace_enabled",
            Self::ProviderAccount => "provider_account",
            Self::ProviderSyncFailed => "provider_sync_failed",
            Self::HostIpUnknown => "host_ip_unknown",
            Self::HttpProfileMissing => "http_profile_missing",
            Self::HttpSelftestFailed => "http_selftest_failed",
        }
    }
}

/// One alert event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// What happened.
    pub kind: AlertKind,
    /// How urgent it is.
    pub severity: AlertSeverity,
    /// One sentence for a human. Never contains secrets.
    pub message: String,
    /// Structured context, for example the file and the validation error.
    pub details: serde_json::Value,
    /// The one thing the alert is about, for example a proxy id. `None`
    /// when it concerns a whole subsystem.
    #[serde(default)]
    pub subject: Option<Uuid>,
    /// When it was raised.
    pub at: DateTime<Utc>,
}

impl Alert {
    /// Builds an alert stamped with the current time.
    pub fn new(
        kind: AlertKind,
        severity: AlertSeverity,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            severity,
            message: message.into(),
            details,
            subject: None,
            at: Utc::now(),
        }
    }

    /// Names the one thing the alert is about.
    #[must_use]
    pub fn about(mut self, subject: impl Into<Uuid>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Stamps the alert with a given time instead of the current one.
    #[must_use]
    pub fn at(mut self, at: DateTime<Utc>) -> Self {
        self.at = at;
        self
    }
}

/// Receives alerts. Implementations must not block for long: `raise` is
/// called from whichever thread noticed the problem.
pub trait AlertSink: Send + Sync {
    /// Delivers one alert.
    fn raise(&self, alert: Alert);
}

impl<F> AlertSink for F
where
    F: Fn(Alert) + Send + Sync,
{
    fn raise(&self, alert: Alert) {
        self(alert);
    }
}

/// Writes every alert to the log, at a level that follows its severity.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingAlertSink;

impl AlertSink for TracingAlertSink {
    fn raise(&self, alert: Alert) {
        let kind = alert.kind.as_str();
        let subject = alert.subject.map(|subject| subject.to_string());
        let subject = subject.as_deref().unwrap_or("");
        match alert.severity {
            AlertSeverity::Info => {
                tracing::info!(alert = kind, subject, details = %alert.details, "{}", alert.message);
            }
            AlertSeverity::Warning => {
                tracing::warn!(alert = kind, subject, details = %alert.details, "{}", alert.message);
            }
            AlertSeverity::Critical => {
                tracing::error!(alert = kind, subject, details = %alert.details, "{}", alert.message);
            }
        }
    }
}

/// Keeps alerts in memory. Used by tests and by `kage config check`, which
/// reports problems itself instead of logging them.
#[derive(Debug, Default)]
pub struct MemoryAlertSink {
    alerts: Mutex<Vec<Alert>>,
}

impl MemoryAlertSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every alert raised so far, oldest first.
    pub fn alerts(&self) -> Vec<Alert> {
        self.alerts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The kinds of the alerts raised so far, oldest first.
    pub fn kinds(&self) -> Vec<AlertKind> {
        self.alerts().into_iter().map(|alert| alert.kind).collect()
    }
}

impl AlertSink for MemoryAlertSink {
    fn raise(&self, alert: Alert) {
        self.alerts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(alert);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::{Alert, AlertKind, AlertSeverity, AlertSink, MemoryAlertSink, TracingAlertSink};
    use crate::ids::ProxyId;

    fn sample(severity: AlertSeverity) -> Alert {
        Alert::new(
            AlertKind::PolicyReloadFailed,
            severity,
            "policy reload failed",
            json!({ "file": "domains.yaml" }),
        )
    }

    #[test]
    fn names_are_stable() {
        let names: Vec<&str> = AlertKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(
            names,
            [
                "policy_reload_failed",
                "proxy_leak",
                "egress_unsafe",
                "proxy_geo_mismatch",
                "proxy_reputation_bad",
                "reputation_usage_type_suspicious",
                "replacement_budget_exhausted",
                "replacement_failed",
                "pool_outage_suspected",
                "pool_capacity_low",
                "provider_list_suspect",
                "provider_replaced_proxy",
                "provider_auto_replace_enabled",
                "provider_account",
                "provider_sync_failed",
                "host_ip_unknown",
                "http_profile_missing",
                "http_selftest_failed",
            ]
        );
        // The serialized name is the stable name.
        for kind in AlertKind::ALL {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::from(kind.as_str())
            );
        }
        assert_eq!(AlertSeverity::Info.as_str(), "info");
        assert_eq!(AlertSeverity::Warning.as_str(), "warning");
        assert_eq!(AlertSeverity::Critical.as_str(), "critical");
        assert!(AlertSeverity::Critical > AlertSeverity::Warning);

        let json = serde_json::to_value(sample(AlertSeverity::Critical)).unwrap();
        assert_eq!(json["kind"], "policy_reload_failed");
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["details"]["file"], "domains.yaml");
        assert!(json["subject"].is_null());
    }

    #[test]
    fn an_alert_can_name_its_subject_and_its_time() {
        let proxy = ProxyId::new();
        let at = chrono::Utc::now() - chrono::Duration::hours(1);

        let alert = sample(AlertSeverity::Warning).about(proxy).at(at);

        assert_eq!(alert.subject, Some(proxy.as_uuid()));
        assert_eq!(alert.at, at);
    }

    #[test]
    fn memory_sink_keeps_alerts_in_order() {
        let sink = MemoryAlertSink::new();

        sink.raise(sample(AlertSeverity::Warning));
        sink.raise(sample(AlertSeverity::Critical));

        let alerts = sink.alerts();
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
        assert_eq!(alerts[1].severity, AlertSeverity::Critical);
        assert_eq!(
            sink.kinds(),
            [AlertKind::PolicyReloadFailed, AlertKind::PolicyReloadFailed]
        );
    }

    #[test]
    fn closures_are_sinks() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let sink: Box<dyn AlertSink> = Box::new(move |_alert: Alert| {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        sink.raise(sample(AlertSeverity::Info));

        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tracing_sink_accepts_every_severity() {
        let sink = TracingAlertSink;
        for severity in [
            AlertSeverity::Info,
            AlertSeverity::Warning,
            AlertSeverity::Critical,
        ] {
            sink.raise(sample(severity));
            sink.raise(sample(severity).about(ProxyId::new()));
        }
    }
}
