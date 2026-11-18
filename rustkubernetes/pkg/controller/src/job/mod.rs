//! Port of `pkg/controller/job` (core semantics).
//!
//! * runs `min(parallelism, completions - succeeded)` pods at a time;
//! * `completions` unset = work-queue job: done when one pod succeeded and none are active;
//! * `Indexed` completion mode: one pod per index with `JOB_COMPLETION_INDEX`;
//! * fails with `BackoffLimitExceeded` after more than `backoffLimit` failed pods,
//!   or `DeadlineExceeded` after `activeDeadlineSeconds`;
//! * `suspend: true` deletes active pods and sets the `Suspended` condition.
//!
//! Simplifications: finished pods are counted from the pod objects themselves
//! (no tracking finalizers) and failed pods are replaced without exponential backoff.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use k8s_api::batch::v1::{Job, JobCondition, JobStatus};
use k8s_api::core::v1::{EnvVar, Pod};
use k8s_apimachinery::meta::{now_rfc3339, parse_time};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const COMPLETION_INDEX_ANNOTATION: &str = "batch.kubernetes.io/job-completion-index";

pub struct Controller {
    client: Client,
    store: Store<Job>,
    pod_store: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    expectations: Expectations,
    recorder: EventRecorder,
    resync: Duration,
}

/// Whether a job has a `Complete` or `Failed` condition that is `True`.
pub fn is_finished(job: &Job) -> Option<&JobCondition> {
    job.status.as_ref()?.conditions.as_ref()?.iter().find(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
}

fn pod_index(p: &Pod) -> Option<i32> {
    p.metadata.annotations.as_ref()?.get(COMPLETION_INDEX_ANNOTATION)?.parse().ok()
}

/// `0,1,2,5` → `0-2,5`
fn compress(idx: &BTreeSet<i32>) -> String {
    let mut out: Vec<String> = vec![];
    let v: Vec<i32> = idx.iter().copied().collect();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        while i + 1 < v.len() && v[i + 1] == v[i] + 1 {
            i += 1;
        }
        out.push(if v[i] > start { format!("{start}-{}", v[i]) } else { start.to_string() });
        i += 1;
    }
    out.join(",")
}

fn set_condition(st: &mut JobStatus, type_: &str, status: &str, reason: &str, message: &str) -> bool {
    let now = now_rfc3339();
    let conds = st.conditions.get_or_insert_with(Vec::new);
    if let Some(c) = conds.iter_mut().find(|c| c.type_ == type_) {
        if c.status == status && c.reason.as_deref() == Some(reason) {
            return false;
        }
        c.status = status.into();
        c.reason = Some(reason.into());
        c.message = Some(message.into());
        c.last_transition_time = Some(now.clone());
        c.last_probe_time = Some(now);
        return true;
    }
    conds.push(JobCondition {
        type_: type_.into(),
        status: status.into(),
        last_probe_time: Some(now.clone()),
        last_transition_time: Some(now),
        reason: Some(reason.into()),
        message: Some(message.into()),
    });
    true
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<Job>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            pod_store: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            expectations: Expectations::default(),
            recorder: ctx.recorder("job-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            let Some(k) = owner_key(&ev.object().metadata, "Job") else { return };
            match &ev {
                Event::Added(_) => me.expectations.creation_observed(&k),
                Event::Deleted(_) => me.expectations.deletion_observed(&k),
                Event::Updated { old, new } if old.metadata.deletion_timestamp.is_none() && new.metadata.deletion_timestamp.is_some() => {
                    me.expectations.deletion_observed(&k)
                }
                _ => {}
            }
            me.queue.add(k);
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("job-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn delete_pods(&self, api: &Api<Pod>, key: &str, pods: &[Arc<Pod>]) -> anyhow::Result<()> {
        self.expectations.expect(key, 0, pods.len() as i64);
        for p in pods {
            match api.delete(p.metadata.name()).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => self.expectations.deletion_observed(key),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(job) = self.store.get(key) else {
            self.expectations.delete(key);
            return Ok(None);
        };
        if is_finished(&job).is_some() {
            return Ok(None);
        }
        let spec = job.spec.clone().unwrap_or_default();
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let owner = new_controller_ref(job.as_ref());
        let selector = selector_of(spec.selector.as_ref());
        let pods =
            claim_pods(&api, self.pod_store.list_namespace(ns), &owner, &selector, job.metadata.deletion_timestamp.is_some()).await?;
        let active: Vec<Arc<Pod>> = pods.iter().filter(|p| is_pod_active(p)).cloned().collect();
        let succeeded_pods: Vec<&Arc<Pod>> = pods.iter().filter(|p| pod_phase(p) == "Succeeded").collect();
        let failed = pods.iter().filter(|p| pod_phase(p) == "Failed").count() as i32;
        let indexed = spec.completion_mode.as_deref() == Some("Indexed");
        let completed_idx: BTreeSet<i32> = succeeded_pods.iter().filter_map(|p| pod_index(p)).collect();
        let succeeded = if indexed { completed_idx.len() as i32 } else { succeeded_pods.len() as i32 };

        let mut st = job.status.clone().unwrap_or_default();
        let suspended = spec.suspend.unwrap_or(false);
        let mut requeue = None;

        if suspended {
            if !active.is_empty() {
                self.delete_pods(&api, key, &active).await?;
            }
            if set_condition(&mut st, "Suspended", "True", "JobSuspended", "Job suspended") {
                self.recorder.event(job.as_ref(), NORMAL, "Suspended", "Job suspended");
            }
            st.start_time = None;
        } else {
            if st.conditions.iter().flatten().any(|c| c.type_ == "Suspended" && c.status == "True") {
                set_condition(&mut st, "Suspended", "False", "JobResumed", "Job resumed");
                self.recorder.event(job.as_ref(), NORMAL, "Resumed", "Job resumed");
            }
            if st.start_time.is_none() {
                st.start_time = Some(now_rfc3339());
            }
            let parallelism = spec.parallelism.unwrap_or(1).max(0);
            let backoff_limit = spec.backoff_limit.unwrap_or(6);
            let deadline_exceeded = spec.active_deadline_seconds.zip(st.start_time.as_deref().and_then(parse_time)).map(|(d, start)| {
                let left = start + chrono::Duration::seconds(d) - Utc::now();
                if left.num_milliseconds() <= 0 {
                    true
                } else {
                    requeue = Some(Duration::from_millis(left.num_milliseconds() as u64));
                    false
                }
            });
            let complete = match (indexed, spec.completions) {
                (true, Some(c)) => (0..c).all(|i| completed_idx.contains(&i)),
                (false, Some(c)) => succeeded >= c,
                (_, None) => succeeded > 0 && active.is_empty(),
            };
            if failed > backoff_limit || deadline_exceeded == Some(true) {
                let (reason, msg) = if failed > backoff_limit {
                    ("BackoffLimitExceeded", "Job has reached the specified backoff limit")
                } else {
                    ("DeadlineExceeded", "Job was active longer than specified deadline")
                };
                self.delete_pods(&api, key, &active).await?;
                set_condition(&mut st, "FailureTarget", "True", reason, msg);
                set_condition(&mut st, "Failed", "True", reason, msg);
                self.recorder.event(job.as_ref(), WARNING, reason, msg);
                requeue = None;
            } else if complete {
                self.delete_pods(&api, key, &active).await?;
                set_condition(&mut st, "SuccessCriteriaMet", "True", "CompletionsReached", "Reached expected number of succeeded pods");
                set_condition(&mut st, "Complete", "True", "CompletionsReached", "Reached expected number of succeeded pods");
                st.completion_time = Some(now_rfc3339());
                self.recorder.event(job.as_ref(), NORMAL, "Completed", "Job completed");
                requeue = None;
            } else if self.expectations.satisfied(key) {
                self.manage(&job, key, &api, &active, &completed_idx, succeeded, parallelism).await?;
            }
        }

        let active_now: Vec<&Arc<Pod>> = pods.iter().filter(|p| is_pod_active(p)).collect();
        st.active = Some(if is_finished_status(&st) || suspended { 0 } else { active_now.len() as i32 });
        st.succeeded = Some(succeeded);
        st.failed = Some(failed);
        st.ready = Some(active_now.iter().filter(|p| is_pod_ready(p)).count() as i32);
        st.terminating = Some(pods.iter().filter(|p| p.metadata.deletion_timestamp.is_some()).count() as i32);
        if indexed {
            st.completed_indexes = Some(compress(&completed_idx));
        }
        if job.status.as_ref() != Some(&st) {
            let mut n = (*job).clone();
            n.status = Some(st);
            let japi: Api<Job> = Api::namespaced(self.client.clone(), ns);
            match japi.replace_status(&n).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(requeue)
    }

    #[allow(clippy::too_many_arguments)]
    async fn manage(
        &self,
        job: &Job,
        key: &str,
        api: &Api<Pod>,
        active: &[Arc<Pod>],
        completed: &BTreeSet<i32>,
        succeeded: i32,
        parallelism: i32,
    ) -> anyhow::Result<()> {
        let spec = job.spec.clone().unwrap_or_default();
        let indexed = spec.completion_mode.as_deref() == Some("Indexed");
        let want_active = match spec.completions {
            Some(c) => parallelism.min((c - succeeded).max(0)),
            None if succeeded > 0 => 0,
            None => parallelism,
        } as usize;
        if active.len() > want_active {
            let mut extra = active.to_vec();
            rank_for_deletion(&mut extra);
            extra.truncate(active.len() - want_active);
            return self.delete_pods(api, key, &extra).await;
        }
        let missing = want_active - active.len();
        if missing == 0 {
            return Ok(());
        }
        // Indexed: next indexes that are neither completed nor running.
        let running_idx: BTreeSet<i32> = active.iter().filter_map(|p| pod_index(p)).collect();
        let indexes: Vec<Option<i32>> = if indexed {
            (0..spec.completions.unwrap_or(0))
                .filter(|i| !completed.contains(i) && !running_idx.contains(i))
                .take(missing)
                .map(Some)
                .collect()
        } else {
            vec![None; missing]
        };
        self.expectations.expect(key, indexes.len() as i64, 0);
        for idx in indexes {
            let gen = match idx {
                Some(i) => format!("{}-{i}-", job.metadata.name()),
                None => format!("{}-", job.metadata.name()),
            };
            let mut pod = pod_from_template(&spec.template, job.metadata.namespace(), &gen, Some(new_controller_ref(job)));
            if let Some(i) = idx {
                pod.metadata.annotations.get_or_insert_with(Default::default).insert(COMPLETION_INDEX_ANNOTATION.into(), i.to_string());
                pod.metadata.labels.get_or_insert_with(Default::default).insert(COMPLETION_INDEX_ANNOTATION.into(), i.to_string());
                let ps = pod.spec.get_or_insert_with(Default::default);
                ps.hostname = Some(format!("{}-{i}", job.metadata.name()));
                for c in ps.containers.iter_mut().chain(ps.init_containers.iter_mut().flatten()) {
                    c.env.get_or_insert_with(Vec::new).push(EnvVar {
                        name: "JOB_COMPLETION_INDEX".into(),
                        value: Some(i.to_string()),
                        value_from: None,
                    });
                }
            }
            match api.create(&pod).await {
                Ok(p) => self.recorder.event(job, NORMAL, "SuccessfulCreate", format!("Created pod: {}", p.metadata.name())),
                Err(e) => {
                    self.expectations.creation_observed(key);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

fn is_finished_status(st: &JobStatus) -> bool {
    st.conditions.iter().flatten().any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_indexes() {
        let s: BTreeSet<i32> = [0, 1, 2, 5, 7, 8].into_iter().collect();
        assert_eq!(compress(&s), "0-2,5,7-8");
        assert_eq!(compress(&BTreeSet::new()), "");
    }
}
