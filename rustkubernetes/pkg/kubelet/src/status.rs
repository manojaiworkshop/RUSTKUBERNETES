//! Pod status generation (`kubelet_pods.go: generateAPIPodStatus`,
//! `getPhase`, `status/generate.go`).

use std::collections::HashMap;

use k8s_api::core::v1::{
    Container, ContainerState as KState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus as KStatus, Pod, PodCondition, PodIp, PodStatus,
};
use k8s_apimachinery::meta::now_rfc3339;

use crate::runtime::{ContainerState, ContainerStatus, SandboxState, SandboxStatus};

/// Per-container information the worker tracks besides runtime state.
#[derive(Clone, Debug, Default)]
pub struct ContainerExtra {
    /// Waiting reason/message to report (ErrImagePull, CrashLoopBackOff, ...).
    pub waiting: Option<(String, String)>,
    pub ready: bool,
    pub started: bool,
}

/// Runtime containers of one name, newest attempt first.
pub fn attempts<'a>(all: &'a [ContainerStatus], name: &str) -> Vec<&'a ContainerStatus> {
    let mut v: Vec<&ContainerStatus> = all.iter().filter(|c| c.name == name).collect();
    v.sort_by(|a, b| b.attempt.cmp(&a.attempt).then(b.created_at.cmp(&a.created_at)));
    v
}

fn terminated(c: &ContainerStatus) -> KState {
    KState {
        terminated: Some(ContainerStateTerminated {
            exit_code: c.exit_code,
            reason: c.reason.clone(),
            message: c.message.clone(),
            started_at: c.started_at.clone(),
            finished_at: c.finished_at.clone(),
            container_id: Some(format!("docker://{}", c.id)),
            signal: None,
        }),
        ..Default::default()
    }
}

fn container_status(spec: &Container, all: &[ContainerStatus], extra: &ContainerExtra, default_wait: &str) -> KStatus {
    let att = attempts(all, &spec.name);
    let latest = att.first().copied();
    let state = match latest {
        Some(c) if c.state == ContainerState::Running => {
            KState { running: Some(ContainerStateRunning { started_at: c.started_at.clone() }), ..Default::default() }
        }
        _ if extra.waiting.is_some() => {
            let (r, m) = extra.waiting.clone().unwrap();
            KState {
                waiting: Some(ContainerStateWaiting { reason: Some(r), message: Some(m).filter(|m| !m.is_empty()) }),
                ..Default::default()
            }
        }
        Some(c) if c.state == ContainerState::Exited => terminated(c),
        _ => KState { waiting: Some(ContainerStateWaiting { reason: Some(default_wait.into()), message: None }), ..Default::default() },
    };
    let last_state = att
        .iter()
        .find(|c| Some(c.id.as_str()) != latest.map(|l| l.id.as_str()) && c.state == ContainerState::Exited)
        .map(|c| terminated(c));
    // While waiting after a crash, report the crashed attempt as the last state.
    let last_state = match (&state.waiting, latest) {
        (Some(_), Some(c)) if c.state == ContainerState::Exited => Some(terminated(c)),
        _ => last_state,
    };
    let running = latest.is_some_and(|c| c.state == ContainerState::Running);
    KStatus {
        name: spec.name.clone(),
        state: Some(state),
        last_state: last_state.or_else(|| Some(KState::default())),
        ready: running && extra.ready,
        restart_count: latest.map(|c| c.attempt as i32).unwrap_or(0),
        image: spec.image.clone(),
        image_id: latest.map(|c| c.image_ref.clone()).filter(|s| !s.is_empty()),
        container_id: latest.map(|c| format!("docker://{}", c.id)),
        started: Some(running && extra.started),
    }
}

fn condition(old: Option<&PodStatus>, type_: &str, ok: bool, reason: Option<&str>) -> PodCondition {
    let status = if ok { "True" } else { "False" };
    let prev = old.and_then(|s| s.conditions.as_ref()).and_then(|cs| cs.iter().find(|c| c.type_ == type_));
    let ltt = match prev {
        Some(p) if p.status == status => p.last_transition_time.clone(),
        _ => Some(now_rfc3339()),
    };
    PodCondition {
        type_: type_.into(),
        status: status.into(),
        last_probe_time: None,
        last_transition_time: ltt,
        reason: if ok { None } else { reason.map(String::from) },
        message: None,
    }
}

/// Builds the API pod status from runtime state.
pub fn generate(
    pod: &Pod,
    sandbox: Option<&SandboxStatus>,
    containers: &[ContainerStatus],
    extras: &HashMap<String, ContainerExtra>,
    node_ip: &str,
) -> PodStatus {
    let spec = pod.spec.clone().unwrap_or_default();
    let old = pod.status.as_ref();
    let empty = ContainerExtra::default();
    let restart = spec.restart_policy.clone().unwrap_or_else(|| "Always".into());

    let inits: Vec<KStatus> = spec
        .init_containers
        .iter()
        .flatten()
        .map(|c| container_status(c, containers, extras.get(&c.name).unwrap_or(&empty), "PodInitializing"))
        .collect();
    let init_done = inits.iter().all(|s| s.state.as_ref().and_then(|st| st.terminated.as_ref()).is_some_and(|t| t.exit_code == 0));
    let init_failed = inits.iter().any(|s| s.state.as_ref().and_then(|st| st.terminated.as_ref()).is_some_and(|t| t.exit_code != 0));
    let wait_reason = if init_done { "ContainerCreating" } else { "PodInitializing" };
    let apps: Vec<KStatus> =
        spec.containers.iter().map(|c| container_status(c, containers, extras.get(&c.name).unwrap_or(&empty), wait_reason)).collect();

    // getPhase
    let term = |s: &KStatus| s.state.as_ref().and_then(|st| st.terminated.clone());
    let running = apps.iter().filter(|s| s.state.as_ref().is_some_and(|st| st.running.is_some())).count();
    let all_terminated = apps.iter().all(|s| term(s).is_some());
    let all_succeeded = apps.iter().all(|s| term(s).is_some_and(|t| t.exit_code == 0));
    let phase = if init_failed && restart == "Never" {
        "Failed"
    } else if !init_done {
        "Pending"
    } else if all_terminated && !apps.is_empty() {
        match restart.as_str() {
            "Never" if all_succeeded => "Succeeded",
            "Never" => "Failed",
            "OnFailure" if all_succeeded => "Succeeded",
            _ => "Running",
        }
    } else if running > 0 || apps.iter().any(|s| term(s).is_some()) {
        "Running"
    } else {
        "Pending"
    };

    let sandbox_ready = sandbox.is_some_and(|s| s.state == SandboxState::Ready);
    let all_ready = !apps.is_empty() && apps.iter().all(|s| s.ready);
    let terminal = phase == "Succeeded" || phase == "Failed";
    let host_network = spec.host_network.unwrap_or(false);
    let pod_ip = if host_network { Some(node_ip.to_string()) } else { sandbox.and_then(|s| s.ip.clone()) };
    let pod_ip = pod_ip.or_else(|| old.and_then(|o| o.pod_ip.clone()));

    PodStatus {
        phase: Some(phase.into()),
        conditions: Some(vec![
            condition(old, "PodReadyToStartContainers", sandbox_ready || terminal, None),
            condition(old, "Initialized", init_done, Some("ContainersNotInitialized")),
            condition(old, "Ready", all_ready, Some(if terminal { "PodCompleted" } else { "ContainersNotReady" })),
            condition(old, "ContainersReady", all_ready, Some(if terminal { "PodCompleted" } else { "ContainersNotReady" })),
            condition(old, "PodScheduled", true, None),
        ]),
        message: None,
        reason: None,
        nominated_node_name: old.and_then(|o| o.nominated_node_name.clone()),
        host_ip: Some(node_ip.to_string()),
        pod_ip: pod_ip.clone(),
        pod_ips: pod_ip.map(|ip| vec![PodIp { ip }]),
        start_time: old.and_then(|o| o.start_time.clone()).or_else(|| Some(now_rfc3339())),
        init_container_statuses: if inits.is_empty() { None } else { Some(inits) },
        container_statuses: Some(apps),
        qos_class: old.and_then(|o| o.qos_class.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn rc(name: &str, attempt: u32, state: ContainerState, code: i32) -> ContainerStatus {
        ContainerStatus {
            id: format!("{name}-{attempt}"),
            pod_uid: "u".into(),
            name: name.into(),
            attempt,
            state,
            created_at: format!("2026-01-01T00:00:0{attempt}Z"),
            started_at: Some("2026-01-01T00:00:00Z".into()),
            finished_at: None,
            exit_code: code,
            reason: (state == ContainerState::Exited).then(|| if code == 0 { "Completed".into() } else { "Error".into() }),
            message: None,
            image: "x".into(),
            image_ref: "sha256:1".into(),
            labels: BTreeMap::new(),
        }
    }

    fn pod(restart: &str) -> Pod {
        serde_json::from_value(
            json!({"metadata": {"name": "p"}, "spec": {"restartPolicy": restart, "containers": [{"name": "a", "image": "x"}]}}),
        )
        .unwrap()
    }

    #[test]
    fn phases() {
        let sb = SandboxStatus {
            id: "s".into(),
            pod_uid: "u".into(),
            attempt: 0,
            state: SandboxState::Ready,
            ip: Some("172.17.0.5".into()),
            created_at: String::new(),
            labels: BTreeMap::new(),
        };
        let ready: HashMap<String, ContainerExtra> =
            [("a".to_string(), ContainerExtra { ready: true, started: true, waiting: None })].into_iter().collect();
        let st = generate(&pod("Always"), Some(&sb), &[rc("a", 0, ContainerState::Running, 0)], &ready, "10.0.0.1");
        assert_eq!(st.phase.as_deref(), Some("Running"));
        assert_eq!(st.pod_ip.as_deref(), Some("172.17.0.5"));
        assert!(st.conditions.unwrap().iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        let st = generate(&pod("Never"), Some(&sb), &[rc("a", 0, ContainerState::Exited, 0)], &HashMap::new(), "10.0.0.1");
        assert_eq!(st.phase.as_deref(), Some("Succeeded"));
        let st = generate(&pod("Never"), Some(&sb), &[rc("a", 0, ContainerState::Exited, 2)], &HashMap::new(), "10.0.0.1");
        assert_eq!(st.phase.as_deref(), Some("Failed"));
        // Crash looping: restartCount from attempts, waiting reason reported.
        let crash: HashMap<String, ContainerExtra> =
            [("a".to_string(), ContainerExtra { waiting: Some(("CrashLoopBackOff".into(), "back-off".into())), ..Default::default() })]
                .into_iter()
                .collect();
        let st = generate(
            &pod("Always"),
            Some(&sb),
            &[rc("a", 2, ContainerState::Exited, 1), rc("a", 1, ContainerState::Exited, 1)],
            &crash,
            "10.0.0.1",
        );
        let cs = &st.container_statuses.unwrap()[0];
        assert_eq!(cs.restart_count, 2);
        assert_eq!(cs.state.as_ref().unwrap().waiting.as_ref().unwrap().reason.as_deref(), Some("CrashLoopBackOff"));
        assert_eq!(cs.last_state.as_ref().unwrap().terminated.as_ref().unwrap().exit_code, 1);
    }
}
