//! Port of `pkg/registry/core/service` (strategy + REST storage with IP/port allocation).

pub mod ipallocator;
pub mod portallocator;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::{Strategy, StrategyContext};
use kube_apis::core::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::{format_labels, str_at};
pub use ipallocator::IpAllocator;
pub use portallocator::PortAllocator;

pub struct ServiceStrategy {
    pub ips: Arc<Mutex<IpAllocator>>,
    pub ports: Arc<Mutex<PortAllocator>>,
}

impl ServiceStrategy {
    pub fn new(ips: IpAllocator, ports: PortAllocator) -> Self {
        Self { ips: Arc::new(Mutex::new(ips)), ports: Arc::new(Mutex::new(ports)) }
    }
}

fn needs_cluster_ip(svc: &Value) -> bool {
    str_at(svc, "/spec/type") != "ExternalName" && str_at(svc, "/spec/clusterIP") != "None"
}

fn needs_node_ports(svc: &Value) -> bool {
    matches!(str_at(svc, "/spec/type"), "NodePort" | "LoadBalancer")
}

fn node_ports(svc: &Value) -> Vec<i64> {
    svc.pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ps| ps.iter().filter_map(|p| p.get("nodePort").and_then(Value::as_i64)).filter(|p| *p != 0).collect())
        .unwrap_or_default()
}

fn invalid(svc: &Value, field: &str, value: &str, msg: &str) -> ApiError {
    ApiError::invalid("", "Service", svc.name(), &[FieldError::invalid(field, value, msg)])
}

impl ServiceStrategy {
    /// Allocates (or reserves user-requested) ClusterIP and NodePorts.
    fn allocate(&self, svc: &mut Value, old: Option<&Value>) -> Result<(), ApiError> {
        let mut allocated_ip = None;
        if needs_cluster_ip(svc) && old.is_none_or(|o| str_at(o, "/spec/clusterIP").is_empty()) {
            let requested = str_at(svc, "/spec/clusterIP").to_string();
            let mut ips = self.ips.lock().unwrap();
            let ip = if requested.is_empty() {
                ips.allocate_next().map_err(|e| invalid(svc, "spec.clusterIPs", "", &e))?
            } else {
                ips.allocate(&requested).map_err(|e| invalid(svc, "spec.clusterIPs", &requested, &e))?;
                requested
            };
            allocated_ip = Some(ip.clone());
            svc["spec"]["clusterIP"] = json!(ip);
            svc["spec"]["clusterIPs"] = json!([ip]);
            svc["spec"]["ipFamilies"] = json!(["IPv4"]);
            svc["spec"]["ipFamilyPolicy"] = json!("SingleStack");
        }
        if needs_node_ports(svc) {
            let already: Vec<i64> = old.map(node_ports).unwrap_or_default();
            let mut ports = self.ports.lock().unwrap();
            let mut newly = vec![];
            let mut fail = None;
            if let Some(ps) = svc["spec"]["ports"].as_array_mut() {
                for (i, p) in ps.iter_mut().enumerate() {
                    let want = p.get("nodePort").and_then(Value::as_i64).unwrap_or(0);
                    if want != 0 && already.contains(&want) {
                        continue;
                    }
                    let res = if want == 0 { ports.allocate_next() } else { ports.allocate(want).map(|_| want) };
                    match res {
                        Ok(np) => {
                            p["nodePort"] = json!(np);
                            newly.push(np);
                        }
                        Err(e) => {
                            fail = Some((i, want, e));
                            break;
                        }
                    }
                }
            }
            if let Some((i, want, e)) = fail {
                for np in newly {
                    ports.release(np);
                }
                drop(ports);
                if let Some(ip) = allocated_ip {
                    self.ips.lock().unwrap().release(&ip);
                }
                return Err(invalid(svc, &format!("spec.ports[{i}].nodePort"), &want.to_string(), &e));
            }
        }
        Ok(())
    }

    fn release(&self, svc: &Value) {
        let ip = str_at(svc, "/spec/clusterIP");
        if !ip.is_empty() && ip != "None" {
            self.ips.lock().unwrap().release(ip);
        }
        let mut ports = self.ports.lock().unwrap();
        for np in node_ports(svc) {
            ports.release(np);
        }
    }
}

#[async_trait]
impl Strategy for ServiceStrategy {
    fn prepare_for_create(&self, obj: &mut Value) {
        defaults::set_service_defaults(obj);
        obj["status"] = json!({"loadBalancer": {}});
    }

    /// `patchAllocatedValues`: keep allocated values the client omitted.
    fn prepare_for_update(&self, new: &mut Value, old: &Value) {
        defaults::set_service_defaults(new);
        if needs_cluster_ip(new) && str_at(new, "/spec/clusterIP").is_empty() {
            for k in ["clusterIP", "clusterIPs", "ipFamilies", "ipFamilyPolicy"] {
                if let Some(v) = old.pointer(&format!("/spec/{k}")) {
                    new["spec"][k] = v.clone();
                }
            }
        }
        if needs_node_ports(new) {
            let old_ports = old.pointer("/spec/ports").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(ps) = new["spec"]["ports"].as_array_mut() {
                for p in ps {
                    if p.get("nodePort").and_then(Value::as_i64).unwrap_or(0) == 0 {
                        if let Some(op) = old_ports.iter().find(|op| op["port"] == p["port"] && op["protocol"] == p["protocol"]) {
                            if let Some(np) = op.get("nodePort") {
                                p["nodePort"] = np.clone();
                            }
                        }
                    }
                }
            }
        }
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        validation::validate_service(obj)
    }

    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        validation::validate_service_update(new, old)
    }

    async fn before_create(&self, _ctx: &StrategyContext<'_>, obj: &mut Value) -> Result<(), ApiError> {
        self.allocate(obj, None)
    }

    async fn rollback_create(&self, _ctx: &StrategyContext<'_>, obj: &Value) {
        self.release(obj);
    }

    async fn before_update(&self, _ctx: &StrategyContext<'_>, new: &mut Value, old: &Value) -> Result<(), ApiError> {
        self.allocate(new, Some(old))?;
        // Release node ports that are no longer used.
        let keep = if needs_node_ports(new) { node_ports(new) } else { vec![] };
        let mut ports = self.ports.lock().unwrap();
        for np in node_ports(old) {
            if !keep.contains(&np) {
                ports.release(np);
            }
        }
        Ok(())
    }

    async fn after_delete(&self, _ctx: &StrategyContext<'_>, obj: &Value) {
        self.release(obj);
    }

    async fn restore(&self, existing: &[Value]) {
        let mut ips = self.ips.lock().unwrap();
        let mut ports = self.ports.lock().unwrap();
        for svc in existing {
            let ip = str_at(svc, "/spec/clusterIP");
            if !ip.is_empty() && ip != "None" {
                let _ = ips.allocate(ip);
            }
            for np in node_ports(svc) {
                let _ = ports.allocate(np);
            }
        }
    }

    fn table_columns(&self) -> Vec<Column> {
        vec![
            Column::new("Type", "string", "The type of the service."),
            Column::new("Cluster-IP", "string", "The IP address of the service."),
            Column::new("External-IP", "string", "The external IP address of the service."),
            Column::new("Port(s)", "string", "The ports of the service."),
            Column::new("Age", "string", ""),
            Column::new("Selector", "string", "The label selector of the service.").wide(),
        ]
    }

    fn table_cells(&self, svc: &Value) -> Vec<Value> {
        let type_ = str_at(svc, "/spec/type");
        let ports = svc
            .pointer("/spec/ports")
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .map(|p| {
                        let base = p["port"].to_string();
                        let np = p.get("nodePort").and_then(Value::as_i64).filter(|n| *n != 0);
                        let proto = p["protocol"].as_str().unwrap_or("TCP");
                        match np {
                            Some(n) => format!("{base}:{n}/{proto}"),
                            None => format!("{base}/{proto}"),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<none>".into());
        let external = match type_ {
            "ExternalName" => str_at(svc, "/spec/externalName").to_string(),
            "LoadBalancer" => svc.pointer("/status/loadBalancer/ingress/0/ip").and_then(Value::as_str).unwrap_or("<pending>").to_string(),
            _ => svc
                .pointer("/spec/externalIPs")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "<none>".into()),
        };
        let cip = str_at(svc, "/spec/clusterIP");
        vec![
            json!(type_),
            json!(if cip.is_empty() { "<none>" } else { cip }),
            json!(external),
            json!(ports),
            age_cell(svc),
            json!(format_labels(svc.pointer("/spec/selector"))),
        ]
    }
}
