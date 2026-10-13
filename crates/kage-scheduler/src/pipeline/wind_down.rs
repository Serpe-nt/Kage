//! What happens to the pages that are being read when the server stops
//! (follow-up F39).
//!
//! An attempt that is under way is a request the site has seen or is about
//! to see. Cut it without a word and the record says nothing of it, the
//! request is taken up again after the restart, and the site sees the page
//! twice. So the pipeline counts the attempts that are under way, and a
//! server that stops waits for them ([`Pipeline::wind_down`]): each is
//! finished and written down as any attempt is. What is still under way
//! when the wait is over is cut, and written down as cut: an attempt that
//! ended `provider_error` with the reason [`INTERRUPTED`], which says
//! nothing about the site or the persona and counts toward the request's
//! retries when the request is taken up again.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kage_core::outcome::Outcome;
use kage_core::schedule::politeness::Courtesy;
use tokio::sync::{Notify, watch};

use super::Pipeline;
use crate::executor::AttemptOutput;

/// The reason of an attempt that was cut because the server stopped while
/// its page was being read.
pub const INTERRUPTED: &str = "interrupted";

/// How long the attempts that were cut are given to be written down.
const CUT_GRACE: Duration = Duration::from_secs(10);

/// What a wind-down came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WoundDown {
    /// Attempts that ended by themselves while the server waited.
    pub finished: usize,
    /// Attempts that were still under way when the wait was over, and
    /// were cut.
    pub cut: usize,
}

/// The attempts that are under way in a pipeline, and the word that cuts
/// them.
#[derive(Debug)]
pub(super) struct UnderWay {
    count: AtomicUsize,
    ended: AtomicUsize,
    cuts: AtomicUsize,
    changed: Notify,
    cut: watch::Sender<bool>,
}

impl Default for UnderWay {
    fn default() -> Self {
        Self {
            count: AtomicUsize::new(0),
            ended: AtomicUsize::new(0),
            cuts: AtomicUsize::new(0),
            changed: Notify::new(),
            cut: watch::Sender::new(false),
        }
    }
}

/// One attempt, counted as under way from when it begins until it is
/// written down.
#[derive(Debug)]
pub(super) struct Counted<'a>(&'a UnderWay);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
        self.0.ended.fetch_add(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}

impl UnderWay {
    /// Counts an attempt as under way until what is returned is dropped.
    pub(super) fn begin(&self) -> Counted<'_> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Counted(self)
    }

    /// How many attempts are under way.
    pub(super) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// What an attempt came to: its own end, or, when the server stopped
    /// waiting for it first, that it was cut.
    pub(super) async fn until_cut(
        &self,
        attempt: impl Future<Output = AttemptOutput>,
    ) -> AttemptOutput {
        let mut cut = self.cut.subscribe();
        tokio::select! {
            output = attempt => output,
            // The sender lives as long as the pipeline: a signal that
            // cannot be read any more never cuts anything.
            Ok(_) = cut.wait_for(|cut| *cut) => {
                self.cuts.fetch_add(1, Ordering::SeqCst);
                AttemptOutput::failed(Outcome::ProviderError, INTERRUPTED, Courtesy::NotTheSite)
            }
        }
    }

    /// Resolves when no attempt is under way.
    async fn settled(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Registered before the count is read: an attempt that ends
            // between the two is not missed.
            changed.as_mut().enable();
            if self.count() == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl Pipeline {
    /// How many attempts are under way: pages that are being asked for or
    /// read right now, and not yet written down.
    pub fn attempts_under_way(&self) -> usize {
        self.under_way.count()
    }

    /// Lets the pages that are being read finish, for at most `bound`,
    /// and cuts what is still under way then; returns once every one of
    /// them is written down. For a server that stops: stop the scheduler
    /// first, so that nothing new begins.
    pub async fn wind_down(&self, bound: Duration) -> WoundDown {
        let under_way = &self.under_way;
        let ended = under_way.ended.load(Ordering::SeqCst);
        let cuts = under_way.cuts.load(Ordering::SeqCst);
        let waiting = under_way.count();
        if waiting > 0 {
            tracing::info!(
                attempts = waiting,
                bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
                "the server stops: the pages that are being read are finished first"
            );
        }
        if tokio::time::timeout(bound, under_way.settled())
            .await
            .is_err()
        {
            under_way.cut.send_replace(true);
            if tokio::time::timeout(CUT_GRACE, under_way.settled())
                .await
                .is_err()
            {
                tracing::warn!(
                    attempts = under_way.count(),
                    "attempts that were cut could not be written down in time"
                );
            }
            // Whatever begins after this is no page of this wind-down.
            under_way.cut.send_replace(false);
        }
        let cut = under_way.cuts.load(Ordering::SeqCst).saturating_sub(cuts);
        let finished = under_way
            .ended
            .load(Ordering::SeqCst)
            .saturating_sub(ended)
            .saturating_sub(cut);
        if cut > 0 {
            tracing::warn!(
                cut,
                finished,
                "pages were still being read when the wait was over: they were cut, and are on \
                 the record as interrupted"
            );
        }
        WoundDown { finished, cut }
    }
}
