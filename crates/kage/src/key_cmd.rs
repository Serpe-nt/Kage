//! `kage key create|list|revoke|rotate`: API keys at the command line
//! (spec §16.2).
//!
//! The commands work on the store alone, so they run next to a server and
//! before one was ever started: the first admin key of a deployment is
//! made here. A key's token is printed once, by `create` and by `rotate`;
//! only the hash of its secret is stored. Every change writes an audit
//! event.

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use kage_api::keys::{self, DEFAULT_LIMITS, NewKey};
use kage_core::config::Config;
use kage_core::domain::{ApiKey, AuditActor, AuditEvent, AuditOutcome, KeyLimits, Scope};
use kage_core::ids::{ApiKeyId, AuditEventId};
use kage_store::Store;
use serde_json::{Value, json};

use crate::migrate;
use crate::proxy_cmd::table;

/// What `kage key create` was asked to make.
#[derive(Debug, Clone)]
pub(crate) struct CreateOptions {
    /// A name for humans.
    pub(crate) name: String,
    /// What the key may do.
    pub(crate) scopes: Vec<Scope>,
    /// Requests per minute.
    pub(crate) requests_per_minute: Option<u32>,
    /// Requests in flight at once.
    pub(crate) max_concurrent: Option<u32>,
    /// Pages per day.
    pub(crate) daily_page_budget: Option<u32>,
    /// One JSON document instead of sentences.
    pub(crate) json: bool,
}

/// Opens the store, as every key command does first.
async fn open(config: &Config) -> anyhow::Result<Store> {
    let store = migrate::connect(config).await?;
    let status = store
        .migration_status()
        .await
        .context("cannot read the migration state")?;
    migrate::ensure_clean(&status)?;
    Ok(store)
}

async fn audit(store: &Store, action: &str, key: ApiKeyId, outcome: AuditOutcome, details: Value) {
    let event = AuditEvent {
        id: AuditEventId::new(),
        at: Utc::now(),
        actor: AuditActor::Cli,
        action: action.to_owned(),
        target: Some(key.to_string()),
        outcome,
        details,
    };
    if let Err(error) = store.audit_events().insert(&event).await {
        tracing::error!(%error, action, "cannot write the audit event");
    }
}

fn document(key: &ApiKey) -> Value {
    json!({
        "id": key.id,
        "name": key.name,
        "scopes": key.scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>(),
        "requests_per_minute": key.limits.requests_per_minute,
        "max_concurrent": key.limits.max_concurrent,
        "daily_page_budget": key.limits.daily_page_budget,
        "active": key.is_active(),
        "disabled_at": key.disabled_at,
        "created_at": key.created_at,
        "last_used_at": key.last_used_at,
    })
}

fn scopes_of(key: &ApiKey) -> String {
    key.scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Prints a key with the token that is shown this once.
fn write_token(
    out: &mut impl Write,
    key: &ApiKey,
    token: &keys::Token,
    as_json: bool,
    what: &str,
) -> anyhow::Result<()> {
    if as_json {
        let mut document = document(key);
        document["token"] = json!(token.reveal());
        serde_json::to_writer_pretty(&mut *out, &document)?;
        writeln!(out)?;
    } else {
        writeln!(out, "{what} key {} ({})", key.id, key.name)?;
        writeln!(out, "scopes: {}", scopes_of(key))?;
        writeln!(
            out,
            "limits: {} requests a minute, {} in flight, {} pages a day",
            key.limits.requests_per_minute,
            key.limits.max_concurrent,
            key.limits
                .daily_page_budget
                .map_or_else(|| "unlimited".to_owned(), |budget| budget.to_string())
        )?;
        writeln!(out)?;
        writeln!(out, "token (shown once, Kage keeps only a hash):")?;
        writeln!(out, "{}", token.reveal())?;
        writeln!(out)?;
        writeln!(out, "send it as: Authorization: Bearer <token>")?;
    }
    Ok(())
}

/// `kage key create`.
pub(crate) async fn create(
    config: &Config,
    options: &CreateOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let new = NewKey {
        name: options.name.clone(),
        scopes: options.scopes.iter().copied().collect::<BTreeSet<_>>(),
        limits: KeyLimits {
            requests_per_minute: options
                .requests_per_minute
                .unwrap_or(DEFAULT_LIMITS.requests_per_minute),
            max_concurrent: options
                .max_concurrent
                .unwrap_or(DEFAULT_LIMITS.max_concurrent),
            daily_page_budget: options.daily_page_budget,
        },
    };
    let (key, token) = keys::mint(&new, Utc::now()).context("the key cannot be made")?;
    let store = open(config).await?;
    let stored = store.api_keys().insert(&key).await;
    if stored.is_ok() {
        audit(
            &store,
            "key.create",
            key.id,
            AuditOutcome::Success,
            json!({ "name": key.name, "scopes": scopes_of(&key) }),
        )
        .await;
    }
    store.close().await;
    stored.context("cannot store the key")?;
    write_token(out, &key, &token, options.json, "created")
}

fn time(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(
        || "-".to_owned(),
        |at| at.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// `kage key list`.
pub(crate) async fn list(
    config: &Config,
    all: bool,
    as_json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let store = open(config).await?;
    let keys = store.api_keys().list().await;
    store.close().await;
    let keys: Vec<ApiKey> = keys
        .context("cannot read the keys")?
        .into_iter()
        .filter(|key| all || key.is_active())
        .collect();
    if as_json {
        let documents: Vec<Value> = keys.iter().map(document).collect();
        serde_json::to_writer_pretty(&mut *out, &json!({ "keys": documents }))?;
        writeln!(out)?;
        return Ok(());
    }
    if keys.is_empty() {
        writeln!(
            out,
            "no API keys; make one with `kage key create --name <name> --scope fetch`"
        )?;
        return Ok(());
    }
    let rows: Vec<Vec<String>> = keys
        .iter()
        .map(|key| {
            vec![
                key.id.to_string(),
                key.name.clone(),
                scopes_of(key),
                key.limits.requests_per_minute.to_string(),
                key.limits.max_concurrent.to_string(),
                key.limits
                    .daily_page_budget
                    .map_or_else(|| "-".to_owned(), |budget| budget.to_string()),
                if key.is_active() {
                    "active"
                } else {
                    "disabled"
                }
                .to_owned(),
                time(key.last_used_at),
            ]
        })
        .collect();
    table(
        out,
        &[
            "ID",
            "NAME",
            "SCOPES",
            "RPM",
            "IN FLIGHT",
            "PAGES/DAY",
            "STATE",
            "LAST USED",
        ],
        &rows,
    )?;
    Ok(())
}

/// `kage key revoke`: the key stops authenticating at once; its row and
/// its audit trail stay.
pub(crate) async fn revoke(
    config: &Config,
    id: ApiKeyId,
    as_json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let store = open(config).await?;
    let result = async {
        let Some(mut key) = store
            .api_keys()
            .get(id)
            .await
            .context("cannot read the key")?
        else {
            bail!("there is no API key {id}");
        };
        let was_active = key.is_active();
        if was_active {
            let now = Utc::now();
            store
                .api_keys()
                .set_disabled(id, Some(now))
                .await
                .context("cannot disable the key")?;
            key.disabled_at = Some(now);
            audit(
                &store,
                "key.revoke",
                id,
                AuditOutcome::Success,
                json!({ "name": key.name }),
            )
            .await;
        }
        Ok((key, was_active))
    }
    .await;
    store.close().await;
    let (key, was_active) = result?;
    if as_json {
        serde_json::to_writer_pretty(&mut *out, &document(&key))?;
        writeln!(out)?;
    } else if was_active {
        writeln!(out, "key {id} ({}) is disabled", key.name)?;
    } else {
        writeln!(out, "key {id} ({}) was disabled already", key.name)?;
    }
    Ok(())
}

/// `kage key rotate`: a new secret for the key; the old token stops
/// working at once.
pub(crate) async fn rotate(
    config: &Config,
    id: ApiKeyId,
    as_json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let store = open(config).await?;
    let result = async {
        let Some(key) = store
            .api_keys()
            .get(id)
            .await
            .context("cannot read the key")?
        else {
            bail!("there is no API key {id}");
        };
        if !key.is_active() {
            bail!("key {id} is disabled; a disabled key is not given a new secret");
        }
        let (hash, token) = keys::rotate(id).context("a new secret cannot be made")?;
        store
            .api_keys()
            .set_secret_hash(id, &hash)
            .await
            .context("cannot store the new secret")?;
        audit(
            &store,
            "key.rotate",
            id,
            AuditOutcome::Success,
            json!({ "name": key.name }),
        )
        .await;
        Ok((key, token))
    }
    .await;
    store.close().await;
    let (key, token) = result?;
    write_token(out, &key, &token, as_json, "rotated")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;
    use kage_api::keys::{DEFAULT_LIMITS, NewKey, mint};
    use kage_core::domain::Scope;

    use super::{document, scopes_of, write_token};

    #[test]
    fn a_new_key_is_printed_with_its_token_once() {
        let new = NewKey {
            name: "project-a".to_owned(),
            scopes: BTreeSet::from([Scope::Fetch, Scope::Sessions]),
            limits: DEFAULT_LIMITS,
        };
        let (key, token) = mint(&new, Utc::now()).unwrap();

        let mut text = Vec::new();
        write_token(&mut text, &key, &token, false, "created").unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.starts_with(&format!("created key {} (project-a)\n", key.id)));
        assert!(text.contains("scopes: fetch,sessions\n"));
        assert!(text.contains("60 requests a minute, 4 in flight, unlimited pages a day"));
        assert!(text.contains(token.reveal()));

        let mut json = Vec::new();
        write_token(&mut json, &key, &token, true, "created").unwrap();
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(json["token"], token.reveal());
        assert_eq!(json["scopes"], serde_json::json!(["fetch", "sessions"]));

        // What `list` prints holds no token and no hash.
        let listed = document(&key).to_string();
        assert!(!listed.contains(&token.reveal()[38..]));
        assert!(!listed.contains("secret"));
        assert_eq!(scopes_of(&key), "fetch,sessions");
    }
}
