//! Port of `pkg/controller/cronjob` (`cronjob_controllerv2.go`).
//!
//! For each CronJob: find the most recent unmet schedule time; honour
//! `startingDeadlineSeconds` and `concurrencyPolicy` (Allow / Forbid / Replace);
//! create the Job `<cronjob>-<minutes since epoch>`; keep `status.active` and
//! `lastScheduleTime` / `lastSuccessfulTime` up to date; prune finished Jobs
//! beyond the history limits; requeue at the next schedule time.

pub mod schedule;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_api::batch::v1::{CronJob, CronJobStatus, Job};
use k8s_api::core::v1::ObjectReference;
use k8s_apimachinery::meta::{parse_time, ObjectMeta};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Store};
use k8s_client_go::tools::record::{object_ref, EventRecorder, NORMAL, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client, DeleteParams};

use crate::controller_utils::*;
use crate::job::is_finished;
use crate::ControllerContext;
use schedule::Schedule;

pub const SCHEDULED_TIMESTAMP_ANNOTATION: &str = "batch.kubernetes.io/cronjob-scheduled-timestamp";

pub struct Controller {
    client: Client,
    store: Store<CronJob>,
    job_store: Store<Job>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<CronJob>();
        let job_inf = ctx.informers.informer::<Job>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            job_store: job_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("cronjob-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(job_inf.subscribe(), move |ev| {
            if let Some(k) = owner_key(&ev.object().metadata, "CronJob") {
                me.queue.add(k);
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("cronjob-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k, Utc::now()).await }
        })
        .await;
    }

    async fn sync(&self, key: &str, now: DateTime<Utc>) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(cj) = self.store.get(key) else { return Ok(None) };
        let spec = cj.spec.clone().unwrap_or_default();
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let uid = cj.metadata.uid.clone().unwrap_or_default();
        let jobs: Vec<Arc<Job>> = owned_by(self.job_store.list_namespace(ns), &uid);

        let mut st = cj.status.clone().unwrap_or_default();
        // Reconcile status.active with the jobs that exist and are unfinished.
        let mut active: Vec<ObjectReference> = jobs
            .iter()
            .filter(|j| is_finished(j).is_none() && j.metadata.deletion_timestamp.is_none())
            .map(|j| object_ref(j.as_ref()))
            .collect();
        active.sort_by(|a, b| a.name.cmp(&b.name));
        for j in jobs.iter().filter(|j| is_finished(j).is_some_and(|c| c.type_ == "Complete")) {
            let done = j.status.as_ref().and_then(|s| s.completion_time.clone());
            if done.as_deref().and_then(parse_time) > st.last_successful_time.as_deref().and_then(parse_time) {
                st.last_successful_time = done;
            }
        }

        let sched = match Schedule::parse(&spec.schedule) {
            Ok(s) => s,
            Err(e) => {
                self.recorder.event(
                    cj.as_ref(),
                    WARNING,
                    "UnparseableSchedule",
                    format!("unparseable schedule: {:?} : {e}", spec.schedule),
                );
                return Ok(None);
            }
        };
        let mut requeue = sched.next(now).map(|n| (n - now).to_std().unwrap_or_default() + Duration::from_millis(100));

        if cj.metadata.deletion_timestamp.is_none() && !spec.suspend.unwrap_or(false) {
            let earliest =
                st.last_schedule_time.as_deref().or(cj.metadata.creation_timestamp.as_deref()).and_then(parse_time).unwrap_or(now);
            let (scheduled, missed) = sched.most_recent(earliest, now);
            if missed > 100 {
                self.recorder.event(
                    cj.as_ref(),
                    WARNING,
                    "TooManyMissedTimes",
                    format!("too many missed start times: {missed}. Set or decrease .spec.startingDeadlineSeconds or check clock skew"),
                );
            }
            if let Some(t) = scheduled {
                let too_late = spec.starting_deadline_seconds.is_some_and(|d| (now - t).num_seconds() > d);
                let job_name = format!("{}-{}", cj.metadata.name(), t.timestamp() / 60);
                let exists = jobs.iter().any(|j| j.metadata.name() == job_name);
                let policy = spec.concurrency_policy.as_deref().unwrap_or("Allow");
                if too_late {
                    self.recorder.event(
                        cj.as_ref(),
                        WARNING,
                        "MissSchedule",
                        format!("Missed scheduled time to start a job: {}", t.to_rfc3339()),
                    );
                } else if exists {
                    // Already created for this time.
                } else if policy == "Forbid" && !active.is_empty() {
                    self.recorder.event(
                        cj.as_ref(),
                        NORMAL,
                        "JobAlreadyActive",
                        "Not starting job because prior execution is running and concurrency policy is Forbid",
                    );
                } else {
                    if policy == "Replace" {
                        for a in active.drain(..) {
                            let name = a.name.clone().unwrap_or_default();
                            match jobs_api.delete_with(&name, &DeleteParams::background()).await {
                                Ok(_) => self.recorder.event(cj.as_ref(), NORMAL, "SuccessfulDelete", format!("Deleted job {name}")),
                                Err(e) if e.is_not_found() => {}
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                    let job = job_from_template(&cj, &job_name, t);
                    match jobs_api.create(&job).await {
                        Ok(j) => {
                            self.recorder.event(cj.as_ref(), NORMAL, "SuccessfulCreate", format!("Created job {}", j.metadata.name()));
                            active.push(object_ref(&j));
                        }
                        Err(e) if e.is_already_exists() => {}
                        Err(e) => return Err(e.into()),
                    }
                    st.last_schedule_time = Some(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                }
            }
        } else {
            requeue = None;
        }
        st.active = if active.is_empty() { None } else { Some(active) };

        self.cleanup_history(&cj, &jobs, &jobs_api).await?;

        if cj.status.as_ref() != Some(&st) && !(cj.status.is_none() && st == CronJobStatus::default()) {
            let mut n = (*cj).clone();
            n.status = Some(st);
            let api: Api<CronJob> = Api::namespaced(self.client.clone(), ns);
            match api.replace_status(&n).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(requeue)
    }

    async fn cleanup_history(&self, cj: &CronJob, jobs: &[Arc<Job>], api: &Api<Job>) -> anyhow::Result<()> {
        let spec = cj.spec.clone().unwrap_or_default();
        for (kind, limit) in
            [("Complete", spec.successful_jobs_history_limit.unwrap_or(3)), ("Failed", spec.failed_jobs_history_limit.unwrap_or(1))]
        {
            let mut finished: Vec<&Arc<Job>> = jobs.iter().filter(|j| is_finished(j).is_some_and(|c| c.type_ == kind)).collect();
            if finished.len() <= limit.max(0) as usize {
                continue;
            }
            finished.sort_by(|a, b| a.metadata.creation_timestamp.cmp(&b.metadata.creation_timestamp));
            let excess = finished.len() - limit.max(0) as usize;
            for j in finished.into_iter().take(excess) {
                match api.delete_with(j.metadata.name(), &DeleteParams::background()).await {
                    Ok(_) => self.recorder.event(cj, NORMAL, "SuccessfulDelete", format!("Deleted job {}", j.metadata.name())),
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    }
}

/// `getJobFromTemplate2`
fn job_from_template(cj: &CronJob, name: &str, scheduled: DateTime<Utc>) -> Job {
    let jt = cj.spec.clone().unwrap_or_default().job_template;
    let tmeta = jt.metadata.unwrap_or_default();
    let mut annotations = tmeta.annotations.unwrap_or_default();
    annotations.insert(SCHEDULED_TIMESTAMP_ANNOTATION.into(), scheduled.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let mut job = Job::new(ObjectMeta {
        name: Some(name.into()),
        namespace: cj.metadata.namespace.clone(),
        labels: tmeta.labels,
        annotations: Some(annotations),
        owner_references: Some(vec![new_controller_ref(cj)]),
        ..Default::default()
    });
    job.spec = jt.spec;
    job
}
