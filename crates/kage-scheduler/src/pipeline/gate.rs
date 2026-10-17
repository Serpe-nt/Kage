//! `robots.txt` on the way of a fetch (spec §14): what the site's file
//! says of the address before anything is asked of the site, and the
//! scout that asks for the file. The rules are [`crate::robots`]'s.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_core::domain::{RenderMode, RobotsVerdict};
use kage_core::ids::{PersonaId, RequestId};
use kage_core::policy::RobotsMode;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use url::Url;

use super::{Course, Deferred, FetchOrder, FetchReport, Pipeline, Ran, Stop, stopped};
use crate::SchedulerError;
use crate::archive::bytes_of;
use crate::executor::Arrival;
use crate::metrics;
use crate::robots::{self, Answer, Known, RobotsWish, Standing};
use crate::scheduler::Refused;

/// The `reason` of a fetch whose site enforces a `robots.txt` that could
/// not be had.
pub const ROBOTS_UNAVAILABLE: &str = "robots_unavailable";

/// The `reason` of a fetch whose site enforces its `robots.txt` and has
/// nobody who could ask for it: every persona that could has read the
/// site.
pub const ROBOTS_NO_SCOUT: &str = "robots_no_scout";

/// How long a fetch whose scout could not ask (its proxy, the sidecar)
/// is put off: nothing was asked of the site, and nothing is kept.
const NOT_ASKED_WAIT: Duration = Duration::from_mins(1);

/// How long a scout that follows up a reported verdict waits for its turn
/// before it leaves it to the next fetch of the origin.
const FOLLOW_UP_PATIENCE: Duration = Duration::from_mins(10);

/// What `robots.txt` means for a fetch that is about to be attempted.
#[derive(Debug)]
pub(super) enum Gate {
    /// Nothing stands in its way.
    Open,
    /// It ends `robots_denied`: a rule of the file disallows the address
    /// (`None`), or the request's wish and the policy cannot both be had.
    Denied(Option<&'static str>),
    /// The site's file is enforced and what it says cannot be had now: a
    /// job waits, a caller that waits for its page is told
    /// `robots_denied`.
    Unavailable {
        /// The `reason`.
        reason: &'static str,
        /// When it is worth asking again.
        retry_at: Option<DateTime<Utc>>,
    },
    /// The scout was refused by the scheduler as the page itself would
    /// have been.
    Refused(Refused),
    /// The stop signal came while the scout waited.
    Stopped,
}

/// What a crawl is told about an address before it asks for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Peek {
    /// Ask for it: nothing known speaks against it. What its fetch finds
    /// out stands in its result.
    Go,
    /// The site's file is enforced and disallows it.
    Denied(RobotsVerdict),
}

/// What came of reading a site's `robots.txt` for somebody who waits for
/// it (a crawl that takes its sitemaps from it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsRead {
    /// The file was read, or there is none: the sitemaps it names, as
    /// written.
    Read {
        /// Whether the origin has a file.
        found: bool,
        /// Its `Sitemap:` lines.
        sitemaps: Vec<String>,
    },
    /// The site's policy does not have the file asked for.
    NotAsked,
    /// The file could not be had, and is asked for again after `until`.
    Unreachable {
        /// When.
        until: DateTime<Utc>,
    },
    /// Nobody can ask: every persona that could has read the site.
    NoScout,
    /// The scout cannot ask now.
    Later(Deferred),
    /// The stop signal came first.
    Stopped,
}

/// Who waits for a site's file and what its scout may be.
#[derive(Debug, Clone)]
pub struct ScoutNeed {
    /// Personas that read the site for the caller and so cannot scout it,
    /// besides those the store knows as its readers.
    pub readers: Vec<PersonaId>,
    /// How many personas the caller needs to read the site.
    pub readers_needed: usize,
    /// Whose work the scout's request is.
    pub priority: Priority,
    /// How long the scout waits for its turn before it would rather be
    /// told when to come back; `None` waits as long as it takes.
    pub patience: Option<Duration>,
}

/// A site's new scout, given by [`Pipeline::replace_scout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewScout {
    /// The scout the site had, when it had one that lives.
    pub previous: Option<PersonaId>,
    /// The persona that asks the site from now on.
    pub scout: PersonaId,
}

/// One request for an origin's `robots.txt`.
#[derive(Debug)]
pub(super) struct Call {
    origin: String,
    site: String,
    host: String,
    file: Url,
    need: ScoutNeed,
    give_up_at: Option<DateTime<Utc>>,
    overflowed: Option<Arc<AtomicBool>>,
}

/// What came of a [`Call`].
enum Asked {
    Known(Arc<Known>),
    NoScout,
    Refused(Refused),
    /// The attempt failed before the site had a say.
    NoAnswer,
    Stopped,
}

/// The scout's request under way. Named, and boxed: the scout's fetch is
/// a run of the pipeline inside a run of the pipeline.
type Asking<'a> = Pin<Box<dyn Future<Output = Result<Asked, SchedulerError>> + Send + 'a>>;

impl Pipeline {
    fn later(&self, wait: Duration) -> Option<DateTime<Utc>> {
        chrono::Duration::from_std(wait)
            .ok()
            .and_then(|wait| self.now().checked_add_signed(wait))
    }

    /// The moment after which a persona that read a site is its reader
    /// and may not scout it (`robots.scout_quarantine`): one that read it
    /// that long ago or longer may.
    fn lately(&self) -> DateTime<Utc> {
        chrono::Duration::from_std(self.robots.scout_quarantine)
            .ok()
            .and_then(|quarantine| self.now().checked_sub_signed(quarantine))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }

    /// The scout of a site, when it has one: the persona that reads none
    /// of its pages.
    pub(super) async fn site_scout(&self, site: &str) -> Result<Option<PersonaId>, SchedulerError> {
        if let Some(known) = self.desk.scout(site) {
            return Ok(known.0);
        }
        let scout = self.backend().scout(site).await?;
        self.desk.note_scout(site, scout);
        Ok(scout)
    }

    /// What is kept of an origin's file, however old.
    async fn robots_known(&self, origin: &str) -> Result<Option<Arc<Known>>, SchedulerError> {
        if let Some(known) = self.desk.known(origin) {
            return Ok(Some(known));
        }
        let Some(entry) = self.backend().robots(origin).await? else {
            self.desk.forget(origin);
            return Ok(None);
        };
        // The row as it was read before is not read again.
        let known = self
            .desk
            .held(origin)
            .filter(|held| {
                held.fetched_at == entry.fetched_at && held.expires_at == entry.expires_at
            })
            .unwrap_or_else(|| Arc::new(Known::of(&entry)));
        Ok(Some(self.desk.keep(origin, known, self.now())))
    }

    /// What is kept of an origin's file and still stands.
    async fn robots_fresh(&self, origin: &str) -> Result<Option<Arc<Known>>, SchedulerError> {
        let now = self.now();
        Ok(self
            .robots_known(origin)
            .await?
            .filter(|known| known.fresh(now)))
    }

    /// The site's scout for a file no reader asks for (its `robots.txt`,
    /// a sitemap): the persona that asked before, else one chosen now and
    /// remembered ([`robots::pick_scout`]). `None` when nobody can ask.
    /// An order for the file names it as `pin` and says `scout`.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn scout_for(
        &self,
        url: &Url,
        readers: &[PersonaId],
        readers_needed: usize,
    ) -> Result<Option<PersonaId>, SchedulerError> {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Ok(None);
        };
        self.choose_scout(
            &kage_crawl::site::of_host(&host),
            &host,
            readers,
            readers_needed,
        )
        .await
    }

    /// Until when a site's scout is kept from a host because the site
    /// turned it away there: the end of its cooldown. `None` when nothing
    /// keeps it.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn scout_barred(
        &self,
        scout: PersonaId,
        url: &Url,
    ) -> Result<Option<DateTime<Utc>>, SchedulerError> {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Ok(None);
        };
        let now = self.now();
        Ok(self
            .backend()
            .facts(&host)
            .await?
            .states
            .get(&scout)
            .and_then(|state| state.cooldown_until)
            .filter(|until| *until > now))
    }

    async fn choose_scout(
        &self,
        site: &str,
        host: &str,
        readers: &[PersonaId],
        readers_needed: usize,
    ) -> Result<Option<PersonaId>, SchedulerError> {
        let _one = self.desk.choosing.lock().await;
        let remembered = self.backend().scout(site).await?;
        let personas = self.backend().personas().await?;
        let mut all_readers = self.backend().readers(site, self.lately()).await?;
        all_readers.extend_from_slice(readers);
        let policy = self.verdicts.policies().current().domains.resolve(host);
        let picked = robots::pick_scout(
            site,
            remembered,
            &personas,
            &all_readers,
            &policy,
            readers_needed,
        );
        if let Some(scout) = picked
            && remembered != Some(scout)
        {
            self.backend().set_scout(site, scout, self.now()).await?;
            tracing::info!(site, persona = %scout, "a site has a scout");
        }
        self.desk.note_scout(site, picked.or(remembered));
        Ok(picked)
    }

    /// Gives a site another scout, on the operator's word (F51): a scout
    /// that is not ready closes a site that enforces its file until it is
    /// back or retired. The new one is chosen as [`robots::pick_scout`]
    /// chooses, for a site that needs one reader, and never the scout the
    /// site had, which reads the site like anybody once it has asked it.
    /// `None`, and the site keeps the scout it has, when nobody else may
    /// ask.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn replace_scout(&self, site: &str) -> Result<Option<NewScout>, SchedulerError> {
        let _one = self.desk.choosing.lock().await;
        let previous = self.backend().scout(site).await?;
        let personas = self.backend().personas().await?;
        let mut readers = self.backend().readers(site, self.lately()).await?;
        readers.extend(previous);
        let policy = self.verdicts.policies().current().domains.resolve(site);
        let Some(scout) = robots::pick_scout(site, None, &personas, &readers, &policy, 1) else {
            return Ok(None);
        };
        self.backend().set_scout(site, scout, self.now()).await?;
        self.desk.note_scout(site, Some(scout));
        tracing::info!(site, persona = %scout, "a site has another scout, on the operator's word");
        Ok(Some(NewScout { previous, scout }))
    }

    /// Sends the site's scout for an origin's `robots.txt` and keeps what
    /// it brought back. One request per origin at a time: whoever comes
    /// second waits for the first one's answer.
    fn ask<'a>(&'a self, call: &'a Call, stop: Option<&'a Stop>) -> Asking<'a> {
        Box::pin(self.ask_now(call, stop))
    }

    async fn ask_now(&self, call: &Call, stop: Option<&Stop>) -> Result<Asked, SchedulerError> {
        let lock = self.desk.asking(&call.origin);
        let _held = match stop {
            Some(stop) => {
                let mut stop = stop.clone();
                tokio::select! {
                    held = lock.lock() => held,
                    () = stopped(&mut stop) => return Ok(Asked::Stopped),
                }
            }
            None => lock.lock().await,
        };
        if let Some(known) = self.robots_fresh(&call.origin).await? {
            return Ok(Asked::Known(known));
        }
        let Some(scout) = self
            .choose_scout(
                &call.site,
                &call.host,
                &call.need.readers,
                call.need.readers_needed,
            )
            .await?
        else {
            metrics::robots("no_scout");
            return Ok(Asked::NoScout);
        };

        // As a person who typed the address: in the scout's browser, once.
        let mut order = FetchOrder::get(call.file.clone(), call.need.priority);
        order.render = RenderMode::Browser;
        // The file as the server sent it, beside what the browser made
        // of it.
        order.capture.raw = true;
        order.arrival = Some(Arrival::Typed);
        order.max_retries = Some(0);
        order.max_body_bytes = Some(self.robots.max_bytes);
        order.pin = Some(scout);
        order.scout = true;
        order.patience = call.need.patience;
        order.spec = serde_json::json!({ "url": call.file.as_str(), "robots": true });
        let mut scouting = self
            .begin(RequestId::new(), order, call.give_up_at, None)
            .await?;
        scouting.overflowed.clone_from(&call.overflowed);
        match Box::pin(self.run(scouting, stop.cloned())).await? {
            Ran::Stopped => Ok(Asked::Stopped),
            Ran::Deferred(deferred) => Ok(Asked::Refused(Refused {
                reason: deferred.reason,
                retry_at: deferred.retry_at,
            })),
            Ran::Finished(report) => self.keep_answer(call, scout, &report).await,
        }
    }

    /// Keeps what the scout's request for an origin's file came to, when
    /// the site had a say in it.
    async fn keep_answer(
        &self,
        call: &Call,
        scout: PersonaId,
        report: &FetchReport,
    ) -> Result<Asked, SchedulerError> {
        let last = report.last.as_ref();
        if report.attempts.is_empty() {
            // Nobody asked: the scheduler said why.
            let refusal = report
                .reason
                .as_deref()
                .and_then(|reason| reason.parse::<Refusal>().ok());
            return Ok(match refusal {
                Some(reason) => Asked::Refused(Refused {
                    reason,
                    retry_at: report.retry_at,
                }),
                None => Asked::NoAnswer,
            });
        }
        if last.is_none_or(|last| last.courtesy == Courtesy::NotTheSite) {
            // The proxy, the sidecar: the site was not asked, and nothing
            // is known of it.
            metrics::robots("not_asked");
            return Ok(Asked::NoAnswer);
        }
        let status = last.and_then(|last| last.status);
        let body = last
            .and_then(|last| last.page.as_deref())
            .and_then(|page| page.raw_body.as_ref())
            .and_then(bytes_of);
        let answer = Answer::of(
            report.outcome,
            status,
            body.as_deref(),
            self.robots.max_bytes,
        );
        let now = self.now();
        let entry = robots::entry_of(&call.origin, &call.site, &answer, status, now, &self.robots);
        self.backend().save_robots(&entry).await?;
        metrics::robots(match answer {
            Answer::File(_) => "file",
            Answer::Missing => "missing",
            Answer::Unreachable => "unreachable",
        });
        tracing::info!(
            origin = call.origin,
            persona = %scout,
            status,
            outcome = report.outcome.as_str(),
            unreachable = entry.unreachable,
            expires_at = %entry.expires_at,
            "a robots.txt was asked for"
        );
        Ok(Asked::Known(self.desk.keep(
            &call.origin,
            Arc::new(Known::of(&entry)),
            now,
        )))
    }

    /// The call for the file of the origin of `url`.
    fn call(url: &Url, need: ScoutNeed) -> Option<Call> {
        let host = url.host_str()?.to_ascii_lowercase();
        Some(Call {
            origin: robots::origin_of(url)?,
            site: kage_crawl::site::of_host(&host),
            host,
            file: robots::file_of(url)?,
            need,
            give_up_at: None,
            overflowed: None,
        })
    }

    /// Sends the scout for an origin's file though no fetch waits for it,
    /// so that the next fetch of the origin has a verdict to report. It is
    /// sent once the fetch that found the file unknown has ended, and
    /// whoever made that fetch's attempts (`readers`) is not chosen.
    pub(super) fn follow_up(&self, mut call: Call, readers: impl IntoIterator<Item = PersonaId>) {
        call.need.readers.extend(readers);
        if !self.desk.follow(&call.origin) {
            return;
        }
        let pipeline = self.clone();
        tokio::spawn(async move {
            let asked = pipeline.ask(&call, None).await;
            let brought = matches!(asked, Ok(Asked::Known(_)));
            if let Err(error) = &asked {
                tracing::warn!(origin = call.origin, %error, "a robots.txt could not be asked for");
            }
            if !brought {
                // Nobody could ask: the next fetches of the origin do not
                // each try again.
                tokio::time::sleep(NOT_ASKED_WAIT).await;
            }
            pipeline.desk.followed(&call.origin);
        });
    }

    /// What `robots.txt` means for the fetch (spec §14). Sets the verdict
    /// its result reports and, where the file is enforced, the gap its
    /// `Crawl-delay` asks for.
    pub(super) async fn robots_gate(
        &self,
        course: &mut Course,
        stop: Option<&Stop>,
    ) -> Result<Gate, SchedulerError> {
        // The scout's own request is for what the file cannot speak of.
        if course.order.scout {
            return Ok(Gate::Open);
        }
        course.robots = Some(robots::unasked());
        let mode = match robots::mode(course.policy.robots, course.order.robots) {
            Ok(mode) => mode,
            Err(conflict) => return Ok(Gate::Denied(Some(conflict.as_str()))),
        };
        if mode == RobotsMode::Ignore {
            return Ok(Gate::Open);
        }
        let order = &course.order;
        let mut readers: Vec<PersonaId> = order.pin.into_iter().collect();
        let mut readers_needed = 1;
        if let Some(visitors) = &order.visitors {
            readers.extend_from_slice(&visitors.known);
            readers_needed = usize::try_from(visitors.max.max(1)).unwrap_or(1);
        }
        let need = ScoutNeed {
            readers,
            readers_needed,
            priority: order.priority,
            patience: order.patience,
        };
        let Some(mut call) = Self::call(&order.url, need) else {
            return Ok(Gate::Open);
        };
        let known = self.robots_fresh(&call.origin).await?;

        if mode == RobotsMode::Report {
            // Nothing waits: the verdict is what is known, and the scout
            // follows after this fetch for the next one.
            if let Some(known) = known {
                if let Some(verdict) = known.verdict(&self.robots.agents, &order.url) {
                    course.robots = Some(verdict);
                }
            } else {
                call.need.priority = Priority::Job;
                call.need.patience = Some(FOLLOW_UP_PATIENCE);
                course.robots_due = Some(call);
            }
            return Ok(Gate::Open);
        }

        // Enforced: nothing of the origin is fetched without a verdict.
        let known = if let Some(known) = known {
            known
        } else {
            call.give_up_at = course.give_up_at;
            call.overflowed.clone_from(&course.overflowed);
            match self.ask(&call, stop).await? {
                Asked::Known(known) => known,
                Asked::NoScout => {
                    return Ok(Gate::Unavailable {
                        reason: ROBOTS_NO_SCOUT,
                        retry_at: self.later(self.robots.error_ttl),
                    });
                }
                // The scout is not ready and nobody knows until when: the
                // verdict cannot be had now.
                Asked::Refused(Refused {
                    reason: Refusal::NoPersona,
                    retry_at: None,
                })
                | Asked::NoAnswer => {
                    return Ok(Gate::Unavailable {
                        reason: ROBOTS_UNAVAILABLE,
                        retry_at: self.later(NOT_ASKED_WAIT),
                    });
                }
                Asked::Refused(refused) => return Ok(Gate::Refused(refused)),
                Asked::Stopped => return Ok(Gate::Stopped),
            }
        };
        let Some(verdict) = known.verdict(&self.robots.agents, &course.order.url) else {
            // Unreachable means disallowed (RFC 9309 §2.3.1.4) until the
            // file is asked for again.
            return Ok(Gate::Unavailable {
                reason: ROBOTS_UNAVAILABLE,
                retry_at: Some(known.expires_at),
            });
        };
        let allowed = verdict.allowed;
        course.gap_floor = robots::gap_floor(&verdict);
        course.robots = Some(verdict);
        Ok(if allowed {
            Gate::Open
        } else {
            Gate::Denied(None)
        })
    }

    /// What a crawl is told about an address before it makes it a page of
    /// its own: whether the site's enforced file is known to disallow it.
    /// Nothing is asked of anybody; an address nothing is known of yet is
    /// [`Peek::Go`], and its fetch waits for the verdict.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn robots_peek(&self, url: &Url, wish: RobotsWish) -> Result<Peek, SchedulerError> {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Ok(Peek::Go);
        };
        let policy = self.verdicts.policies().current().domains.resolve(&host);
        if robots::mode(policy.robots, wish) != Ok(RobotsMode::Enforce) {
            return Ok(Peek::Go);
        }
        let Some(origin) = robots::origin_of(url) else {
            return Ok(Peek::Go);
        };
        let verdict = self
            .robots_fresh(&origin)
            .await?
            .and_then(|known| known.verdict(&self.robots.agents, url));
        Ok(match verdict {
            Some(verdict) if !verdict.allowed => Peek::Denied(verdict),
            _ => Peek::Go,
        })
    }

    /// Reads the `robots.txt` of the origin of `url` for somebody who
    /// waits for it: from what is kept, else by the site's scout.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn robots_read(
        &self,
        url: &Url,
        wish: RobotsWish,
        need: ScoutNeed,
        stop: Stop,
    ) -> Result<RobotsRead, SchedulerError> {
        let Some(call) = Self::call(url, need) else {
            return Ok(RobotsRead::NotAsked);
        };
        let policy = self
            .verdicts
            .policies()
            .current()
            .domains
            .resolve(&call.host);
        if matches!(
            robots::mode(policy.robots, wish),
            Ok(RobotsMode::Ignore) | Err(_)
        ) {
            return Ok(RobotsRead::NotAsked);
        }
        let known = match self.robots_fresh(&call.origin).await? {
            Some(known) => known,
            None => match self.ask(&call, Some(&stop)).await? {
                Asked::Known(known) => known,
                Asked::NoScout => return Ok(RobotsRead::NoScout),
                Asked::Refused(refused) => {
                    return Ok(RobotsRead::Later(Deferred {
                        reason: refused.reason,
                        retry_at: refused.retry_at,
                    }));
                }
                Asked::NoAnswer => {
                    return Ok(RobotsRead::Later(Deferred {
                        reason: Refusal::RobotsUnavailable,
                        retry_at: self.later(NOT_ASKED_WAIT),
                    }));
                }
                Asked::Stopped => return Ok(RobotsRead::Stopped),
            },
        };
        Ok(match &known.standing {
            Standing::Unreachable => RobotsRead::Unreachable {
                until: known.expires_at,
            },
            Standing::Missing => RobotsRead::Read {
                found: false,
                sitemaps: Vec::new(),
            },
            Standing::Rules(_) => RobotsRead::Read {
                found: true,
                sitemaps: known.sitemaps().to_vec(),
            },
        })
    }
}
