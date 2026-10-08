//! `kage migrate`: applies the schema migrations, or with `--check` only
//! reports whether the database is up to date.

use anyhow::{Context, bail};
use kage_core::config::Config;
use kage_store::{MigrationStatus, Store};

/// Applies every pending migration. Running it again changes nothing.
pub(crate) async fn run(config: &Config) -> anyhow::Result<()> {
    let store = connect(config).await?;
    let before = store
        .migration_status()
        .await
        .context("cannot read the migration state")?;
    store.migrate().await.context("migration failed")?;
    let after = store
        .migration_status()
        .await
        .context("cannot read the migration state")?;
    tracing::info!(
        applied_now = ?before.pending(),
        total = after.migrations.len(),
        "migrations applied"
    );
    ensure_clean(&after)
}

/// Fails unless the database has exactly the migrations this binary carries.
pub(crate) async fn check(config: &Config) -> anyhow::Result<()> {
    let store = connect(config).await?;
    let status = store
        .migration_status()
        .await
        .context("cannot read the migration state")?;
    ensure_clean(&status)?;
    tracing::info!(
        total = status.migrations.len(),
        "the database schema is up to date"
    );
    Ok(())
}

pub(crate) async fn connect(config: &Config) -> anyhow::Result<Store> {
    Store::connect(&config.database)
        .await
        .context("cannot connect to the database")
}

/// Explains what is wrong with a schema that is not the expected one.
pub(crate) fn ensure_clean(status: &MigrationStatus) -> anyhow::Result<()> {
    if status.is_clean() {
        return Ok(());
    }
    let mut problems = Vec::new();
    let pending = status.pending();
    if !pending.is_empty() {
        problems.push(format!(
            "pending migrations {pending:?}; run `kage migrate`"
        ));
    }
    let modified = status.modified();
    if !modified.is_empty() {
        problems.push(format!(
            "migrations {modified:?} were changed after they were applied"
        ));
    }
    if !status.unknown_versions.is_empty() {
        problems.push(format!(
            "the database has migrations {:?} that this version of kage does not know; \
             it was migrated by a newer version",
            status.unknown_versions
        ));
    }
    bail!(
        "the database schema is not up to date: {}",
        problems.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use kage_store::{MigrationState, MigrationStatus};

    use super::ensure_clean;

    fn status(states: &[(i64, bool, Option<bool>)], unknown: &[i64]) -> MigrationStatus {
        MigrationStatus {
            migrations: states
                .iter()
                .map(|&(version, applied, checksum_matches)| MigrationState {
                    version,
                    description: format!("migration {version}"),
                    applied,
                    checksum_matches,
                })
                .collect(),
            unknown_versions: unknown.to_vec(),
        }
    }

    #[test]
    fn a_clean_schema_passes() {
        assert!(
            ensure_clean(&status(
                &[(1, true, Some(true)), (2, true, Some(true))],
                &[]
            ))
            .is_ok()
        );
    }

    #[test]
    fn each_problem_is_explained() {
        let pending = ensure_clean(&status(&[(1, true, Some(true)), (2, false, None)], &[]))
            .unwrap_err()
            .to_string();
        assert!(pending.contains("pending migrations [2]"), "{pending}");
        assert!(pending.contains("kage migrate"), "{pending}");

        let modified = ensure_clean(&status(&[(1, true, Some(false))], &[]))
            .unwrap_err()
            .to_string();
        assert!(modified.contains("[1] were changed"), "{modified}");

        let ahead = ensure_clean(&status(&[(1, true, Some(true))], &[9]))
            .unwrap_err()
            .to_string();
        assert!(
            ahead.contains("[9]") && ahead.contains("newer version"),
            "{ahead}"
        );

        let all = ensure_clean(&status(&[(1, true, Some(false)), (2, false, None)], &[9]))
            .unwrap_err()
            .to_string();
        for phrase in [
            "pending migrations [2]",
            "[1] were changed",
            "newer version",
        ] {
            assert!(all.contains(phrase), "{all}");
        }
    }
}
