//! Operator alerts: conditions that need a human, raised by any subsystem
//! and delivered through an [`AlertSink`].
//!
//! The first alert is a policy file that fails validation on reload (spec
//! §17). Later milestones add their own kinds (replacement budget exhausted,
//! pool availability, missing wreq profile).

use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

impl AlertKind {
    /// The stable `snake_case` name used in logs and the store.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PolicyReloadFailed => "policy_reload_failed",
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
            at: Utc::now(),
        }
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
        match alert.severity {
            AlertSeverity::Info => {
                tracing::info!(alert = kind, details = %alert.details, "{}", alert.message);
            }
            AlertSeverity::Warning => {
                tracing::warn!(alert = kind, details = %alert.details, "{}", alert.message);
            }
            AlertSeverity::Critical => {
                tracing::error!(alert = kind, details = %alert.details, "{}", alert.message);
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
        assert_eq!(
            AlertKind::PolicyReloadFailed.as_str(),
            "policy_reload_failed"
        );
        assert_eq!(AlertSeverity::Info.as_str(), "info");
        assert_eq!(AlertSeverity::Warning.as_str(), "warning");
        assert_eq!(AlertSeverity::Critical.as_str(), "critical");
        assert!(AlertSeverity::Critical > AlertSeverity::Warning);

        let json = serde_json::to_value(sample(AlertSeverity::Critical)).unwrap();
        assert_eq!(json["kind"], "policy_reload_failed");
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["details"]["file"], "domains.yaml");
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
        }
    }
}
