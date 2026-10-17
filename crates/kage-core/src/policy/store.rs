//! Loading the three policy files and keeping them current (spec §17).
//!
//! The files are validated together: a domain entry that names a pacing
//! profile missing from `pacing-profiles.yaml` makes the whole set invalid.
//! A set that fails to load never replaces the one in use: the store keeps
//! the last good policy and raises an alert.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{DomainPolicies, PolicyError};
use crate::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use crate::classifier::{SignatureError, SignatureSet};
use crate::pacing::{PacingError, PacingProfiles};

/// File name of the per-domain policies.
pub const DOMAINS_FILE: &str = "domains.yaml";
/// File name of the block signatures.
pub const SIGNATURES_FILE: &str = "block-signatures.yaml";
/// File name of the pacing profiles.
pub const PACING_FILE: &str = "pacing-profiles.yaml";

/// How long the watcher waits for further changes before reloading, so that
/// an editor's several writes to one file cause one reload.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Why a policy directory could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum PolicyLoadError {
    /// A policy file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// `domains.yaml` is invalid.
    #[error("{}: {source}", path.display())]
    Domains {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: PolicyError,
    },
    /// `block-signatures.yaml` is invalid.
    #[error("{}: {source}", path.display())]
    Signatures {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: SignatureError,
    },
    /// `pacing-profiles.yaml` is invalid.
    #[error("{}: {source}", path.display())]
    Pacing {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: PacingError,
    },
}

impl PolicyLoadError {
    /// The file the error is about.
    pub fn path(&self) -> &Path {
        match self {
            Self::Read { path, .. }
            | Self::Domains { path, .. }
            | Self::Signatures { path, .. }
            | Self::Pacing { path, .. } => path,
        }
    }
}

/// The three policy files, validated together.
#[derive(Debug, Clone)]
pub struct PolicySet {
    /// `domains.yaml`.
    pub domains: DomainPolicies,
    /// `pacing-profiles.yaml`.
    pub pacing: PacingProfiles,
    /// `block-signatures.yaml`.
    pub signatures: SignatureSet,
}

impl PolicySet {
    /// Validates three YAML documents as one set. `dir` is only used to
    /// name the files in errors.
    ///
    /// # Errors
    ///
    /// Fails on the first file that does not parse or validate, or when
    /// `domains` names a pacing profile that `pacing` does not define.
    pub fn from_yaml(
        dir: &Path,
        domains: &str,
        pacing: &str,
        signatures: &str,
    ) -> Result<Self, PolicyLoadError> {
        let pacing =
            PacingProfiles::from_yaml(pacing).map_err(|source| PolicyLoadError::Pacing {
                path: dir.join(PACING_FILE),
                source,
            })?;
        let domains_error = |source| PolicyLoadError::Domains {
            path: dir.join(DOMAINS_FILE),
            source,
        };
        let domains = DomainPolicies::from_yaml(domains).map_err(domains_error)?;
        domains.check_pacing(&pacing).map_err(domains_error)?;
        let signatures =
            SignatureSet::from_yaml(signatures).map_err(|source| PolicyLoadError::Signatures {
                path: dir.join(SIGNATURES_FILE),
                source,
            })?;
        Ok(Self {
            domains,
            pacing,
            signatures,
        })
    }

    /// Reads and validates the three files in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when a file cannot be read or the set does not validate.
    pub fn load(dir: &Path) -> Result<Self, PolicyLoadError> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|source| PolicyLoadError::Read { path, source })
        };
        let domains = read(DOMAINS_FILE)?;
        let pacing = read(PACING_FILE)?;
        let signatures = read(SIGNATURES_FILE)?;
        Self::from_yaml(dir, &domains, &pacing, &signatures)
    }
}

/// The policy in force, replaceable while the process runs.
///
/// Readers take a cheap snapshot with [`PolicyStore::current`] and keep
/// using it for the whole request, so one request never sees two policies.
pub struct PolicyStore {
    dir: PathBuf,
    current: RwLock<Arc<PolicySet>>,
    alerts: Arc<dyn AlertSink>,
}

impl std::fmt::Debug for PolicyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl PolicyStore {
    /// Loads the policy directory. There is no last good policy yet, so an
    /// invalid directory is an error here.
    ///
    /// # Errors
    ///
    /// Fails when the directory does not load.
    pub fn open(
        dir: impl Into<PathBuf>,
        alerts: Arc<dyn AlertSink>,
    ) -> Result<Self, PolicyLoadError> {
        let dir = dir.into();
        let set = PolicySet::load(&dir)?;
        Ok(Self {
            dir,
            current: RwLock::new(Arc::new(set)),
            alerts,
        })
    }

    /// The directory the policy files are read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A snapshot of the policy in force.
    pub fn current(&self) -> Arc<PolicySet> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Reads the directory again. On success the new policy takes effect for
    /// every later [`PolicyStore::current`]. On failure the policy in force
    /// stays, an alert is raised, and the error is returned.
    ///
    /// # Errors
    ///
    /// Fails when the directory does not load; nothing changes in that case.
    pub fn reload(&self) -> Result<Arc<PolicySet>, PolicyLoadError> {
        match PolicySet::load(&self.dir) {
            Ok(set) => {
                let set = Arc::new(set);
                *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&set);
                tracing::info!(
                    dir = %self.dir.display(),
                    domains = set.domains.domains().len(),
                    pacing_profiles = set.pacing.as_map().len(),
                    signature_rules = set.signatures.rules().len(),
                    "policies reloaded"
                );
                Ok(set)
            }
            Err(error) => {
                self.alerts.raise(Alert::new(
                    AlertKind::PolicyReloadFailed,
                    AlertSeverity::Critical,
                    "a policy file is invalid; the last good policy stays in force",
                    serde_json::json!({
                        "file": error.path().display().to_string(),
                        "error": error.to_string(),
                    }),
                ));
                Err(error)
            }
        }
    }

    /// Starts watching the directory and reloads after every change.
    ///
    /// The directory is watched, not the files: editors and config
    /// management replace files by renaming, which a file watch would lose.
    /// Dropping the returned handle stops the watch.
    ///
    /// # Errors
    ///
    /// Fails when the operating system refuses the watch.
    pub fn watch(self: &Arc<Self>) -> Result<PolicyWatcher, WatchError> {
        let (sender, receiver) = mpsc::channel::<()>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let relevant = match event {
                    Ok(event) => is_policy_change(&event),
                    // A watch error may hide a change: reload to be safe.
                    Err(_) => true,
                };
                if relevant {
                    // The receiver is gone only while the watcher shuts down.
                    let _ = sender.send(());
                }
            })
            .map_err(WatchError::from_notify)?;
        watcher
            .watch(&self.dir, RecursiveMode::NonRecursive)
            .map_err(WatchError::from_notify)?;

        let store = Arc::clone(self);
        let thread = std::thread::Builder::new()
            .name("kage-policy-watch".to_owned())
            .spawn(move || {
                // Ends when the watcher, which owns the sender, is dropped.
                while receiver.recv().is_ok() {
                    loop {
                        match receiver.recv_timeout(DEBOUNCE) {
                            Ok(()) => {}
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if let Err(error) = store.reload() {
                        tracing::error!(%error, "policy reload failed, keeping the last good policy");
                    }
                }
            })
            .map_err(WatchError::Thread)?;

        Ok(PolicyWatcher {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}

/// Whether a file system event can change what a reload would read.
fn is_policy_change(event: &notify::Event) -> bool {
    if event.kind.is_access() {
        return false;
    }
    let policy_file = |path: &PathBuf| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| [DOMAINS_FILE, SIGNATURES_FILE, PACING_FILE].contains(&name))
    };
    // Events without paths (rescans, overflows) may hide a change.
    event.paths.is_empty() || event.paths.iter().any(policy_file)
}

/// Why the policy directory could not be watched.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The file system watch could not be set up.
    #[error("cannot watch the policy directory: {0}")]
    Notify(String),
    /// The reload thread could not be started.
    #[error("cannot start the policy reload thread: {0}")]
    Thread(std::io::Error),
}

impl WatchError {
    // Taken by value so it can be passed straight to `map_err`.
    #[allow(clippy::needless_pass_by_value)]
    fn from_notify(error: notify::Error) -> Self {
        Self::Notify(error.to_string())
    }
}

/// Keeps a [`PolicyStore`] watching its directory. Dropping it stops the
/// watch and waits for a reload in progress to finish.
pub struct PolicyWatcher {
    watcher: Option<RecommendedWatcher>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PolicyWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyWatcher").finish_non_exhaustive()
    }
}

impl Drop for PolicyWatcher {
    fn drop(&mut self) {
        // Dropping the watcher drops the sender, which ends the thread.
        drop(self.watcher.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::{
        DOMAINS_FILE, PACING_FILE, PolicyLoadError, PolicySet, PolicyStore, SIGNATURES_FILE,
    };
    use crate::alert::{AlertKind, AlertSeverity, AlertSink, MemoryAlertSink};
    use crate::domain::RenderMode;
    use crate::pacing::PacingError;
    use crate::policy::PolicyError;

    const DOMAINS: &str =
        "defaults:\n  pacing: human\ndomains:\n  example.org:\n    render: browser\n";
    const PACING: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
";
    const SIGNATURES: &str = "
version: 1
rules:
  - { id: http-403, vendor: generic, type: http_403, outcome: forbidden, confidence: 0.5, when: { status: 403 } }
";

    fn policy_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(DOMAINS_FILE), DOMAINS).unwrap();
        std::fs::write(dir.path().join(PACING_FILE), PACING).unwrap();
        std::fs::write(dir.path().join(SIGNATURES_FILE), SIGNATURES).unwrap();
        dir
    }

    fn open(dir: &Path) -> (Arc<PolicyStore>, Arc<MemoryAlertSink>) {
        let alerts = Arc::new(MemoryAlertSink::new());
        let sink: Arc<dyn AlertSink> = alerts.clone();
        let store = PolicyStore::open(dir, sink).unwrap();
        (Arc::new(store), alerts)
    }

    fn render_of(store: &PolicyStore, host: &str) -> RenderMode {
        store.current().domains.resolve(host).render
    }

    /// Polls until `condition` holds; file system events arrive when the
    /// kernel delivers them, so the tests wait for the effect, with a limit.
    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn a_valid_directory_loads_as_one_set() {
        let dir = policy_dir();

        let set = PolicySet::load(dir.path()).unwrap();

        assert_eq!(set.domains.domains().len(), 1);
        assert_eq!(set.pacing.names().collect::<Vec<_>>(), ["human"]);
        assert_eq!(set.signatures.rules().len(), 1);
    }

    #[test]
    fn a_missing_file_is_a_read_error_naming_the_file() {
        let dir = policy_dir();
        std::fs::remove_file(dir.path().join(SIGNATURES_FILE)).unwrap();

        let error = PolicySet::load(dir.path()).unwrap_err();

        assert!(matches!(error, PolicyLoadError::Read { .. }));
        assert_eq!(error.path(), dir.path().join(SIGNATURES_FILE));
        assert!(error.to_string().contains(SIGNATURES_FILE));
    }

    #[test]
    fn each_invalid_file_is_reported_with_its_path() {
        let dir = Path::new("policies");

        let error = PolicySet::from_yaml(dir, "domains: [", PACING, SIGNATURES).unwrap_err();
        assert!(matches!(
            error,
            PolicyLoadError::Domains {
                source: PolicyError::Parse(_),
                ..
            }
        ));
        assert_eq!(error.path(), dir.join(DOMAINS_FILE));

        let error = PolicySet::from_yaml(dir, DOMAINS, "human: 5", SIGNATURES).unwrap_err();
        assert!(matches!(
            error,
            PolicyLoadError::Pacing {
                source: PacingError::Parse(_),
                ..
            }
        ));
        assert_eq!(error.path(), dir.join(PACING_FILE));

        let error =
            PolicySet::from_yaml(dir, DOMAINS, PACING, "version: 9\nrules: []").unwrap_err();
        assert!(matches!(error, PolicyLoadError::Signatures { .. }));
        assert_eq!(error.path(), dir.join(SIGNATURES_FILE));
        assert!(error.to_string().starts_with("policies"));
    }

    #[test]
    fn a_domain_naming_an_undefined_pacing_profile_invalidates_the_set() {
        let domains = "domains:\n  example.org:\n    pacing: warp\n";

        let error = PolicySet::from_yaml(Path::new("p"), domains, PACING, SIGNATURES).unwrap_err();

        assert!(matches!(
            &error,
            PolicyLoadError::Domains { source: PolicyError::UnknownPacing { profile, .. }, .. } if profile == "warp"
        ));
    }

    #[test]
    fn opening_an_invalid_directory_fails() {
        let dir = policy_dir();
        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            "defaults:\n  render: sideways\n",
        )
        .unwrap();
        let alerts = Arc::new(MemoryAlertSink::new());

        let sink: Arc<dyn AlertSink> = alerts.clone();
        let error = PolicyStore::open(dir.path(), sink).unwrap_err();

        assert!(matches!(error, PolicyLoadError::Domains { .. }));
        // Nothing was in force before, so there is nothing to alert about.
        assert_eq!(alerts.alerts(), []);
    }

    #[test]
    fn reload_picks_up_a_valid_change() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());
        assert_eq!(store.dir(), dir.path());
        assert_eq!(render_of(&store, "example.org"), RenderMode::Browser);
        let before = store.current();

        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            DOMAINS.replace("browser", "http"),
        )
        .unwrap();
        let reloaded = store.reload().unwrap();

        assert_eq!(
            reloaded.domains.resolve("example.org").render,
            RenderMode::Http
        );
        assert_eq!(render_of(&store, "example.org"), RenderMode::Http);
        // A snapshot taken earlier keeps the policy it was taken with.
        assert_eq!(
            before.domains.resolve("example.org").render,
            RenderMode::Browser
        );
        assert_eq!(alerts.alerts(), []);
        assert!(format!("{store:?}").contains("PolicyStore"));
    }

    #[test]
    fn an_invalid_file_keeps_the_last_good_policy_and_raises_an_alert() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());

        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            "domains:\n  example.org:\n    render: sideways\n",
        )
        .unwrap();
        let error = store.reload().unwrap_err();

        assert!(matches!(error, PolicyLoadError::Domains { .. }));
        assert_eq!(render_of(&store, "example.org"), RenderMode::Browser);

        let raised = alerts.alerts();
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].kind, AlertKind::PolicyReloadFailed);
        assert_eq!(raised[0].severity, AlertSeverity::Critical);
        assert!(
            raised[0].details["file"]
                .as_str()
                .unwrap()
                .ends_with(DOMAINS_FILE)
        );
        assert!(
            raised[0].details["error"]
                .as_str()
                .unwrap()
                .contains("sideways")
        );

        // Fixing the file brings the store back in step, with no new alert.
        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            DOMAINS.replace("browser", "http"),
        )
        .unwrap();
        store.reload().unwrap();
        assert_eq!(render_of(&store, "example.org"), RenderMode::Http);
        assert_eq!(alerts.alerts().len(), 1);

        // A domain said twice is an error (F45): the policy in force stays,
        // and the alert says on which line.
        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            "domains:\n  example.org: { render: browser }\n  example.org: { render: http }\n",
        )
        .unwrap();
        assert!(store.reload().is_err());
        assert_eq!(render_of(&store, "example.org"), RenderMode::Http);
        let raised = alerts.alerts();
        assert_eq!(raised.len(), 2);
        let said = raised[1].details["error"].as_str().unwrap();
        assert!(said.contains("line 3"), "{said}");
    }

    #[test]
    fn every_kind_of_invalid_file_keeps_the_last_good_policy() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());

        std::fs::write(dir.path().join(PACING_FILE), "fast: {}").unwrap();
        assert!(matches!(
            store.reload(),
            Err(PolicyLoadError::Pacing { .. })
        ));
        std::fs::write(dir.path().join(PACING_FILE), PACING).unwrap();

        std::fs::write(dir.path().join(SIGNATURES_FILE), "version: 1\nrules: []\n").unwrap();
        assert!(matches!(
            store.reload(),
            Err(PolicyLoadError::Signatures { .. })
        ));
        std::fs::write(dir.path().join(SIGNATURES_FILE), SIGNATURES).unwrap();

        std::fs::remove_file(dir.path().join(DOMAINS_FILE)).unwrap();
        assert!(matches!(store.reload(), Err(PolicyLoadError::Read { .. })));

        assert_eq!(alerts.alerts().len(), 3);
        assert_eq!(render_of(&store, "example.org"), RenderMode::Browser);
        assert_eq!(store.current().signatures.rules().len(), 1);
    }

    #[test]
    fn the_watcher_reloads_after_a_file_changes() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());
        let watcher = store.watch().unwrap();
        assert!(format!("{watcher:?}").contains("PolicyWatcher"));

        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            DOMAINS.replace("browser", "http"),
        )
        .unwrap();

        eventually("the new policy is in force", || {
            render_of(&store, "example.org") == RenderMode::Http
        });
        assert_eq!(alerts.alerts(), []);
    }

    #[test]
    fn the_watcher_follows_a_file_replaced_by_rename() {
        let dir = policy_dir();
        let (store, _alerts) = open(dir.path());
        let _watcher = store.watch().unwrap();

        // Editors and config management write a temporary file and rename it.
        let staged = dir.path().join(".domains.yaml.tmp");
        std::fs::write(&staged, DOMAINS.replace("browser", "http")).unwrap();
        std::fs::rename(&staged, dir.path().join(DOMAINS_FILE)).unwrap();

        eventually("the renamed file is in force", || {
            render_of(&store, "example.org") == RenderMode::Http
        });
    }

    #[test]
    fn the_watcher_keeps_the_last_good_policy_and_recovers() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());
        let _watcher = store.watch().unwrap();

        std::fs::write(dir.path().join(DOMAINS_FILE), "defaults:\n  retries: 99\n").unwrap();
        eventually("the alert is raised", || !alerts.alerts().is_empty());
        assert_eq!(alerts.alerts()[0].kind, AlertKind::PolicyReloadFailed);
        assert_eq!(render_of(&store, "example.org"), RenderMode::Browser);

        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            DOMAINS.replace("browser", "http"),
        )
        .unwrap();
        eventually("the fixed file is in force", || {
            render_of(&store, "example.org") == RenderMode::Http
        });
    }

    #[test]
    fn unrelated_files_do_not_trigger_a_reload() {
        let dir = policy_dir();
        let (store, alerts) = open(dir.path());
        let _watcher = store.watch().unwrap();

        // Break a policy file without the watcher seeing a policy change is
        // not possible, so prove the filter the other way round: an
        // unrelated file changes, and the store still holds the same set.
        let before = store.current();
        std::fs::write(dir.path().join("README.md"), "notes").unwrap();
        std::thread::sleep(Duration::from_millis(600));

        assert!(Arc::ptr_eq(&before, &store.current()));
        assert_eq!(alerts.alerts(), []);
    }

    #[test]
    fn dropping_the_watcher_stops_reloading() {
        let dir = policy_dir();
        let (store, _alerts) = open(dir.path());
        let watcher = store.watch().unwrap();
        drop(watcher);

        std::fs::write(
            dir.path().join(DOMAINS_FILE),
            DOMAINS.replace("browser", "http"),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(600));

        assert_eq!(render_of(&store, "example.org"), RenderMode::Browser);
        // The store itself still works on demand.
        store.reload().unwrap();
        assert_eq!(render_of(&store, "example.org"), RenderMode::Http);
    }

    #[test]
    fn watching_a_missing_directory_fails() {
        let dir = policy_dir();
        let (store, _alerts) = open(dir.path());
        let path = dir.path().to_path_buf();
        drop(dir);
        assert!(!path.exists());

        let error = store.watch().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot watch the policy directory")
        );
    }
}
