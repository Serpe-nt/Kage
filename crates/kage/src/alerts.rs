//! Where operator alerts go in the binary: into the log, and into the
//! `health_events` table.

use std::sync::Arc;

use kage_core::alert::{Alert, AlertKind, AlertSink, TracingAlertSink};
use kage_core::domain::SubjectKind;
use kage_store::Store;
use tokio::sync::mpsc;

/// What an alert is about, for the health event it is stored as.
pub(crate) fn subject_of(kind: AlertKind) -> SubjectKind {
    match kind {
        AlertKind::PolicyReloadFailed => SubjectKind::Policy,
        AlertKind::ProxyLeak
        | AlertKind::ProxyGeoMismatch
        | AlertKind::ProxyReputationBad
        | AlertKind::ReputationUsageTypeSuspicious
        | AlertKind::ReplacementBudgetExhausted
        | AlertKind::ReplacementFailed
        | AlertKind::ProviderReplacedProxy
        // The proxy that has no persona yet.
        | AlertKind::PersonaGenerationFailed
        // A stand-in's self-test (`kage selftest http --proxy`) is about
        // the proxy it ran through; a persona's names the persona itself
        // (`Alert::about_persona`).
        | AlertKind::HttpSelftestFailed => SubjectKind::Proxy,
        AlertKind::BrowserSelftestFailed => SubjectKind::Persona,
        // The pool as a whole, the provider account and the host. Kinds
        // added by later milestones land here too until they are mapped.
        _ => SubjectKind::System,
    }
}

/// Logs every alert and hands it to a task that stores it as a health
/// event. Raising an alert never blocks and never fails the caller.
struct RecordingAlertSink {
    sender: mpsc::UnboundedSender<Alert>,
}

impl AlertSink for RecordingAlertSink {
    fn raise(&self, alert: Alert) {
        TracingAlertSink.raise(alert.clone());
        // The receiver is gone only during shutdown; the alert is logged.
        let _ = self.sender.send(alert);
    }
}

/// Builds the alert sink of the server and the task that stores what it
/// receives.
pub(crate) fn recorder(store: Store) -> (Arc<dyn AlertSink>, tokio::task::JoinHandle<()>) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<Alert>();
    let task = tokio::spawn(async move {
        while let Some(alert) = receiver.recv().await {
            record(&store, &alert).await;
        }
    });
    (Arc::new(RecordingAlertSink { sender }), task)
}

/// Stores one alert as a health event. A failure is logged: the alert
/// itself already is.
pub(crate) async fn record(store: &Store, alert: &Alert) {
    // An alert that names what its subject is knows better than its kind.
    let subject = alert.subject_kind.unwrap_or_else(|| subject_of(alert.kind));
    if let Err(error) = store.health_events().record_alert(subject, alert).await {
        tracing::error!(%error, alert = alert.kind.as_str(), "cannot store the alert");
    }
}

#[cfg(test)]
mod tests {
    use kage_core::alert::AlertKind;
    use kage_core::domain::SubjectKind;

    use super::subject_of;

    #[test]
    fn every_alert_is_stored_about_the_right_kind_of_subject() {
        assert_eq!(
            subject_of(AlertKind::PolicyReloadFailed),
            SubjectKind::Policy
        );
        let about_one_proxy = [
            AlertKind::ProxyLeak,
            AlertKind::ProxyGeoMismatch,
            AlertKind::ProxyReputationBad,
            AlertKind::ReputationUsageTypeSuspicious,
            AlertKind::ReplacementBudgetExhausted,
            AlertKind::ReplacementFailed,
            AlertKind::ProviderReplacedProxy,
            AlertKind::HttpSelftestFailed,
            AlertKind::PersonaGenerationFailed,
        ];
        assert_eq!(
            subject_of(AlertKind::BrowserSelftestFailed),
            SubjectKind::Persona
        );
        let about_the_system = [
            AlertKind::EgressUnsafe,
            AlertKind::PoolOutageSuspected,
            AlertKind::PoolCapacityLow,
            AlertKind::ProviderListSuspect,
            AlertKind::ProviderAutoReplaceEnabled,
            AlertKind::ProviderAccount,
            AlertKind::ProviderSyncFailed,
            AlertKind::HostIpUnknown,
            AlertKind::HttpProfileMissing,
        ];
        for kind in about_one_proxy {
            assert_eq!(subject_of(kind), SubjectKind::Proxy, "{kind:?}");
        }
        for kind in about_the_system {
            assert_eq!(subject_of(kind), SubjectKind::System, "{kind:?}");
        }
        // Every kind is in exactly one of the lists above, or is one of
        // the two named by themselves.
        assert_eq!(
            2 + about_one_proxy.len() + about_the_system.len(),
            AlertKind::ALL.len()
        );
    }
}
