//! `kage proxy`: list the pool, probe it, replace a proxy, and show or
//! clear the egress flag (spec §7).
//!
//! The commands work on the same database as a running server, and may run
//! next to it: every change to a proxy is made under its row lock, and the
//! replacement budget under one lock of its own. Each command prints its
//! result on stdout, as a table or with `--json` as one JSON document; logs
//! go to stderr. Commands that change something write an audit event.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use kage_core::alert::{AlertSink, MemoryAlertSink, TracingAlertSink};
use kage_core::config::Config;
use kage_core::domain::{AuditActor, AuditEvent, AuditOutcome, Proxy, ProxyState, ReplaceReason};
use kage_core::ids::{AuditEventId, ProxyId};
use kage_proxy::{
    AccountInfo, AutoReplace, BudgetWindow, CheckResult, HealthChecker, Pool, PoolError,
    PoolSummary, ReplacementOutcome,
};
use kage_store::Store;
use serde_json::{Value, json};

use crate::{alerts, migrate};

/// How often `replace` looks whether the provider is done.
const REPLACE_POLL: Duration = Duration::from_secs(2);

/// What `kage proxy list` was asked for.
// One switch per command-line flag.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub(crate) struct ListOptions {
    /// Only proxies in this state.
    pub(crate) state: Option<ProxyState>,
    /// Retired proxies too.
    pub(crate) all: bool,
    /// Sync with the provider first.
    pub(crate) sync: bool,
    /// One JSON document instead of a table.
    pub(crate) json: bool,
    /// Decrypt and show the proxy user names.
    pub(crate) reveal_username: bool,
}

/// What `kage proxy health` was asked for.
#[derive(Debug, Clone, Default)]
pub(crate) struct HealthOptions {
    /// Only these proxies; all live ones when empty.
    pub(crate) ids: Vec<ProxyId>,
    /// Do not sync with the provider first.
    pub(crate) no_sync: bool,
    /// One JSON document instead of a table.
    pub(crate) json: bool,
}

/// What `kage proxy replace` was asked for.
#[derive(Debug, Clone)]
pub(crate) struct ReplaceOptions {
    /// The proxy to replace.
    pub(crate) id: ProxyId,
    /// How long to wait for the provider; `None` returns at once.
    pub(crate) wait: Option<Duration>,
    /// One JSON document instead of a sentence.
    pub(crate) json: bool,
}

/// The database, the pool and the alerts one command raised.
pub(crate) struct Session {
    pub(crate) store: Store,
    pub(crate) pool: Arc<Pool>,
    pub(crate) alerts: Arc<MemoryAlertSink>,
}

impl Session {
    pub(crate) async fn open(config: &Config) -> anyhow::Result<Self> {
        let store = migrate::connect(config).await?;
        let status = store
            .migration_status()
            .await
            .context("cannot read the migration state")?;
        migrate::ensure_clean(&status)?;
        let alerts = Arc::new(MemoryAlertSink::new());
        let pool = Pool::from_config(config, store.clone(), alerts.clone())
            .context("cannot set up the proxy pool")?;
        Ok(Self {
            store,
            pool: Arc::new(pool),
            alerts,
        })
    }

    /// Writes the audit event of a command that changed something.
    pub(crate) async fn audit(
        &self,
        action: &str,
        target: Option<String>,
        outcome: AuditOutcome,
        details: Value,
    ) {
        let event = AuditEvent {
            id: AuditEventId::new(),
            at: Utc::now(),
            actor: AuditActor::Cli,
            action: action.to_owned(),
            target,
            outcome,
            details,
        };
        if let Err(error) = self.store.audit_events().insert(&event).await {
            tracing::error!(%error, action, "cannot write the audit event");
        }
    }

    /// Logs and stores the alerts the command raised, then closes the
    /// database.
    pub(crate) async fn close(self) {
        for alert in self.alerts.alerts() {
            TracingAlertSink.raise(alert.clone());
            alerts::record(&self.store, &alert).await;
        }
        self.store.close().await;
    }
}

fn time(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(
        || "-".to_owned(),
        |at| at.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// The provider's country, with the observed one when it differs.
fn country(proxy: &Proxy) -> String {
    let observed = proxy.observed_geo().map(|geo| geo.country);
    match (proxy.country, observed) {
        (Some(stated), Some(observed)) if stated != observed => {
            format!("{stated} (observed {observed})")
        }
        (Some(country), _) | (None, Some(country)) => country.to_string(),
        (None, None) => "-".to_owned(),
    }
}

/// A proxy as JSON. Credentials are never part of it; the user name is
/// only when it was asked for.
fn proxy_json(proxy: &Proxy, username: Option<&str>) -> Value {
    let mut value = json!({
        "id": proxy.id,
        "provider": proxy.provider,
        "provider_ref": proxy.provider_ref,
        "host": proxy.host,
        "port": proxy.port,
        "country": proxy.country,
        "observed_country": proxy.observed_geo().map(|geo| geo.country),
        "city": proxy.city,
        "asn": proxy.asn,
        "state": proxy.state,
        "consecutive_failures": proxy.consecutive_failures,
        "backoff_count": proxy.backoff_count,
        "backoff_until": proxy.backoff_until,
        "failing_since": proxy.failing_since,
        "replacement": proxy.replacement.as_ref().map(|replacement| json!({
            "requested_at": replacement.requested_at,
            "reason": replacement.reason,
            "ticket": replacement.ticket,
        })),
        "last_health": proxy.last_health,
        "created_at": proxy.created_at,
        "retired_at": proxy.retired_at,
    });
    if let (Some(username), Some(object)) = (username, value.as_object_mut()) {
        object.insert("username".to_owned(), json!(username));
    }
    value
}

/// Writes rows as a table with a header, columns padded to their widest
/// cell.
pub(crate) fn table(
    out: &mut impl Write,
    header: &[&str],
    rows: &[Vec<String>],
) -> std::io::Result<()> {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut line = |cells: Vec<&str>| -> std::io::Result<()> {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        writeln!(out, "{}", padded.join("  ").trim_end())
    };
    line(header.to_vec())?;
    for row in rows {
        line(row.iter().map(String::as_str).collect())?;
    }
    Ok(())
}

fn summary_line(summary: &PoolSummary) -> String {
    let states: Vec<String> = summary
        .by_state
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(state, count)| format!("{count} {state}"))
        .collect();
    // The budget is counted over the provider's subscription term when the
    // account reports one, else over the calendar month.
    let window = match summary.budget_window {
        BudgetWindow::Term => format!(
            "term (since {})",
            summary.budget_window_start.format("%Y-%m-%d")
        ),
        BudgetWindow::Month => "month".to_owned(),
    };
    format!(
        "{} live, {} usable ({}); replacements this {window}: {} of {}; egress: {}",
        summary.live,
        summary.usable,
        if states.is_empty() {
            "no proxies".to_owned()
        } else {
            states.join(", ")
        },
        summary.replacements_this_month,
        summary.replacements_per_month,
        if summary.egress_unsafe {
            "UNSAFE, a proxy leaked the host address (see `kage proxy egress`)"
        } else {
            "safe"
        },
    )
}

/// `kage proxy list`.
pub(crate) async fn list(
    config: &Config,
    options: &ListOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = list_in(&session, options, out).await;
    session.close().await;
    result
}

async fn list_in(
    session: &Session,
    options: &ListOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let pool = &session.pool;
    if options.sync {
        let report = pool
            .reconcile()
            .await
            .context("cannot sync with the proxy provider")?;
        match pool.sync_account().await {
            Ok(_) | Err(PoolError::NoProvider) => {}
            Err(error) => tracing::warn!(%error, "cannot read the provider account"),
        }
        session
            .audit(
                "proxy.sync",
                None,
                AuditOutcome::Success,
                json!({
                    "listed": report.listed, "added": report.added.len(),
                    "updated": report.updated.len(), "retired": report.retired.len(),
                }),
            )
            .await;
    }

    let proxies: Vec<Proxy> = if options.all {
        pool.proxies().await?
    } else {
        pool.live().await?
    }
    .into_iter()
    .filter(|proxy| options.state.is_none_or(|state| proxy.state == state))
    .collect();
    let summary = pool.summary().await?;

    let usernames: Vec<Option<String>> = if options.reveal_username {
        session
            .audit(
                "proxy.reveal_username",
                None,
                AuditOutcome::Success,
                json!({ "proxies": proxies.len() }),
            )
            .await;
        proxies
            .iter()
            .map(|proxy| {
                Some(pool.endpoint(proxy).map_or_else(
                    |_| "(does not open with this master key)".to_owned(),
                    |endpoint| endpoint.username.expose().to_owned(),
                ))
            })
            .collect()
    } else {
        vec![None; proxies.len()]
    };

    if options.json {
        let document = json!({
            "proxies": proxies
                .iter()
                .zip(&usernames)
                .map(|(proxy, username)| proxy_json(proxy, username.as_deref()))
                .collect::<Vec<_>>(),
            "summary": summary,
            "account": pool.account(),
        });
        serde_json::to_writer_pretty(&mut *out, &document).context("cannot write the list")?;
        writeln!(out).context("cannot write the list")?;
        return Ok(());
    }

    list_table(
        out,
        &proxies,
        &usernames,
        &summary,
        pool.account().as_ref(),
        &pool.rules().accepted_auto_replace,
    )
    .context("cannot write the list")
}

/// The list as a table, followed by the pool's summary and what the
/// provider account says.
fn list_table(
    out: &mut impl Write,
    proxies: &[Proxy],
    usernames: &[Option<String>],
    summary: &PoolSummary,
    account: Option<&AccountInfo>,
    accepted: &AutoReplace,
) -> std::io::Result<()> {
    let mut header = vec![
        "ID",
        "HOST:PORT",
        "COUNTRY",
        "STATE",
        "FAILS",
        "LATENCY",
        "EXIT IP",
        "CHECKED (UTC)",
    ];
    if usernames.iter().any(Option::is_some) {
        header.push("USERNAME");
    }
    let rows: Vec<Vec<String>> = proxies
        .iter()
        .zip(usernames)
        .map(|(proxy, username)| {
            let health = proxy.last_health.as_ref();
            let mut row = vec![
                proxy.id.to_string(),
                format!("{}:{}", proxy.host, proxy.port),
                country(proxy),
                proxy.state.to_string(),
                proxy.consecutive_failures.to_string(),
                health
                    .and_then(|health| health.latency_ms)
                    .map_or_else(|| "-".to_owned(), |latency| format!("{latency} ms")),
                health
                    .and_then(|health| health.exit_ip)
                    .map_or_else(|| "-".to_owned(), |ip| ip.to_string()),
                time(health.map(|health| health.checked_at)),
            ];
            if let Some(username) = username {
                row.push(username.clone());
            }
            row
        })
        .collect();
    table(out, &header, &rows)?;
    writeln!(out, "{}", summary_line(summary))?;
    let Some(account) = account else {
        return Ok(());
    };
    if let Some(available) = account.replacements_available {
        writeln!(out, "provider plan: {available} replacements left")?;
    }
    let on = account.auto_replace.unwrap_or_default();
    let unexpected = on.without(accepted).enabled();
    if !unexpected.is_empty() {
        writeln!(
            out,
            "provider replaces proxies by itself ({}); switch this off in its dashboard",
            unexpected.join(", ")
        )?;
    }
    let noted = on.both(accepted).enabled();
    if !noted.is_empty() {
        writeln!(
            out,
            "provider replaces proxies by itself ({}), as accepted in \
             webshare.accepted_auto_replace",
            noted.join(", ")
        )?;
    }
    Ok(())
}

/// What a probe found, in one word.
fn verdict(result: &CheckResult) -> &'static str {
    match (&result.report.exit, result.report.leak) {
        (Ok(_), Some(true)) => "LEAK",
        (Ok(_), _) => "ok",
        (Err(failure), _) => failure.as_str(),
    }
}

/// `kage proxy health`.
pub(crate) async fn health(
    config: &Config,
    options: &HealthOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = health_in(&session, config, options, out).await;
    session.close().await;
    result
}

async fn health_in(
    session: &Session,
    config: &Config,
    options: &HealthOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let pool = &session.pool;
    if !options.no_sync && pool.provider_kind().is_some() {
        pool.reconcile().await.context(
            "cannot sync with the proxy provider (use --no-sync to check what is stored)",
        )?;
    }
    for id in &options.ids {
        if pool.get(*id).await?.is_none() {
            bail!("there is no proxy {id}");
        }
    }
    let checker = Arc::new(
        HealthChecker::from_config(config, Arc::clone(pool))
            .context("cannot set up the health checks")?,
    );
    let only = (!options.ids.is_empty()).then_some(options.ids.as_slice());
    let results = checker.check_now(only).await?;
    let summary = pool.summary().await?;
    let host_known = checker.host_ip().is_some();
    session
        .audit(
            "proxy.health",
            None,
            AuditOutcome::Success,
            json!({ "checked": results.len() }),
        )
        .await;

    if options.json {
        let document = json!({
            "results": results
                .iter()
                .map(|result| {
                    let mut value = proxy_json(&result.proxy, None);
                    if let Some(object) = value.as_object_mut() {
                        object.insert("verdict".to_owned(), json!(verdict(result)));
                    }
                    value
                })
                .collect::<Vec<_>>(),
            "host_address_known": host_known,
            "summary": summary,
        });
        serde_json::to_writer_pretty(&mut *out, &document).context("cannot write the result")?;
        writeln!(out).context("cannot write the result")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|result| {
            let exit = result.report.exit.as_ref().ok();
            vec![
                result.proxy.id.to_string(),
                format!("{}:{}", result.proxy.host, result.proxy.port),
                verdict(result).to_owned(),
                exit.map_or_else(|| "-".to_owned(), |exit| format!("{} ms", exit.latency_ms)),
                // Of the latency: opening the tunnel, and the handshake.
                exit.and_then(|exit| exit.connect_ms)
                    .map_or_else(|| "-".to_owned(), |connect| format!("{connect} ms")),
                exit.and_then(|exit| exit.tls_ms)
                    .map_or_else(|| "-".to_owned(), |tls| format!("{tls} ms")),
                exit.map_or_else(|| "-".to_owned(), |exit| exit.ip.to_string()),
                country(&result.proxy),
                result.proxy.state.to_string(),
            ]
        })
        .collect();
    table(
        out,
        &[
            "ID",
            "HOST:PORT",
            "RESULT",
            "LATENCY",
            "TUNNEL",
            "TLS",
            "EXIT IP",
            "COUNTRY",
            "STATE",
        ],
        &rows,
    )
    .context("cannot write the result")?;
    writeln!(out, "{} checked; {}", results.len(), summary_line(&summary))
        .context("cannot write the result")?;
    if !host_known {
        writeln!(
            out,
            "the host's own address could not be read, so the leak check did not run"
        )
        .context("cannot write the result")?;
    }
    Ok(())
}

/// How a replacement request ended, for the audit trail and for the
/// operator.
fn describe(outcome: &ReplacementOutcome) -> (AuditOutcome, String) {
    match outcome {
        ReplacementOutcome::Requested { ticket } => (
            AuditOutcome::Success,
            format!(
                "replacement requested{}",
                ticket
                    .as_ref()
                    .map_or_else(String::new, |ticket| format!(" (ticket {ticket})"))
            ),
        ),
        ReplacementOutcome::BudgetExhausted { used, budget } => (
            AuditOutcome::Denied,
            format!(
                "refused: this month's replacement budget is used up ({used} of {budget}); \
                 raise proxies.replacements_per_month to allow it"
            ),
        ),
        ReplacementOutcome::QuotaExhausted => (
            AuditOutcome::Denied,
            "refused: the provider's plan has no replacements left".to_owned(),
        ),
        ReplacementOutcome::CountryUnavailable { country } => (
            AuditOutcome::Denied,
            format!("refused: the provider has no proxy to offer in {country}"),
        ),
        ReplacementOutcome::Unsupported => (
            AuditOutcome::Denied,
            "refused: this provider cannot replace proxies".to_owned(),
        ),
        ReplacementOutcome::NotEligible { state } => (
            AuditOutcome::Denied,
            format!("refused: the proxy is {state}"),
        ),
        ReplacementOutcome::Failed { error } => (
            AuditOutcome::Failure,
            format!("the provider refused: {error}"),
        ),
    }
}

/// `kage proxy replace`.
pub(crate) async fn replace(
    config: &Config,
    options: &ReplaceOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = replace_in(&session, options, out).await;
    session.close().await;
    result
}

async fn replace_in(
    session: &Session,
    options: &ReplaceOptions,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let pool = &session.pool;
    let id = options.id;
    // The plan's quota and its stock per country decide too; read them
    // fresh. Without them the provider still has the last word.
    match pool.sync_account().await {
        Ok(_) => {}
        Err(PoolError::NoProvider) => bail!(PoolError::NoProvider),
        Err(error) => tracing::warn!(%error, "cannot read the provider account"),
    }
    let outcome = match pool.request_replacement(id, ReplaceReason::Operator).await {
        Ok(outcome) => outcome,
        Err(PoolError::NotFound(_)) => bail!("there is no proxy {id}"),
        Err(error) => return Err(error).context("cannot request the replacement"),
    };

    let (audit, sentence) = describe(&outcome);
    session
        .audit(
            "proxy.replace",
            Some(id.to_string()),
            audit,
            json!({ "outcome": sentence }),
        )
        .await;

    let requested = matches!(outcome, ReplacementOutcome::Requested { .. });
    let mut finished = false;
    if requested && let Some(wait) = options.wait {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            pool.tick().await?;
            let state = pool.get(id).await?.map(|proxy| proxy.state);
            if state != Some(ProxyState::Replacing) {
                finished = true;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(REPLACE_POLL).await;
        }
    }
    let proxy = pool.get(id).await?;
    let state = proxy.as_ref().map(|proxy| proxy.state);
    let ending = match (requested, finished, state) {
        (true, true, Some(ProxyState::Retired)) => {
            "; the provider replaced the proxy and the new address is in the pool"
        }
        (true, true, _) => "; the provider did not replace the proxy, it is back in backoff",
        (true, false, _) => "; the provider is still working on it, the server follows it up",
        _ => "",
    };

    if options.json {
        let document = json!({
            "requested": requested,
            "finished": finished,
            "message": format!("{sentence}{ending}"),
            "proxy": proxy.as_ref().map(|proxy| proxy_json(proxy, None)),
            "summary": pool.summary().await?,
        });
        serde_json::to_writer_pretty(&mut *out, &document).context("cannot write the result")?;
        writeln!(out).context("cannot write the result")?;
    } else {
        writeln!(out, "{sentence}{ending}").context("cannot write the result")?;
    }
    if requested && (!finished || state == Some(ProxyState::Retired)) {
        Ok(())
    } else {
        bail!("the proxy was not replaced")
    }
}

/// `kage proxy egress`.
pub(crate) async fn egress(
    config: &Config,
    clear: bool,
    json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = Session::open(config).await?;
    let result = egress_in(&session, clear, json, out).await;
    session.close().await;
    result
}

async fn egress_in(
    session: &Session,
    clear: bool,
    as_json: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let pool = &session.pool;
    let leaking: Vec<Proxy> = pool
        .proxies()
        .await?
        .into_iter()
        .filter(Proxy::leak_detected)
        .collect();
    let cleared = if clear {
        let cleared = pool.clear_egress_unsafe().await?;
        session
            .audit(
                "proxy.egress_clear",
                None,
                AuditOutcome::Success,
                json!({ "cleared": cleared }),
            )
            .await;
        cleared
    } else {
        Vec::new()
    };
    let egress_unsafe = pool.egress_unsafe().await?;

    if as_json {
        let document = json!({
            "egress_unsafe": egress_unsafe,
            "leaking": leaking.iter().map(|proxy| proxy.id).collect::<Vec<_>>(),
            "cleared": cleared,
        });
        serde_json::to_writer_pretty(&mut *out, &document).context("cannot write the result")?;
        writeln!(out).context("cannot write the result")?;
        return Ok(());
    }
    if leaking.is_empty() {
        writeln!(out, "egress: safe, no leak is on record").context("cannot write the result")?;
        return Ok(());
    }
    for proxy in &leaking {
        writeln!(
            out,
            "leak on record: proxy {} at {}:{} ({}), last probed {}",
            proxy.id,
            proxy.host,
            proxy.port,
            proxy.state,
            time(proxy.last_health.as_ref().map(|health| health.checked_at)),
        )
        .context("cannot write the result")?;
    }
    if egress_unsafe {
        writeln!(
            out,
            "egress: UNSAFE. Nothing may be fetched until a probe through each of these proxies \
             shows a distinct exit address (`kage proxy health`), or the flag is cleared with \
             `kage proxy egress --clear` once the cause is fixed."
        )
        .context("cannot write the result")?;
    } else {
        writeln!(
            out,
            "egress: safe, {} leak record(s) cleared; the proxies are probed again before they \
             are used",
            cleared.len()
        )
        .context("cannot write the result")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use kage_core::domain::{
        GeoRecord, HealthFailure, Proxy, ProxyHealth, ProxyProviderKind, ProxyReplacement,
        ProxyScheme, ProxyState, ReplaceReason,
    };
    use kage_core::ids::ProxyId;
    use kage_core::secret::SealedSecret;
    use kage_proxy::machine::{ExitObservation, ProbeReport};
    use kage_proxy::{BudgetWindow, CheckResult, PoolSummary};

    use super::{country, proxy_json, summary_line, table, time, verdict};

    fn proxy() -> Proxy {
        let at = Utc.with_ymd_and_hms(2026, 10, 9, 10, 11, 12).unwrap();
        Proxy {
            id: ProxyId::new(),
            provider: ProxyProviderKind::Webshare,
            provider_ref: "d-10001".to_owned(),
            host: "198.51.100.10".to_owned(),
            port: 8001,
            username: SealedSecret::new(b"sealed-user".to_vec()),
            password: SealedSecret::new(b"sealed-pass".to_vec()),
            scheme: ProxyScheme::Http,
            country: "DE".parse().ok(),
            city: Some("Berlin".to_owned()),
            asn: Some("AS3320".to_owned()),
            state: ProxyState::Healthy,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: Some(ProxyHealth {
                latency_ms: Some(143),
                exit_ip: "203.0.113.20".parse().ok(),
                leak_detected: Some(false),
                ..ProxyHealth::new(at, true)
            }),
            created_at: at,
            retired_at: None,
        }
    }

    fn observed(proxy: &mut Proxy, country: &str) {
        let health = proxy.last_health.as_mut().unwrap();
        health.geo = Some(GeoRecord {
            country: country.parse().unwrap(),
            city: None,
            region: None,
            asn: None,
            org: None,
            timezone: None,
            loc: None,
            ip: "203.0.113.20".parse().unwrap(),
            checked_at: health.checked_at,
        });
    }

    #[test]
    fn a_proxy_as_json_never_carries_credentials() {
        let mut proxy = proxy();
        proxy.replacement = Some(ProxyReplacement {
            requested_at: proxy.created_at,
            reason: ReplaceReason::Operator,
            ticket: Some("98315".to_owned()),
        });

        let value = proxy_json(&proxy, None);

        let object = value.as_object().unwrap();
        assert!(!object.contains_key("username"));
        assert!(!object.contains_key("password"));
        let text = value.to_string();
        assert!(!text.contains("sealed"), "{text}");
        assert_eq!(value["host"], "198.51.100.10");
        assert_eq!(value["state"], "healthy");
        assert_eq!(value["country"], "DE");
        assert!(value["observed_country"].is_null());
        assert_eq!(value["last_health"]["latency_ms"], 143);
        assert_eq!(value["replacement"]["reason"], "operator");
        assert_eq!(value["replacement"]["ticket"], "98315");

        // The user name appears only when it was asked for.
        let revealed = proxy_json(&proxy, Some("wsuser"));
        assert_eq!(revealed["username"], "wsuser");
        assert!(!revealed.as_object().unwrap().contains_key("password"));
    }

    #[test]
    fn the_country_column_shows_a_mismatch() {
        let mut proxy = proxy();
        assert_eq!(country(&proxy), "DE");

        observed(&mut proxy, "DE");
        assert_eq!(country(&proxy), "DE");

        observed(&mut proxy, "AT");
        assert_eq!(country(&proxy), "DE (observed AT)");

        proxy.country = None;
        assert_eq!(country(&proxy), "AT");
        proxy.last_health = None;
        assert_eq!(country(&proxy), "-");
    }

    #[test]
    fn a_table_pads_every_column_to_its_widest_cell() {
        let mut out = Vec::new();

        table(
            &mut out,
            &["ID", "STATE", "LATENCY"],
            &[
                vec!["a".to_owned(), "healthy".to_owned(), "143 ms".to_owned()],
                vec!["bbbb".to_owned(), "new".to_owned(), "-".to_owned()],
            ],
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "ID    STATE    LATENCY\n\
             a     healthy  143 ms\n\
             bbbb  new      -\n"
        );
        // No rows: just the header.
        let mut empty = Vec::new();
        table(&mut empty, &["ID", "STATE"], &[]).unwrap();
        assert_eq!(String::from_utf8(empty).unwrap(), "ID  STATE\n");
    }

    #[test]
    fn the_summary_names_states_budget_and_egress() {
        let mut summary = PoolSummary {
            by_state: BTreeMap::from([
                (ProxyState::New, 0),
                (ProxyState::Healthy, 18),
                (ProxyState::Degraded, 1),
                (ProxyState::Backoff, 1),
                (ProxyState::Replacing, 0),
                (ProxyState::Retired, 2),
            ]),
            live: 20,
            usable: 19,
            egress_unsafe: false,
            replacements_this_month: 2,
            replacements_per_month: 5,
            budget_window_start: Utc.with_ymd_and_hms(2026, 10, 9, 12, 20, 0).unwrap(),
            budget_window: BudgetWindow::Term,
        };

        assert_eq!(
            summary_line(&summary),
            "20 live, 19 usable (18 healthy, 1 degraded, 1 backoff, 2 retired); \
             replacements this term (since 2026-10-09): 2 of 5; egress: safe"
        );

        summary.egress_unsafe = true;
        assert!(summary_line(&summary).contains("egress: UNSAFE"));

        let empty = PoolSummary {
            by_state: BTreeMap::new(),
            live: 0,
            usable: 0,
            egress_unsafe: false,
            replacements_this_month: 0,
            replacements_per_month: 5,
            budget_window_start: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
            budget_window: BudgetWindow::Month,
        };
        let line = summary_line(&empty);
        assert!(line.starts_with("0 live, 0 usable (no proxies)"), "{line}");
        assert!(line.contains("replacements this month: 0 of 5"), "{line}");
    }

    #[test]
    fn a_probe_is_summed_up_in_one_word() {
        let at = Utc::now();
        let result = |exit, leak| CheckResult {
            proxy: proxy(),
            report: ProbeReport {
                at,
                exit,
                leak,
                geo: None,
            },
        };
        let seen = Ok(ExitObservation {
            ip: "203.0.113.20".parse().unwrap(),
            latency_ms: 143,
            connect_ms: None,
            tls_ms: None,
        });

        assert_eq!(verdict(&result(seen, Some(false))), "ok");
        assert_eq!(verdict(&result(seen, None)), "ok");
        assert_eq!(verdict(&result(seen, Some(true))), "LEAK");
        assert_eq!(
            verdict(&result(Err(HealthFailure::Unreachable), None)),
            "unreachable"
        );
        assert_eq!(
            verdict(&result(Err(HealthFailure::ProxyAuth), None)),
            "proxy_auth"
        );
    }

    #[test]
    fn times_are_printed_in_utc_or_as_a_dash() {
        assert_eq!(time(None), "-");
        assert_eq!(
            time(Some(Utc.with_ymd_and_hms(2026, 10, 9, 10, 11, 12).unwrap())),
            "2026-10-09 10:11:12"
        );
    }
}
