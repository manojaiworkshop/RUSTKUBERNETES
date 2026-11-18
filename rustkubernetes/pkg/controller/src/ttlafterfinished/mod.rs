//! Port of `pkg/controller/ttlafterfinished`: deletes finished Jobs
//! `ttlSecondsAfterFinished` seconds after they completed or failed
//! (dependents such as pods are removed by the garbage collector).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use k8s_api::batch::v1::Job;
use k8s_apimachinery::meta::parse_time;
use k8s_client_go::tools::cache::{split_key, Informer, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client, DeleteParams};

use crate::controller_utils::*;
use crate::job::is_finished;
use crate::ControllerContext;

pub struct Controller {
    client: Client,
    store: Store<Job>,
    inf: Arc<Informer<Job>>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<Job>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            inf: inf.clone(),
            queue: WorkQueue::new(),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| {
            let j = ev.object();
            if j.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished).is_some() && is_finished(j).is_some() {
                me.queue.add(key(j.as_ref()));
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.inf.wait_synced().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("ttl-after-finished-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, name) = split_key(key);
        let Some(job) = self.store.get(key) else { return Ok(None) };
        let Some(ttl) = job.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished) else { return Ok(None) };
        let Some(cond) = is_finished(&job) else { return Ok(None) };
        if job.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let finished = cond.last_transition_time.as_deref().and_then(parse_time).unwrap_or_else(Utc::now);
        let expire = finished + chrono::Duration::seconds(ttl as i64);
        let now = Utc::now();
        if expire > now {
            return Ok(Some((expire - now).to_std().unwrap_or_default()));
        }
        let api: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let dp = DeleteParams { precondition_uid: job.metadata.uid.clone(), ..DeleteParams::background() };
        match api.delete_with(name, &dp).await {
            Ok(_) => tracing::info!(job = key, "cleaned up finished job (TTL expired)"),
            Err(e) if e.is_not_found() || e.is_conflict() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }
}
