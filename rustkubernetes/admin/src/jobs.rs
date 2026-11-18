//! Long-running work (installing a machine, provisioning a cluster): each job
//! has a log that browsers follow live (`jobs` topic) and that is kept on disk
//! (`<data-dir>/jobs/<id>.log`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use crate::events::Hub;
use crate::state::now;

const KEEP_JOBS: usize = 40;
const LIVE_LINES: usize = 400;

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Job {
    pub id: String,
    /// `install`, `provision`, `start`, `stop`, `delete`, `bundle`
    pub kind: String,
    pub title: String,
    /// Machine id or cluster name.
    pub target: String,
    /// `running`, `succeeded`, `failed`
    pub state: String,
    pub user: String,
    pub started: String,
    pub finished: Option<String>,
    pub log: Vec<String>,
}

pub struct Jobs {
    list: Mutex<Vec<Job>>,
    hub: Arc<Hub>,
    dir: PathBuf,
}

impl Jobs {
    /// Loads the job history kept in `<dir>/jobs.json` (jobs that were running are marked failed).
    pub fn new(hub: Arc<Hub>, dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        let mut list: Vec<Job> =
            std::fs::read(dir.join("jobs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for j in list.iter_mut().filter(|j| j.state == "running") {
            j.state = "failed".into();
            j.log.push("✘ interrupted by a restart of rustkubeadmin".into());
        }
        Arc::new(Self { list: Mutex::new(list), hub, dir })
    }

    fn publish(&self) {
        let v: Vec<Value> = self.list.lock().unwrap().iter().map(|j| serde_json::to_value(j).unwrap_or_default()).collect();
        self.hub.publish("jobs", Value::Array(v));
    }

    fn save(&self) {
        let l = self.list.lock().unwrap().clone();
        if let Ok(b) = serde_json::to_vec(&l) {
            let tmp = self.dir.join("jobs.json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, self.dir.join("jobs.json"));
            }
        }
    }

    pub fn start(self: &Arc<Self>, kind: &str, title: &str, target: &str, user: &str) -> JobHandle {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let job = Job {
            id: id.clone(),
            kind: kind.into(),
            title: title.into(),
            target: target.into(),
            state: "running".into(),
            user: user.into(),
            started: now(),
            finished: None,
            log: vec![],
        };
        {
            let mut l = self.list.lock().unwrap();
            l.insert(0, job);
            l.truncate(KEEP_JOBS);
        }
        self.save();
        self.publish();
        JobHandle { jobs: self.clone(), id }
    }

    pub fn running_for(&self, target: &str) -> bool {
        self.list.lock().unwrap().iter().any(|j| j.target == target && j.state == "running")
    }

    pub fn list(&self) -> Value {
        json!(self.list.lock().unwrap().clone())
    }
}

/// Writes to one job.
#[derive(Clone)]
pub struct JobHandle {
    jobs: Arc<Jobs>,
    pub id: String,
}

impl JobHandle {
    pub fn log(&self, line: impl AsRef<str>) {
        let line = line.as_ref().trim_end().to_string();
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.jobs.dir.join(format!("{}.log", self.id))) {
                let _ = writeln!(f, "{line}");
            }
        }
        let mut l = self.jobs.list.lock().unwrap();
        if let Some(j) = l.iter_mut().find(|j| j.id == self.id) {
            j.log.push(line);
            if j.log.len() > LIVE_LINES {
                let cut = j.log.len() - LIVE_LINES;
                j.log.drain(..cut);
            }
        }
        drop(l);
        self.jobs.publish();
    }

    pub fn finish(&self, result: &anyhow::Result<()>) {
        match result {
            Ok(()) => self.log("✔ done"),
            Err(e) => self.log(format!("✘ failed: {e:#}")),
        }
        let mut l = self.jobs.list.lock().unwrap();
        if let Some(j) = l.iter_mut().find(|j| j.id == self.id) {
            j.state = if result.is_ok() { "succeeded" } else { "failed" }.into();
            j.finished = Some(now());
        }
        drop(l);
        self.jobs.save();
        self.jobs.publish();
    }
}
