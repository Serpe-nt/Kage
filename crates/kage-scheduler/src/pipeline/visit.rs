//! What a result keeps of the visit that got its page, besides the page
//! (spec §16.5, follow-up F36): the pacing that applied to the request,
//! what the persona did on the page, the browser's log when the request
//! asked for it, and the events a capture fired in the page.
//!
//! It is built once, when a fetch ends, and stored with the result, so
//! that the answer of the fetch, `GET /v1/results/{id}` and an answer from
//! the response cache say the same.

use std::time::Duration;

use kage_browser::contract::{PageResult, ScrollEnd};
use kage_core::domain::{BehaviorDone, ConsoleEntry, PacingApplied, Visit};
use kage_core::outcome::Outcome;

use super::PacingReport;
use crate::executor::{AttemptOutput, BehaviorApplied};

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The pacing that applied to a request, and how long it waited, as a
/// result keeps it.
pub(super) fn pacing(report: &PacingReport) -> PacingApplied {
    PacingApplied {
        applied: report.applied.clone(),
        request_ignored: report.request_ignored,
        waited_ms: millis(report.waited),
    }
}

/// Why the scrolling of a page ended, as a result says it. Kage gives
/// every page a patience, so the time to reach the page (`budget`) never
/// ends it.
const fn scroll_end(end: ScrollEnd) -> Option<&'static str> {
    match end {
        ScrollEnd::Depth => Some("depth"),
        ScrollEnd::Patience => Some("patience"),
        ScrollEnd::Page => Some("page"),
        ScrollEnd::Budget => None,
    }
}

/// What the persona was asked to do on a page and what the browser says
/// it did.
fn behavior(applied: &BehaviorApplied, page: Option<&PageResult>) -> BehaviorDone {
    let done = page.and_then(|page| page.behavior.as_ref());
    BehaviorDone {
        from_request: applied.from_request,
        request_ignored: applied.request_ignored,
        // No scrolling is a depth of 0, as a request asks for it.
        scroll_depth: Some(applied.scroll_depth.unwrap_or(0.0)),
        scroll_reached: done.and_then(|done| done.scroll_reached),
        scroll_ended_by: done
            .and_then(|done| done.scroll_ended_by)
            .and_then(scroll_end)
            .map(str::to_owned),
        // What the page was given; a browser that reports it says the same.
        patience_ms: applied.patience.map(millis),
        // What the sidecar says it dwelt; else what it was asked for.
        dwell_ms: done
            .map(|done| done.dwell_ms)
            .or_else(|| applied.dwell.map(millis)),
        referer_strategy: applied.referer_strategy,
        moves: done.map(|done| done.moves),
        notches: done.map(|done| done.notches),
    }
}

/// What is kept of the last attempt of a fetch that ended with `outcome`.
/// `console_asked` says whether the request asked for the browser's log.
pub(super) fn of(
    pacing: PacingApplied,
    outcome: Outcome,
    last: Option<&AttemptOutput>,
    console_asked: bool,
) -> Visit {
    let page = last.and_then(|last| last.page.as_deref());
    // Nothing read from a hop the guard refused is kept (spec §19).
    let readable = outcome != Outcome::SsrfDenied;
    Visit {
        pacing: Some(pacing),
        behavior: last
            .and_then(|last| last.behavior.as_ref())
            .map(|applied| behavior(applied, page)),
        console: page.filter(|_| console_asked && readable).map(|page| {
            Visit::console_of(page.console.iter().map(|entry| {
                ConsoleEntry::bounded(
                    entry.level.as_str(),
                    &entry.source,
                    &entry.text,
                    entry.url.as_deref(),
                )
            }))
        }),
        side_effects: page.map_or_else(Vec::new, |page| {
            page.side_effects
                .iter()
                .map(|effect| effect.as_str().to_owned())
                .collect()
        }),
    }
}

/// What an answer from the response cache keeps: the page is the one a
/// visit at `cached_at` got, so what the persona did there, the browser's
/// log and what a capture fired are that visit's; the pacing is the
/// request's own, which waited for nobody.
pub(super) fn of_cached(stored: Visit, pacing: PacingApplied) -> Visit {
    Visit {
        pacing: Some(pacing),
        ..stored
    }
}
