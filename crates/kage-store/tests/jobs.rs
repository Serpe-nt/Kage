//! The store as the job worker and the webhook sender use it (spec §16.6):
//! a job stored with its items in one go, idempotency keys and their time,
//! claims under a lease that skip what another worker holds, counters that
//! move with the items, deliveries that wait their turn, and what the
//! retention job and the purge delete.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use chrono::{DateTime, Duration, Utc};
use common::{TestDb, now};
use kage_core::domain::{
    ArtifactKind, DeliveryState, FetchRecord, Job, JobItem, JobItemState, JobKind, JobOrigin,
    JobState, Scope, WebhookAttempt, WebhookDelivery, WebhookEvent,
};
use kage_core::ids::{ApiKeyId, ArtifactId, JobId, JobItemId, ResultId, WebhookDeliveryId};
use kage_core::outcome::Outcome;
use kage_store::Store;
use kage_store::repo::{AttemptEnd, Created, Tally};
use uuid::Uuid;

fn job(api_key_id: ApiKeyId, created_at: DateTime<Utc>, items: u32) -> Job {
    Job {
        id: JobId::new(),
        api_key_id,
        kind: JobKind::Batch,
        state: JobState::Queued,
        spec: serde_json::json!({ "type": "batch" }),
        idempotency_key: None,
        idempotency_hash: None,
        idempotency_until: None,
        origin: JobOrigin::Api,
        webhook_url: None,
        webhook_secret: None,
        items_total: items,
        items_succeeded: 0,
        items_failed: 0,
        items_cancelled: 0,
        cancel_requested: false,
        error: None,
        deadline: Some(created_at + Duration::days(7)),
        paused_until: None,
        paused_reason: None,
        created_at,
        started_at: None,
        finished_at: None,
    }
}

fn items(job: &Job, sites: &[&str]) -> Vec<JobItem> {
    sites
        .iter()
        .zip(0_u64..)
        .map(|(site, seq)| JobItem {
            id: JobItemId::new(),
            job_id: job.id,
            seq,
            state: JobItemState::Queued,
            input: serde_json::json!({ "url": format!("https://{site}/{seq}") }),
            site: Some((*site).to_owned()),
            request_id: None,
            outcome: None,
            output: None,
            error: None,
            not_before: None,
            claims: 0,
            created_at: job.created_at,
            started_at: None,
            finished_at: None,
        })
        .collect()
}

async fn stored(store: &Store, id: JobId) -> Job {
    store.jobs().get(id).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_job_is_stored_with_its_items_or_not_at_all() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    // More items than one statement inserts.
    let sites: Vec<&str> = (0..1203)
        .map(|n| if n % 3 == 0 { "a.example" } else { "b.example" })
        .collect();
    let parent = job(key.id, start, 1203);
    let all = items(&parent, &sites);

    assert_eq!(
        db.store.jobs().create(&parent, &all, start).await.unwrap(),
        Created::New
    );

    assert_eq!(stored(&db.store, parent.id).await, parent);
    let open = db.store.job_items().open(parent.id).await.unwrap();
    assert_eq!(open, all);
    assert_eq!(
        db.store
            .job_items()
            .list(parent.id, Some(1200), 10)
            .await
            .unwrap(),
        all[1201..]
    );

    // An item that cannot be stored leaves no job behind.
    let broken = job(key.id, start, 2);
    let mut twice = items(&broken, &["a.example", "a.example"]);
    twice[1].seq = 0;
    assert!(
        db.store
            .jobs()
            .create(&broken, &twice, start)
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert_eq!(db.store.jobs().get(broken.id).await.unwrap(), None);
}

#[tokio::test]
async fn an_idempotency_key_names_its_job_until_its_time_is_over() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let other = common::api_key("other", &[Scope::Jobs]);
    db.store.api_keys().insert(&other).await.unwrap();
    let start = now();
    let keyed = |api_key_id, at: DateTime<Utc>| {
        let mut made = job(api_key_id, at, 1);
        made.idempotency_key = Some("order-17".to_owned());
        made.idempotency_hash = Some([7; 32]);
        made.idempotency_until = Some(at + Duration::hours(24));
        made
    };
    let first = keyed(key.id, start);
    let first_items = items(&first, &["a.example"]);
    assert_eq!(
        db.store
            .jobs()
            .create(&first, &first_items, start)
            .await
            .unwrap(),
        Created::New
    );

    // The same key again within the day: the first job, and nothing stored.
    let again = keyed(key.id, start + Duration::hours(23));
    let answer = db
        .store
        .jobs()
        .create(
            &again,
            &items(&again, &["a.example"]),
            start + Duration::hours(23),
        )
        .await
        .unwrap();
    assert_eq!(answer, Created::Existing(Box::new(first.clone())));
    assert_eq!(db.store.jobs().get(again.id).await.unwrap(), None);
    assert_eq!(
        db.store.job_items().open(again.id).await.unwrap(),
        Vec::new()
    );
    // Another key's use of the same words is its own.
    let theirs = keyed(other.id, start);
    assert_eq!(
        db.store.jobs().create(&theirs, &[], start).await.unwrap(),
        Created::New
    );

    // A day later the key names nothing: a new job, and the old one keeps
    // everything but the key.
    let later = start + Duration::hours(24);
    let second = keyed(key.id, later);
    assert_eq!(
        db.store.jobs().create(&second, &[], later).await.unwrap(),
        Created::New
    );
    assert_eq!(
        db.store
            .jobs()
            .find_by_idempotency_key(key.id, "order-17")
            .await
            .unwrap()
            .map(|job| job.id),
        Some(second.id)
    );
    let old = stored(&db.store, first.id).await;
    assert_eq!(old.idempotency_key, None);
    assert_eq!(old.idempotency_hash, Some([7; 32]));
    assert_eq!(db.store.job_items().open(first.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_claim_is_a_lease_that_another_worker_respects_until_it_runs_out() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let jobs = db.store.jobs();
    let first = job(key.id, start, 1);
    let second = job(key.id, start + Duration::seconds(1), 1);
    let third = job(key.id, start + Duration::seconds(2), 1);
    for one in [&third, &first, &second] {
        jobs.create(one, &[], start).await.unwrap();
    }
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let at = start + Duration::seconds(10);
    let lease = at + Duration::seconds(60);

    // Oldest first, as many as asked for; a queued job is running once
    // claimed.
    let claimed = jobs.claim(alice, at, lease, 2).await.unwrap();
    assert_eq!(
        claimed.iter().map(|job| job.id).collect::<Vec<_>>(),
        [first.id, second.id]
    );
    assert!(claimed.iter().all(|job| job.state == JobState::Running));
    assert!(claimed.iter().all(|job| job.started_at == Some(at)));
    // What is leased is not there for another worker; the rest is.
    let others = jobs.claim(bob, at, lease, 10).await.unwrap();
    assert_eq!(
        others.iter().map(|job| job.id).collect::<Vec<_>>(),
        [third.id]
    );
    assert_eq!(jobs.claim(bob, at, lease, 10).await.unwrap(), Vec::new());

    // A lease that is renewed holds; one that ran out is anybody's.
    let renewed = jobs
        .renew(first.id, alice, lease + Duration::seconds(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renewed.id, first.id);
    let after_lease = lease + Duration::seconds(1);
    let taken = jobs
        .claim(bob, after_lease, after_lease + Duration::seconds(60), 10)
        .await
        .unwrap();
    // The one the first worker let go of, and the second worker's own,
    // whose lease it did not renew either.
    assert_eq!(
        taken.iter().map(|job| job.id).collect::<Vec<_>>(),
        [second.id, third.id]
    );
    // It started when it first started.
    assert_eq!(taken[0].started_at, Some(at));
    // The worker that lost it is told so when it renews.
    assert_eq!(
        jobs.renew(second.id, alice, after_lease + Duration::seconds(60))
            .await
            .unwrap(),
        None
    );
    assert!(!jobs.release(second.id, alice, None).await.unwrap());

    // Given back, a job is free at once.
    assert!(jobs.release(first.id, alice, None).await.unwrap());
    let back = jobs
        .claim(bob, after_lease, after_lease + Duration::seconds(60), 10)
        .await
        .unwrap();
    assert_eq!(
        back.iter().map(|job| job.id).collect::<Vec<_>>(),
        [first.id]
    );
}

#[tokio::test]
async fn two_workers_claiming_at_once_never_get_the_same_job() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    for n in 0..40 {
        let one = job(key.id, start + Duration::milliseconds(n), 0);
        db.store.jobs().create(&one, &[], start).await.unwrap();
    }
    let at = start + Duration::seconds(5);
    let claim = |worker: Uuid| {
        let store = db.store.clone();
        async move {
            let mut mine = Vec::new();
            loop {
                let got = store
                    .jobs()
                    .claim(worker, at, at + Duration::seconds(60), 3)
                    .await
                    .unwrap();
                if got.is_empty() {
                    return mine;
                }
                mine.extend(got.into_iter().map(|job| job.id));
            }
        }
    };

    let (a, b, c) = tokio::join!(
        tokio::spawn(claim(Uuid::now_v7())),
        tokio::spawn(claim(Uuid::now_v7())),
        tokio::spawn(claim(Uuid::now_v7())),
    );

    let mut all: Vec<JobId> = [a.unwrap(), b.unwrap(), c.unwrap()].concat();
    assert_eq!(all.len(), 40);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 40);
}

#[tokio::test]
async fn a_paused_job_is_left_alone_until_what_it_waits_for_is_over() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let jobs = db.store.jobs();
    let waiting = job(key.id, start, 1);
    jobs.create(&waiting, &[], start).await.unwrap();
    let worker = Uuid::now_v7();
    let lease = |at: DateTime<Utc>| at + Duration::seconds(60);
    assert_eq!(
        jobs.claim(worker, start, lease(start), 1)
            .await
            .unwrap()
            .len(),
        1
    );

    // Everything left of it waits for a paused site: given back paused.
    let until = start + Duration::minutes(30);
    assert!(
        jobs.release(waiting.id, worker, Some((until, "domain_paused")))
            .await
            .unwrap()
    );
    let paused = stored(&db.store, waiting.id).await;
    assert_eq!(paused.state, JobState::Paused);
    assert_eq!(paused.paused_until, Some(until));
    assert_eq!(paused.paused_reason.as_deref(), Some("domain_paused"));

    let early = start + Duration::minutes(29);
    assert_eq!(
        jobs.claim(worker, early, lease(early), 1).await.unwrap(),
        []
    );
    // A cancel is a reason to look at it before its time.
    let cancelled = jobs.request_cancel(waiting.id).await.unwrap().unwrap();
    assert!(cancelled.cancel_requested);
    let taken = jobs.claim(worker, early, lease(early), 1).await.unwrap();
    assert_eq!(taken.len(), 1);
    // It is still paused until its worker says otherwise.
    assert_eq!(taken[0].state, JobState::Paused);
    assert!(jobs.set_paused(waiting.id, None).await.unwrap());
    let running = stored(&db.store, waiting.id).await;
    assert_eq!(
        (running.state, running.paused_until, running.paused_reason),
        (JobState::Running, None, None)
    );
    assert!(
        jobs.set_paused(waiting.id, Some((until, "bandwidth_exhausted")))
            .await
            .unwrap()
    );
    assert_eq!(stored(&db.store, waiting.id).await.state, JobState::Paused);

    // Its time, and its deadline, each end the wait.
    let another = job(key.id, start, 1);
    jobs.create(&another, &[], start).await.unwrap();
    assert_eq!(
        jobs.claim(worker, start, lease(start), 5)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        jobs.release(
            another.id,
            worker,
            Some((start + Duration::days(30), "no_persona"))
        )
        .await
        .unwrap()
    );
    let at_deadline = start + Duration::days(7);
    let due = jobs
        .claim(Uuid::now_v7(), at_deadline, lease(at_deadline), 5)
        .await
        .unwrap();
    assert!(due.iter().any(|job| job.id == another.id));
    // No such job: nothing to cancel.
    assert_eq!(jobs.request_cancel(JobId::new()).await.unwrap(), None);
}

#[tokio::test]
async fn the_counters_of_a_job_move_with_its_items_and_never_twice() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let parent = job(key.id, start, 5);
    let all = items(
        &parent,
        &[
            "a.example",
            "a.example",
            "b.example",
            "a.example",
            "b.example",
        ],
    );
    db.store.jobs().create(&parent, &all, start).await.unwrap();
    let repo = db.store.job_items();
    let request = common::request(Some(key.id));
    db.store.requests().insert(&request).await.unwrap();

    // Taken up, put back, taken up again.
    assert!(
        repo.start(all[0].id, Some(request.id), start)
            .await
            .unwrap()
    );
    let wait_until = start + Duration::minutes(30);
    assert!(repo.defer(all[0].id, Some(wait_until)).await.unwrap());
    let deferred = &repo.open(parent.id).await.unwrap()[0];
    assert_eq!(
        (deferred.state, deferred.not_before, deferred.claims),
        (JobItemState::Queued, Some(wait_until), 1)
    );
    assert_eq!(deferred.request_id, Some(request.id));
    let later = start + Duration::minutes(31);
    assert!(repo.start(all[0].id, None, later).await.unwrap());
    let running = &repo.open(parent.id).await.unwrap()[0];
    assert_eq!(
        (running.state, running.not_before, running.claims),
        (JobItemState::Running, None, 2)
    );
    // The request it had is the request it keeps, and it started once.
    assert_eq!(running.request_id, Some(request.id));
    assert_eq!(running.started_at, Some(start));
    assert_eq!(
        repo.find_by_request(request.id)
            .await
            .unwrap()
            .map(|item| item.id),
        Some(all[0].id)
    );

    let mut done = running.clone();
    done.state = JobItemState::Succeeded;
    done.outcome = Some(Outcome::Ok);
    done.finished_at = Some(later);
    let after = repo.finish(&done).await.unwrap().unwrap();
    assert_eq!(
        (
            after.items_succeeded,
            after.items_failed,
            after.items_cancelled
        ),
        (1, 0, 0)
    );
    // Said twice, counted once.
    let again = repo.finish(&done).await.unwrap().unwrap();
    assert_eq!(again.items_succeeded, 1);
    assert_eq!(again.items_pending(), 4);

    let mut failed = all[1].clone();
    failed.state = JobItemState::Failed;
    failed.outcome = Some(Outcome::Forbidden);
    failed.error = Some("forbidden".to_owned());
    failed.finished_at = Some(later);
    let after = repo.finish(&failed).await.unwrap().unwrap();
    assert_eq!((after.items_succeeded, after.items_failed), (1, 1));

    // A cancel ends what waits, and leaves what is being worked.
    assert!(repo.start(all[2].id, None, later).await.unwrap());
    let cancelled = repo
        .end_queued(parent.id, JobItemState::Cancelled, Some("cancelled"), later)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.items_cancelled, 2);
    assert_eq!(cancelled.items_pending(), 1);
    let open = repo.open(parent.id).await.unwrap();
    assert_eq!(
        open.iter().map(|item| item.id).collect::<Vec<_>>(),
        [all[2].id]
    );
    assert_eq!(repo.backlog().await.unwrap(), (0, 1));
    let listed = repo.list(parent.id, None, 10).await.unwrap();
    assert_eq!(
        listed.iter().map(|item| item.state).collect::<Vec<_>>(),
        [
            JobItemState::Succeeded,
            JobItemState::Failed,
            JobItemState::Running,
            JobItemState::Cancelled,
            JobItemState::Cancelled,
        ]
    );
    assert_eq!(listed[3].error.as_deref(), Some("cancelled"));

    // The job ends as its items did.
    let mut last = open[0].clone();
    last.state = JobItemState::Succeeded;
    last.finished_at = Some(later);
    let ended = repo.finish(&last).await.unwrap().unwrap();
    assert_eq!(ended.items_pending(), 0);
    let cancel = db
        .store
        .jobs()
        .request_cancel(parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.final_state(), JobState::Cancelled);
    let finished = db
        .store
        .jobs()
        .finish(parent.id, cancel.final_state(), None, later)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished.state, JobState::Cancelled);
    assert_eq!(finished.finished_at, Some(later));
    // Ended once.
    assert_eq!(
        db.store
            .jobs()
            .finish(parent.id, JobState::Failed, Some("late"), later)
            .await
            .unwrap(),
        None
    );
    // A job that ended is nobody's to claim, renew or cancel.
    assert_eq!(
        db.store
            .jobs()
            .claim(Uuid::now_v7(), later, later + Duration::minutes(1), 5)
            .await
            .unwrap(),
        []
    );
    let counts = db.store.jobs().counts().await.unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!(
        (counts[0].kind, counts[0].state, counts[0].jobs),
        (JobKind::Batch, JobState::Cancelled, 1)
    );
    assert_eq!(Tally::of(JobItemState::Running), Tally::default());
}

#[tokio::test]
async fn the_visitors_of_a_job_are_kept_with_it() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let parent = job(key.id, now(), 0);
    db.store.jobs().create(&parent, &[], now()).await.unwrap();

    assert_eq!(
        db.store.jobs().visitors(parent.id).await.unwrap(),
        serde_json::json!({})
    );
    let visitors = serde_json::json!({ "a.example": ["0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"] });
    db.store
        .jobs()
        .set_visitors(parent.id, &visitors)
        .await
        .unwrap();
    assert_eq!(db.store.jobs().visitors(parent.id).await.unwrap(), visitors);
    assert_eq!(
        db.store.jobs().visitors(JobId::new()).await.unwrap(),
        serde_json::json!({})
    );
}

fn delivery(job: &Job, event: WebhookEvent, origin: &str, at: DateTime<Utc>) -> WebhookDelivery {
    WebhookDelivery {
        id: WebhookDeliveryId::new(),
        job_id: job.id,
        event,
        origin: origin.to_owned(),
        body: format!("{{\"type\":\"{event}\"}}"),
        state: DeliveryState::Pending,
        attempts: 0,
        next_attempt_at: Some(at),
        created_at: at,
        finished_at: None,
    }
}

#[tokio::test]
async fn deliveries_are_claimed_one_per_receiver_and_logged_attempt_by_attempt() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let parent = job(key.id, start, 0);
    db.store.jobs().create(&parent, &[], start).await.unwrap();
    let hooks = db.store.webhooks();
    let (app, other) = ("https://hooks.example.org:443", "http://app:3000");
    let first = delivery(&parent, WebhookEvent::JobCompleted, app, start);
    let second = delivery(
        &parent,
        WebhookEvent::JobCompleted,
        app,
        start + Duration::seconds(1),
    );
    let elsewhere = delivery(
        &parent,
        WebhookEvent::JobFailed,
        other,
        start + Duration::seconds(2),
    );
    for one in [&first, &second, &elsewhere] {
        hooks.enqueue(one).await.unwrap();
    }
    assert_eq!(hooks.pending().await.unwrap(), (3, Some(start)));
    assert_eq!(hooks.get(first.id).await.unwrap(), Some(first.clone()));

    // Nothing is due before its time.
    let before = start - Duration::seconds(1);
    assert_eq!(
        hooks
            .claim_due(before, before + Duration::seconds(30), &[])
            .await
            .unwrap(),
        None
    );
    // The oldest first; a receiver with one in flight is passed over.
    let at = start + Duration::seconds(5);
    let lease = at + Duration::seconds(30);
    let one = hooks.claim_due(at, lease, &[]).await.unwrap().unwrap();
    assert_eq!(one.id, first.id);
    let two = hooks
        .claim_due(at, lease, &[app.to_owned()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(two.id, elsewhere.id);
    assert_eq!(
        hooks
            .claim_due(at, lease, &[app.to_owned(), other.to_owned()])
            .await
            .unwrap(),
        None
    );
    // What is claimed is not claimed again while its lease holds.
    let free = hooks.claim_due(at, lease, &[]).await.unwrap().unwrap();
    assert_eq!(free.id, second.id);
    assert_eq!(hooks.claim_due(at, lease, &[]).await.unwrap(), None);
    hooks.unclaim(second.id).await.unwrap();
    assert_eq!(
        hooks.claim_due(at, lease, &[]).await.unwrap().map(|d| d.id),
        Some(second.id)
    );

    // A failure with a retry: logged, and due again later.
    let retry_at = at + Duration::seconds(30);
    let failed = WebhookAttempt {
        delivery_id: first.id,
        n: 1,
        started_at: at,
        duration_ms: 120,
        status: Some(503),
        error: None,
    };
    hooks
        .record(&failed, AttemptEnd::Retry(retry_at))
        .await
        .unwrap();
    let waiting = hooks.get(first.id).await.unwrap().unwrap();
    assert_eq!(
        (waiting.state, waiting.attempts, waiting.next_attempt_at),
        (DeliveryState::Pending, 1, Some(retry_at))
    );
    // Then the receiver takes it.
    let delivered = WebhookAttempt {
        delivery_id: first.id,
        n: 2,
        started_at: retry_at,
        duration_ms: 80,
        status: Some(204),
        error: None,
    };
    hooks
        .record(&delivered, AttemptEnd::Delivered)
        .await
        .unwrap();
    let done = hooks.get(first.id).await.unwrap().unwrap();
    assert_eq!(
        (done.state, done.attempts, done.next_attempt_at),
        (DeliveryState::Delivered, 2, None)
    );
    assert_eq!(
        done.finished_at,
        Some(retry_at + Duration::milliseconds(80))
    );
    assert_eq!(hooks.attempts(first.id).await.unwrap(), [failed, delivered]);
    // One that never got through.
    let refused = WebhookAttempt {
        delivery_id: elsewhere.id,
        n: 1,
        started_at: at,
        duration_ms: 10_000,
        status: None,
        error: Some("timeout".to_owned()),
    };
    hooks.record(&refused, AttemptEnd::Failed).await.unwrap();
    assert_eq!(
        hooks.get(elsewhere.id).await.unwrap().unwrap().state,
        DeliveryState::Failed
    );
    assert_eq!(hooks.attempts(elsewhere.id).await.unwrap(), [refused]);

    let listed = hooks.list_for_job(parent.id, 2).await.unwrap();
    assert_eq!(
        listed.iter().map(|d| d.id).collect::<Vec<_>>(),
        [elsewhere.id, second.id]
    );
    let mut events = hooks.events_of(parent.id).await.unwrap();
    events.sort_unstable();
    assert_eq!(
        events,
        [WebhookEvent::JobCompleted, WebhookEvent::JobFailed]
    );
    assert_eq!(hooks.events_of(JobId::new()).await.unwrap(), []);
}

#[tokio::test]
async fn a_progress_event_that_was_overtaken_is_not_delivered_late() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let parent = job(key.id, start, 0);
    let sibling = job(key.id, start, 0);
    db.store.jobs().create(&parent, &[], start).await.unwrap();
    db.store.jobs().create(&sibling, &[], start).await.unwrap();
    let hooks = db.store.webhooks();
    let origin = "https://hooks.example.org:443";
    let progress = |job: &Job, seconds| {
        delivery(
            job,
            WebhookEvent::JobProgress,
            origin,
            start + Duration::seconds(seconds),
        )
    };
    let ten = progress(&parent, 0);
    let twenty = progress(&parent, 10);
    let theirs = progress(&sibling, 5);
    hooks.enqueue(&ten).await.unwrap();
    hooks.enqueue(&theirs).await.unwrap();
    // One that is being attempted right now is not taken away.
    let in_flight = hooks
        .claim_due(start, start + Duration::seconds(30), &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(in_flight.id, ten.id);
    hooks.enqueue(&twenty).await.unwrap();
    assert_eq!(
        hooks.get(ten.id).await.unwrap().unwrap().state,
        DeliveryState::Pending
    );

    // The next one overtakes the one that only waits; another job's event
    // is untouched, and so is anything that is not progress.
    let thirty = progress(&parent, 20);
    hooks.enqueue(&thirty).await.unwrap();
    let overtaken = hooks.get(twenty.id).await.unwrap().unwrap();
    assert_eq!(overtaken.state, DeliveryState::Superseded);
    assert_eq!(overtaken.next_attempt_at, None);
    assert_eq!(overtaken.finished_at, Some(thirty.created_at));
    assert_eq!(
        hooks.get(theirs.id).await.unwrap().unwrap().state,
        DeliveryState::Pending
    );
    let end = delivery(
        &parent,
        WebhookEvent::JobCompleted,
        origin,
        start + Duration::seconds(40),
    );
    hooks.enqueue(&end).await.unwrap();
    // The end of the job overtakes the progress that still waits.
    assert_eq!(
        hooks.get(thirty.id).await.unwrap().unwrap().state,
        DeliveryState::Superseded
    );
    assert_eq!(
        hooks.get(end.id).await.unwrap().unwrap().state,
        DeliveryState::Pending
    );
}

/// A request of `key` with a result and one artifact row, accepted at `at`.
async fn fetched(
    store: &Store,
    key: ApiKeyId,
    job_id: Option<JobId>,
    at: DateTime<Utc>,
) -> kage_core::ids::RequestId {
    let mut request = common::request(Some(key));
    request.created_at = at;
    request.job_id = job_id;
    store.requests().insert(&request).await.unwrap();
    let record = FetchRecord {
        id: ResultId::new(),
        request_id: request.id,
        outcome: Outcome::Ok,
        status: Some(200),
        headers: Vec::new(),
        final_url: None,
        redirects: Vec::new(),
        extract: None,
        xhr: None,
        artifacts: Vec::new(),
        persona: None,
        block: None,
        robots: None,
        cached: false,
        reason: None,
        created_at: at,
    };
    store.results().insert(&record).await.unwrap();
    store
        .requests()
        .finish(request.id, Outcome::Ok, at)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO artifacts (id, content_hash, kind, content_type, size_bytes, stored_bytes,
                                encoding, request_id, created_at, expires_at)
         VALUES ($1, $2, $3, 'text/html', 1, 1, 'identity', $4, $5, $6)",
    )
    .bind(ArtifactId::new().as_uuid())
    .bind("a".repeat(64))
    .bind(ArtifactKind::DomHtml.as_str())
    .bind(request.id.as_uuid())
    .bind(at)
    .bind(at + Duration::days(14))
    .execute(store.pool())
    .await
    .unwrap();
    request.id
}

async fn count(store: &Store, table: &str) -> i64 {
    // The table names are this file's own.
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn retention_and_the_purge_delete_a_keys_results_with_what_they_kept() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let other = common::api_key("other", &[Scope::Fetch]);
    db.store.api_keys().insert(&other).await.unwrap();
    let start = now();
    let old = start - Duration::days(20);

    // Fetches by themselves: an old and a new one of each key.
    let mine_old = fetched(&db.store, key.id, None, old).await;
    let mine_new = fetched(&db.store, key.id, None, start).await;
    let theirs_old = fetched(&db.store, other.id, None, old).await;
    // A job that ended long ago, one that ended just now, one that runs.
    let mut ended_old = job(key.id, old, 1);
    ended_old.state = JobState::Succeeded;
    ended_old.finished_at = Some(old + Duration::hours(1));
    let mut ended_new = job(key.id, start, 1);
    ended_new.state = JobState::Succeeded;
    ended_new.finished_at = Some(start);
    let mut running = job(key.id, old, 1);
    running.state = JobState::Running;
    for one in [&ended_old, &ended_new, &running] {
        let its_items = items(one, &["a.example"]);
        db.store
            .jobs()
            .create(one, &its_items, one.created_at)
            .await
            .unwrap();
        fetched(&db.store, key.id, Some(one.id), one.created_at).await;
        db.store
            .webhooks()
            .enqueue(&delivery(
                one,
                WebhookEvent::JobCompleted,
                "https://hooks.example.org:443",
                one.created_at,
            ))
            .await
            .unwrap();
    }
    assert_eq!(count(&db.store, "requests").await, 6);
    assert_eq!(count(&db.store, "artifacts").await, 6);

    // The purge of one key before a moment: its own fetches, not the
    // other key's, and not what is part of a job.
    let cutoff = start - Duration::days(14);
    assert_eq!(
        db.store
            .requests()
            .delete_before(cutoff, Some(key.id))
            .await
            .unwrap(),
        (1, 1)
    );
    assert_eq!(db.store.requests().get(mine_old).await.unwrap(), None);
    assert!(db.store.requests().get(mine_new).await.unwrap().is_some());
    assert!(db.store.requests().get(theirs_old).await.unwrap().is_some());
    assert_eq!(
        db.store.results().get_by_request(mine_old).await.unwrap(),
        None
    );
    assert_eq!(count(&db.store, "requests").await, 5);
    assert_eq!(count(&db.store, "artifacts").await, 5);

    // Finished jobs before the moment go with everything of theirs; a job
    // that still runs stays, however old.
    assert_eq!(
        db.store.jobs().delete_finished(cutoff, None).await.unwrap(),
        1
    );
    assert_eq!(db.store.jobs().get(ended_old.id).await.unwrap(), None);
    assert!(db.store.jobs().get(ended_new.id).await.unwrap().is_some());
    assert!(db.store.jobs().get(running.id).await.unwrap().is_some());
    assert_eq!(count(&db.store, "job_items").await, 2);
    assert_eq!(count(&db.store, "requests").await, 4);
    assert_eq!(count(&db.store, "results").await, 4);
    assert_eq!(count(&db.store, "artifacts").await, 4);
    assert_eq!(count(&db.store, "webhook_deliveries").await, 2);

    // The retention of everything, for every key.
    assert_eq!(
        db.store
            .requests()
            .delete_before(cutoff, None)
            .await
            .unwrap(),
        (1, 1)
    );
    assert_eq!(db.store.requests().get(theirs_old).await.unwrap(), None);
    // A purge of everything a key has, to this moment.
    let everything = start + Duration::seconds(1);
    assert_eq!(
        db.store
            .jobs()
            .delete_finished(everything, Some(other.id))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        db.store
            .jobs()
            .delete_finished(everything, Some(key.id))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.store
            .requests()
            .delete_before(everything, Some(key.id))
            .await
            .unwrap(),
        (1, 1)
    );
    // What is left is the job that runs, and its request.
    assert_eq!(count(&db.store, "jobs").await, 1);
    assert_eq!(count(&db.store, "requests").await, 1);
    assert_eq!(count(&db.store, "artifacts").await, 1);
}

#[tokio::test]
async fn a_fetch_that_is_carried_on_as_a_job_becomes_part_of_it() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;
    let start = now();
    let request = common::request(Some(key.id));
    db.store.requests().insert(&request).await.unwrap();
    let mut carried = job(key.id, start, 1);
    carried.origin = JobOrigin::SyncOverflow;
    carried.state = JobState::Running;
    let mut item = items(&carried, &["example.org"]).remove(0);
    item.state = JobItemState::Running;
    item.request_id = Some(request.id);
    item.started_at = Some(start);
    db.store
        .jobs()
        .create(&carried, &[item.clone()], start)
        .await
        .unwrap();

    assert!(
        db.store
            .requests()
            .adopt(request.id, carried.id)
            .await
            .unwrap()
    );

    let adopted = db.store.requests().get(request.id).await.unwrap().unwrap();
    assert_eq!(adopted.job_id, Some(carried.id));
    assert_eq!(
        stored(&db.store, carried.id).await.origin,
        JobOrigin::SyncOverflow
    );
    assert_eq!(db.store.job_items().open(carried.id).await.unwrap(), [item]);
    assert!(
        !db.store
            .requests()
            .adopt(kage_core::ids::RequestId::new(), carried.id)
            .await
            .unwrap()
    );
}
