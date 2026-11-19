//! `GET /metrics/resource` (`pkg/kubelet/metrics/collectors/resource_metrics.go`):
//! the CPU and memory metrics metrics-server scrapes, in the Prometheus text
//! format with sample timestamps.
//!
//! | Metric | Labels | Source |
//! |--------|--------|--------|
//! | `container_cpu_usage_seconds_total` | container, namespace, pod | container cgroup |
//! | `container_memory_working_set_bytes` | container, namespace, pod | usage − inactive_file |
//! | `container_start_time_seconds` | container, namespace, pod | container start |
//! | `node_cpu_usage_seconds_total`, `node_memory_working_set_bytes` | — | the node's cgroup (or `/proc`) |
//! | `pod_cpu_usage_seconds_total`, `pod_memory_working_set_bytes` | namespace, pod | sum of its containers |
//! | `resource_scrape_error` | — | 1 if container stats failed |

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::runtime::{labels, ContainerState, ContainerStats};
use crate::KubeletCtx;

/// Node CPU and memory (`cadvisor` root container).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeStats {
    pub cpu_usage_ns: u64,
    pub memory_working_set_bytes: u64,
}

fn read(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(char::is_whitespace)?;
        (k.trim_end_matches(':') == key).then(|| v.split_whitespace().next()?.parse().ok())?
    })
}

/// The node's CPU and memory. A cgroup v2 hierarchy (`/sys/fs/cgroup`) is used when it
/// has the counters (a node running in a container sees its own cgroup there);
/// otherwise `/proc/stat` and `/proc/meminfo`.
pub fn node_stats() -> NodeStats {
    let cpu_usage_ns = read("/sys/fs/cgroup/cpu.stat")
        .and_then(|t| kv(&t, "usage_usec"))
        .map(|us| us * 1000)
        .or_else(|| {
            // user nice system idle iowait irq softirq steal (USER_HZ = 100)
            let stat = read("/proc/stat")?;
            let f: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let busy = f.first()? + f.get(1)? + f.get(2)? + f.get(5).unwrap_or(&0) + f.get(6).unwrap_or(&0) + f.get(7).unwrap_or(&0);
            Some(busy * 10_000_000)
        })
        .unwrap_or(0);
    let memory_working_set_bytes = match read("/sys/fs/cgroup/memory.current").and_then(|c| c.trim().parse::<u64>().ok()) {
        Some(usage) => {
            let inactive = read("/sys/fs/cgroup/memory.stat").and_then(|t| kv(&t, "inactive_file")).unwrap_or(0);
            usage.saturating_sub(inactive)
        }
        None => {
            let m = read("/proc/meminfo").unwrap_or_default();
            let kb = |k: &str| kv(&m, k).unwrap_or(0) * 1024;
            kb("MemTotal").saturating_sub(kb("MemFree")).saturating_sub(kb("Inactive(file)"))
        }
    };
    NodeStats { cpu_usage_ns, memory_working_set_bytes }
}

/// One running container's sample.
#[derive(Clone, Debug)]
pub struct ContainerSample {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    /// Start time, seconds since the epoch.
    pub start_time: f64,
    pub stats: ContainerStats,
}

/// Go's `strconv.FormatFloat(v, 'g', -1, 64)`, as the Prometheus text format
/// writes values: shortest digits, exponent form when the exponent is < -4 or >= 6
/// (`1.048576e+06`), plain decimal otherwise.
pub fn float(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    if !v.is_finite() {
        return if v.is_nan() {
            "NaN".into()
        } else if v > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    let e = format!("{v:e}"); // shortest digits, e.g. "1.048576e6"
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if !(-4..6).contains(&exp) {
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        format!("{v}")
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn family(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} [STABLE] {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Renders the resource metrics (families sorted by name, as the Prometheus registry does).
pub fn render(node: &NodeStats, node_ts_ms: i64, containers: &[ContainerSample], scrape_error: bool) -> String {
    let mut out = String::new();
    let mut cs: Vec<&ContainerSample> = containers.iter().collect();
    cs.sort_by(|a, b| (&a.container, &a.namespace, &a.pod).cmp(&(&b.container, &b.namespace, &b.pod)));
    let lbl =
        |c: &ContainerSample| format!("container=\"{}\",namespace=\"{}\",pod=\"{}\"", esc(&c.container), esc(&c.namespace), esc(&c.pod));
    if !cs.is_empty() {
        family(&mut out, "container_cpu_usage_seconds_total", "Cumulative cpu time consumed by the container in core-seconds", "counter");
        for c in &cs {
            let _ = writeln!(
                out,
                "container_cpu_usage_seconds_total{{{}}} {} {}",
                lbl(c),
                float(c.stats.cpu_usage_ns as f64 / 1e9),
                c.stats.timestamp_ms
            );
        }
        family(
            &mut out,
            "container_memory_working_set_bytes",
            "Current memory usage of the container in bytes: committed memory on Windows, working set on other platforms",
            "gauge",
        );
        for c in &cs {
            let _ = writeln!(
                out,
                "container_memory_working_set_bytes{{{}}} {} {}",
                lbl(c),
                float(c.stats.memory_working_set_bytes as f64),
                c.stats.timestamp_ms
            );
        }
        family(&mut out, "container_start_time_seconds", "Start time of the container since unix epoch in seconds", "gauge");
        for c in &cs {
            let _ = writeln!(out, "container_start_time_seconds{{{}}} {} {}", lbl(c), float(c.start_time), (c.start_time * 1000.0) as i64);
        }
    }
    family(&mut out, "node_cpu_usage_seconds_total", "Cumulative cpu time consumed by the node in core-seconds", "counter");
    let _ = writeln!(out, "node_cpu_usage_seconds_total {} {node_ts_ms}", float(node.cpu_usage_ns as f64 / 1e9));
    family(
        &mut out,
        "node_memory_working_set_bytes",
        "Current memory usage of the node in bytes: committed memory on Windows, working set on other platforms",
        "gauge",
    );
    let _ = writeln!(out, "node_memory_working_set_bytes {} {node_ts_ms}", float(node.memory_working_set_bytes as f64));
    // Pods: the sum of their containers, at the newest container sample time.
    let mut pods: BTreeMap<(String, String), (u64, u64, i64)> = BTreeMap::new();
    for c in &cs {
        let e = pods.entry((c.namespace.clone(), c.pod.clone())).or_default();
        e.0 += c.stats.cpu_usage_ns;
        e.1 += c.stats.memory_working_set_bytes;
        e.2 = e.2.max(c.stats.timestamp_ms);
    }
    if !pods.is_empty() {
        family(&mut out, "pod_cpu_usage_seconds_total", "Cumulative cpu time consumed by the pod in core-seconds", "counter");
        for ((ns, pod), (cpu, _, ts)) in &pods {
            let _ = writeln!(
                out,
                "pod_cpu_usage_seconds_total{{namespace=\"{}\",pod=\"{}\"}} {} {ts}",
                esc(ns),
                esc(pod),
                float(*cpu as f64 / 1e9)
            );
        }
        family(
            &mut out,
            "pod_memory_working_set_bytes",
            "Current memory usage of the pod in bytes: committed memory on Windows, working set on other platforms",
            "gauge",
        );
        for ((ns, pod), (_, mem, ts)) in &pods {
            let _ =
                writeln!(out, "pod_memory_working_set_bytes{{namespace=\"{}\",pod=\"{}\"}} {} {ts}", esc(ns), esc(pod), float(*mem as f64));
        }
    }
    family(&mut out, "resource_scrape_error", "1 if there was an error while getting container metrics, 0 otherwise", "gauge");
    let _ = writeln!(out, "resource_scrape_error {}", if scrape_error { 1 } else { 0 });
    out
}

/// Samples every running container of this node's pods and renders the metrics.
pub async fn collect(ctx: &KubeletCtx) -> String {
    let (_, containers) = ctx.cache.all();
    let running: Vec<_> = containers.into_iter().filter(|c| c.state == ContainerState::Running).collect();
    let rt = ctx.runtime.clone();
    let stats = futures::future::join_all(running.iter().map(|c| {
        let rt = rt.clone();
        let id = c.id.clone();
        async move { rt.container_stats(&id).await }
    }))
    .await;
    let mut error = false;
    let mut samples = vec![];
    for (c, s) in running.iter().zip(stats) {
        match s {
            Ok(stats) => samples.push(ContainerSample {
                namespace: c.labels.get(labels::POD_NAMESPACE).cloned().unwrap_or_default(),
                pod: c.labels.get(labels::POD_NAME).cloned().unwrap_or_default(),
                container: c.name.clone(),
                start_time: c
                    .started_at
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.timestamp_millis() as f64 / 1000.0)
                    .unwrap_or(0.0),
                stats,
            }),
            Err(e) => {
                tracing::debug!(container = %c.id, "container stats: {e:#}");
                error = true;
            }
        }
    }
    render(&node_stats(), chrono::Utc::now().timestamp_millis(), &samples, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition() {
        let c = ContainerSample {
            namespace: "default".into(),
            pod: "web".into(),
            container: "nginx".into(),
            start_time: 1_700_000_000.5,
            stats: ContainerStats { timestamp_ms: 1_700_000_100_000, cpu_usage_ns: 1_500_000_000, memory_working_set_bytes: 4096 },
        };
        let text = render(&NodeStats { cpu_usage_ns: 2_000_000_000, memory_working_set_bytes: 1 << 20 }, 1_700_000_100_000, &[c], false);
        assert!(text.contains("# HELP container_cpu_usage_seconds_total [STABLE] Cumulative cpu time consumed by the container in core-seconds\n# TYPE container_cpu_usage_seconds_total counter\n"));
        assert!(
            text.contains("container_cpu_usage_seconds_total{container=\"nginx\",namespace=\"default\",pod=\"web\"} 1.5 1700000100000\n")
        );
        assert!(
            text.contains("container_memory_working_set_bytes{container=\"nginx\",namespace=\"default\",pod=\"web\"} 4096 1700000100000\n")
        );
        assert!(text.contains("node_cpu_usage_seconds_total 2 1700000100000\n"));
        assert!(text.contains("node_memory_working_set_bytes 1.048576e+06 1700000100000\n"));
        assert!(text.contains("pod_cpu_usage_seconds_total{namespace=\"default\",pod=\"web\"} 1.5 1700000100000\n"));
        assert!(text.ends_with("resource_scrape_error 0\n"));
        let names: Vec<&str> = text.lines().filter(|l| l.starts_with("# TYPE")).map(|l| l.split(' ').nth(2).unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn go_float_format() {
        assert_eq!(float(4096.0), "4096");
        assert_eq!(float(999999.0), "999999");
        assert_eq!(float(1_000_000.0), "1e+06");
        assert_eq!(float(1_700_000_000.5), "1.7000000005e+09");
        assert_eq!(float(0.00012), "0.00012");
        assert_eq!(float(0.000012), "1.2e-05");
        assert_eq!(float(1.5), "1.5");
    }

    #[test]
    fn node_stats_are_read() {
        let n = node_stats();
        assert!(n.cpu_usage_ns > 0 && n.memory_working_set_bytes > 0);
    }
}
