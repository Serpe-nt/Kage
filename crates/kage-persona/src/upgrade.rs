//! A persona's move to the browser major its engine runs (spec §6.2,
//! `docs/runbooks/upgrading-an-engine.md`).
//!
//! An engine image that bundles a newer browser serves the personas an
//! older one made. A real user's browser updates itself and keeps its
//! profile, its cookies and its sessions; a persona does the same. The
//! first time its context is opened after the deploy the engine refuses
//! the old document (`browser_major_mismatch`). The manager then has the
//! engine's sidecar move the document to the browser it runs
//! (`POST /v1/personas/upgrade`, deterministic: the browser's version and
//! the `User-Agent` move, nothing of the device), checks that nothing else
//! moved, stores it with the new major and the wreq profile that major
//! resolves to (none for a browser-only major), records
//! `persona_browser_upgraded` with what the persona was before, and opens
//! the context as the moved persona. Its sessions, its per-domain state
//! and its http jar stay as they are.
//!
//! The move is one-way. Firefox moves a profile forward at its first start
//! under a newer major and does not give it back to an older one, so the
//! Firefox sidecar copies the profile to `.upgrade/<persona id>` on the
//! personas volume before that start. The copy is the way back to the
//! previous image; [`PersonaManager::sweep_upgrade_copies`] deletes it
//! `personas.upgrade_backup_days` after it was taken.

use std::io::ErrorKind;
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use kage_browser::contract::{PersonaSummary, UpgradePersonaRequest};
use kage_core::alert::AlertSeverity;
use kage_core::domain::{Engine, Persona, ProxyEndpoint};
use kage_store::repo::{BrowserUpgrade, Upgraded};
use serde_json::json;

use crate::PersonaError;
use crate::contexts::Lease;
use crate::identity::profile_for;
use crate::manager::PersonaManager;
use crate::sidecar::{domain_engine, domain_os, domain_screen};

/// Where the sidecars keep a profile's copy, below the personas volume.
pub const UPGRADE_DIR: &str = ".upgrade";

/// The note a sidecar writes into a copy: who, from which major, when.
const NOTE_FILE: &str = "upgrade.json";

/// The suffix of a copy that is still being written.
const PARTIAL: &str = ".partial";

impl PersonaManager {
    /// Takes the persona's browser context
    /// ([`crate::contexts::Contexts::acquire`]). When the engine runs a
    /// newer browser major than the persona was made for, the persona is
    /// moved first ([`Self::upgrade_browser`]) and the context is opened as
    /// the moved persona.
    ///
    /// Returns the lease, and the persona as it is stored now when it was
    /// moved: whoever goes on with it uses that one.
    ///
    /// # Errors
    ///
    /// What [`crate::contexts::Contexts::acquire`] and
    /// [`Self::upgrade_browser`] fail with; `browser_major_mismatch` as the
    /// engine said it when the sidecar has nothing to move.
    pub async fn open_browser(
        &self,
        persona: &Persona,
        endpoint: &ProxyEndpoint,
    ) -> Result<(Lease, Option<Persona>), PersonaError> {
        match self.contexts().acquire(persona, endpoint).await {
            Ok(lease) => Ok((lease, None)),
            Err(PersonaError::Browser(error)) if error.is_browser_major_mismatch() => {
                let Some(moved) = self.upgrade_browser(persona).await? else {
                    return Err(PersonaError::Browser(error));
                };
                let lease = self.contexts().acquire(&moved, endpoint).await?;
                Ok((lease, Some(moved)))
            }
            Err(error) => Err(error),
        }
    }

    /// Moves a persona to the browser major its engine's sidecar runs, and
    /// stores it. Returns the persona as it is stored now, or `None` when
    /// the sidecar had nothing to move (the document is of its browser
    /// already).
    ///
    /// # Errors
    ///
    /// [`PersonaError::Upgrade`] when the moved document is another
    /// persona than the stored one, of an older major or of another one
    /// than `engines.<engine>.browser_major` names; a sidecar or store
    /// error as it came.
    pub async fn upgrade_browser(
        &self,
        persona: &Persona,
    ) -> Result<Option<Persona>, PersonaError> {
        let fingerprint = persona
            .fingerprint
            .as_object()
            .cloned()
            .ok_or(PersonaError::Fingerprint(persona.id))?;
        let answer = self
            .sidecars()
            .of(persona.engine)
            .upgrade_persona(&UpgradePersonaRequest { fingerprint })
            .await?;
        if !answer.upgraded {
            return Ok(None);
        }
        let configured_major = match persona.engine {
            Engine::Chromium => self.config.engines.chromium.browser_major,
            Engine::Firefox => self.config.engines.firefox.browser_major,
        };
        let browser_major =
            verify_move(persona, configured_major, &answer.summary).map_err(|reason| {
                PersonaError::Upgrade {
                    persona: persona.id,
                    reason,
                }
            })?;
        let upgrade = BrowserUpgrade {
            fingerprint: serde_json::Value::Object(answer.fingerprint),
            browser_major,
            user_agent: answer.summary.user_agent.clone(),
            wreq_profile: profile_for(persona.engine, browser_major, persona.claimed_os),
        };
        let Upgraded {
            persona: stored,
            moved,
        } = self
            .store
            .personas()
            .upgrade_browser(persona.id, persona.browser_major, &upgrade)
            .await?
            .ok_or(PersonaError::NotFound(persona.id))?;
        if stored.browser_major != browser_major {
            return Err(PersonaError::Upgrade {
                persona: persona.id,
                reason: format!(
                    "it is stored as {} {} meanwhile, and the engine runs {browser_major}",
                    stored.engine, stored.browser_major
                ),
            });
        }
        if moved {
            self.directory().record(&stored);
            tracing::info!(
                persona = %stored.id,
                engine = %stored.engine,
                from_major = persona.browser_major,
                to_major = browser_major,
                "a persona moved to the browser its engine runs"
            );
            // The event keeps what the persona was before the move: the
            // way back to the previous image within the backup window
            // (docs/runbooks/upgrading-an-engine.md).
            self.event(
                stored.id,
                "persona_browser_upgraded",
                AlertSeverity::Info,
                json!({
                    "engine": stored.engine.as_str(),
                    "from_major": persona.browser_major,
                    "to_major": browser_major,
                    "browser_version": answer.summary.browser_version,
                    "wreq_profile": stored.wreq_profile,
                    "previous": {
                        "fingerprint": persona.fingerprint,
                        "user_agent": persona.user_agent,
                        "wreq_profile": persona.wreq_profile,
                    },
                }),
            )
            .await;
        }
        Ok(Some(stored))
    }

    /// Deletes the copies of profiles that the sidecars took before a
    /// browser upgrade (`.upgrade/<persona id>` on the personas volume)
    /// once they are `personas.upgrade_backup_days` old, and copies that
    /// were never finished once they are as old. Returns how many were
    /// deleted.
    ///
    /// A copy's age is the time its note (`upgrade.json`) gives, or the
    /// time the directory was last changed when it has none that can be
    /// read. Where Kage does not see the volume there is nothing to do.
    ///
    /// # Errors
    ///
    /// Fails when the directory of copies cannot be read. A copy that
    /// cannot be removed is logged and tried again at the next sweep.
    pub async fn sweep_upgrade_copies(&self) -> Result<u32, PersonaError> {
        let root = self.config.personas.dir.join(UPGRADE_DIR);
        let now = self.now();
        let window = chrono::Duration::from_std(self.config.personas.upgrade_backup())
            .unwrap_or(chrono::Duration::MAX);
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(PersonaError::Profile {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        let mut deleted = 0;
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(source) => {
                    return Err(PersonaError::Profile {
                        path: root.display().to_string(),
                        source,
                    });
                }
            };
            let path = entry.path();
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(taken) = taken_at(&path).await else {
                continue;
            };
            if now.signed_duration_since(taken) < window {
                continue;
            }
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    deleted += 1;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    tracing::info!(copy = %name, %taken, "a profile's copy from before an upgrade was deleted");
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "cannot delete a profile's copy from before an upgrade"
                    );
                }
            }
        }
        Ok(deleted)
    }
}

/// When a copy was taken: the note's time for a finished copy, the
/// directory's last change otherwise.
async fn taken_at(path: &Path) -> Option<DateTime<Utc>> {
    let finished = !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(PARTIAL));
    if finished
        && let Ok(text) = tokio::fs::read_to_string(path.join(NOTE_FILE)).await
        && let Some(copied_at) = copied_at(&text)
    {
        return Some(copied_at);
    }
    let changed: SystemTime = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(changed))
}

/// The time a copy's note gives, when it gives one.
fn copied_at(note: &str) -> Option<DateTime<Utc>> {
    let note: serde_json::Value = serde_json::from_str(note).ok()?;
    let at = note.get("copied_at")?.as_str()?;
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Checks the sidecar's move of a persona: the same engine, system, seed,
/// screen and processors, and a newer browser major that is the configured
/// one when one is configured. Returns the new major, or what is wrong.
fn verify_move(
    persona: &Persona,
    configured_major: Option<u32>,
    summary: &PersonaSummary,
) -> Result<u32, String> {
    if domain_engine(summary.engine) != persona.engine {
        return Err(format!(
            "the {} sidecar answered",
            domain_engine(summary.engine)
        ));
    }
    if domain_os(summary.claimed_os) != persona.claimed_os {
        return Err(format!(
            "the moved document claims {}, not {}",
            domain_os(summary.claimed_os),
            persona.claimed_os
        ));
    }
    if !summary.seed.eq_ignore_ascii_case(&persona.seed.to_string()) {
        return Err("the moved document is of another seed".to_owned());
    }
    if domain_screen(&summary.screen) != Some(persona.screen) {
        return Err("the moved document has another screen".to_owned());
    }
    if u64::from(persona.hardware_concurrency) != summary.hardware_concurrency {
        return Err(format!(
            "the moved document reports {} processors, not {}",
            summary.hardware_concurrency, persona.hardware_concurrency
        ));
    }
    let browser_major = u32::try_from(summary.browser_major)
        .map_err(|_| format!("browser major {}", summary.browser_major))?;
    if browser_major <= persona.browser_major {
        return Err(format!(
            "the engine runs major {browser_major}, the persona is of {}: a browser never goes back",
            persona.browser_major
        ));
    }
    if let Some(configured) = configured_major.filter(|configured| *configured != browser_major) {
        return Err(format!(
            "the engine runs major {browser_major}, the configuration names {configured}"
        ));
    }
    Ok(browser_major)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use kage_browser::contract::{self, EngineName, InjectMode, PersonaSummary};
    use kage_core::domain::{ClaimedOs, Engine};

    use super::{copied_at, verify_move};
    use crate::sidecar::contract_screen;
    use crate::test_support::persona;

    fn summary_of(persona: &kage_core::domain::Persona, major: u64) -> PersonaSummary {
        PersonaSummary {
            browser_major: major,
            browser_version: format!("{major}.0.1"),
            claimed_os: contract::ClaimedOs::Windows,
            engine: EngineName::Firefox,
            generator: "camoufox-js@0.12.1".to_owned(),
            hardware_concurrency: u64::from(persona.hardware_concurrency),
            inject: InjectMode::None,
            languages: persona.languages.clone(),
            platform: "Win32".to_owned(),
            screen: contract_screen(persona.screen),
            seed: persona.seed.to_string(),
            user_agent: format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{major}.0) Gecko/20100101 Firefox/{major}.0"
            ),
        }
    }

    #[test]
    fn a_move_changes_the_browser_and_nothing_of_the_device() {
        let stored = persona(Engine::Firefox, 152);
        let moved = summary_of(&stored, 156);
        assert_eq!(verify_move(&stored, None, &moved), Ok(156));
        assert_eq!(verify_move(&stored, Some(156), &moved), Ok(156));

        let refused = |change: fn(&mut PersonaSummary), configured: Option<u32>| {
            let mut summary = summary_of(&stored, 156);
            change(&mut summary);
            verify_move(&stored, configured, &summary).unwrap_err()
        };
        assert!(refused(|_| {}, Some(157)).contains("configuration names 157"));
        assert!(refused(|s| s.browser_major = 152, None).contains("never goes back"));
        assert!(refused(|s| s.browser_major = 151, None).contains("never goes back"));
        assert!(refused(|s| s.engine = EngineName::Chromium, None).contains("chromium sidecar"));
        assert!(
            refused(|s| s.claimed_os = contract::ClaimedOs::Macos, None).contains("claims macos")
        );
        assert!(refused(|s| s.seed = "00".repeat(32), None).contains("another seed"));
        assert!(refused(|s| s.screen.width = 1280, None).contains("another screen"));
        assert!(refused(|s| s.hardware_concurrency = 64, None).contains("64 processors"));
        assert!(refused(|s| s.browser_major = u64::MAX, None).contains("browser major"));

        // The seed is compared as the sidecar writes it, in any case.
        let mut upper = summary_of(&stored, 156);
        upper.seed = upper.seed.to_uppercase();
        assert_eq!(verify_move(&stored, None, &upper), Ok(156));
        assert_eq!(stored.claimed_os, ClaimedOs::Windows);
    }

    #[test]
    fn a_copys_age_is_the_time_its_note_gives() {
        assert_eq!(
            copied_at(r#"{"persona_id":"x","copied_at":"2026-10-18T09:30:00.000Z"}"#),
            Some(Utc.with_ymd_and_hms(2026, 10, 18, 9, 30, 0).unwrap())
        );
        for unreadable in [
            "",
            "{}",
            r#"{"copied_at":7}"#,
            r#"{"copied_at":"yesterday"}"#,
        ] {
            assert_eq!(copied_at(unreadable), None, "{unreadable:?}");
        }
    }
}
