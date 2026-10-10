//! The profile directory of a retired persona (spec §6.3): archived, then
//! deleted after `personas.retention_days`.
//!
//! A persona's browser profile lives on the personas volume, which the
//! sidecars write and Kage mounts as `personas.dir`. Kage never reads a
//! profile. `personas.archive_after` after a persona was retired (longer
//! than the sidecar's idle close, so that no browser has the profile open)
//! its directory is renamed to `.archive/<persona id>` on the same volume
//! and a `persona.json` is written into it: the ids, the engine and the
//! system, the proxy's address without its credentials, why and when. The
//! whole directory is deleted `personas.retention_days` after it was
//! archived. Nothing of it is ever given to another persona.
//!
//! The persona's row, its fingerprint, its per-domain state and its
//! self-test runs stay in the store. Where Kage does not see the volume
//! (`just dev` runs it outside the containers), nothing can be archived:
//! the row records that, and a warning says so once.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use kage_core::alert::AlertSeverity;
use kage_core::domain::Persona;
use kage_core::persona;
use serde_json::json;

use crate::PersonaError;
use crate::manager::PersonaManager;

/// The description written next to an archived profile.
const DESCRIPTION_FILE: &str = "persona.json";

/// `relative` below `root`, when it names something inside it: a relative
/// path of plain names. A stored path is never trusted to stay inside.
fn inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let plain = relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    (plain && relative.components().next().is_some()).then(|| root.join(relative))
}

fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> PersonaError + '_ {
    move |source| PersonaError::Profile {
        path: path.display().to_string(),
        source,
    }
}

async fn exists(path: &Path) -> Result<bool, PersonaError> {
    tokio::fs::try_exists(path).await.map_err(failed(path))
}

impl PersonaManager {
    /// Archives the profiles of personas retired `personas.archive_after`
    /// ago or longer, and deletes the archives that are older than the
    /// retention. Returns how many were archived and how many deleted.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A directory that cannot be moved or
    /// removed is logged and tried again at the next sweep.
    pub async fn archive_due(&self) -> Result<(u32, u32), PersonaError> {
        let now = self.now();
        let root = self.config.personas.dir.clone();
        let since = |span: std::time::Duration| {
            chrono::Duration::from_std(span).map_or(now, |span| now - span)
        };

        let mut archived = 0;
        let due = self
            .store
            .personas()
            .list_archive_due(since(self.config.personas.archive_after.get()))
            .await?;
        if !due.is_empty() && !exists(&root).await.unwrap_or(false) && self.report_volume_once() {
            tracing::warn!(
                dir = %root.display(),
                "the personas volume is not visible here: the profiles of retired personas \
                 are neither archived nor deleted"
            );
        }
        for persona in due {
            match self.archive(&root, &persona).await {
                Ok(archive) => {
                    self.store
                        .personas()
                        .modify(persona.id, |persona| {
                            persona.profile_archived_at = Some(now);
                            persona.profile_archive.clone_from(&archive);
                        })
                        .await?;
                    if archive.is_some() {
                        archived += 1;
                    }
                    self.event(
                        persona.id,
                        "persona_profile_archived",
                        AlertSeverity::Info,
                        json!({ "archive": archive, "from": persona.profile_dir }),
                    )
                    .await;
                }
                Err(error) => {
                    tracing::warn!(persona = %persona.id, %error, "cannot archive a profile");
                }
            }
        }

        let mut deleted = 0;
        let due = self
            .store
            .personas()
            .list_delete_due(since(self.config.personas.retention()))
            .await?;
        for persona in due {
            let removed = match persona
                .profile_archive
                .as_deref()
                .and_then(|archive| inside(&root, archive))
            {
                Some(path) => match tokio::fs::remove_dir_all(&path).await {
                    Ok(()) => true,
                    // Someone removed it already: the same result.
                    Err(error) if error.kind() == ErrorKind::NotFound => false,
                    Err(error) => {
                        tracing::warn!(
                            persona = %persona.id,
                            path = %path.display(),
                            %error,
                            "cannot delete an archived profile"
                        );
                        continue;
                    }
                },
                None => false,
            };
            self.store
                .personas()
                .modify(persona.id, |persona| {
                    persona.profile_deleted_at = Some(now);
                })
                .await?;
            if removed {
                deleted += 1;
            }
            self.event(
                persona.id,
                "persona_profile_deleted",
                AlertSeverity::Info,
                json!({ "archive": persona.profile_archive, "removed": removed }),
            )
            .await;
        }
        Ok((archived, deleted))
    }

    /// Moves one profile directory into the archive and describes it.
    /// Returns where it is, relative to the volume, or `None` when there
    /// was no directory within reach.
    async fn archive(
        &self,
        root: &Path,
        persona: &Persona,
    ) -> Result<Option<String>, PersonaError> {
        let archive = persona::archive_dir(persona.id);
        let (Some(from), Some(to)) = (inside(root, &persona.profile_dir), inside(root, &archive))
        else {
            return Ok(None);
        };
        if exists(&to).await? {
            // A sweep before this one moved it and could not say so.
            return Ok(Some(archive));
        }
        if !exists(&from).await? {
            return Ok(None);
        }
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(failed(parent))?;
        }
        tokio::fs::rename(&from, &to).await.map_err(failed(&from))?;

        // Without credentials: the address only.
        let proxy = self.pool.get(persona.proxy_id).await?;
        let description = json!({
            "persona_id": persona.id,
            "proxy_id": persona.proxy_id,
            "proxy": proxy.map(|proxy| format!("{}:{}", proxy.host, proxy.port)),
            "engine": persona.engine.as_str(),
            "claimed_os": persona.claimed_os.as_str(),
            "browser_major": persona.browser_major,
            "country": persona.country.map(|country| country.to_string()),
            "profile_dir": persona.profile_dir,
            "created_at": persona.created_at,
            "retired_at": persona.retired_at,
            "retire_reason": persona.retire_reason,
            "archived_at": self.now(),
        });
        let file = to.join(DESCRIPTION_FILE);
        tokio::fs::write(&file, format!("{description:#}\n"))
            .await
            .map_err(failed(&file))?;
        Ok(Some(archive))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::inside;

    #[test]
    fn a_stored_path_never_leaves_the_volume() {
        let root = Path::new("/var/lib/kage/personas");
        assert_eq!(
            inside(root, "firefox/0199-abc").unwrap(),
            root.join("firefox").join("0199-abc")
        );
        assert_eq!(
            inside(root, ".archive/0199-abc").unwrap(),
            root.join(".archive").join("0199-abc")
        );
        for refused in [
            "",
            ".",
            "..",
            "../other",
            "firefox/../../etc",
            "/etc/passwd",
            "./firefox/x",
        ] {
            assert_eq!(inside(root, refused), None, "{refused:?}");
        }
    }
}
