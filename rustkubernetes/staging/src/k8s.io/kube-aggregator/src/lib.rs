//! Port of `k8s.io/kube-aggregator`: the aggregation layer in front of
//! kube-apiserver.
//!
//! | Piece | Upstream |
//! |-------|----------|
//! | [`priorities`]: built-in `APIService`s created at start (`automanaged: onstart`) | `cmd/kube-apiserver/app/aggregator.go`, autoregister controller |
//! | availability: `Local`, or Service → EndpointSlices → discovery check | `controllers/status/{local,remote}` |
//! | `/apis` and `/apis/<group>` for extension groups | `apiserver/handler_apis.go` |
//! | proxy to the Service with the front-proxy client certificate and `X-Remote-*` headers | `apiserver/handler_proxy.go`, `transport.NewAuthProxyRoundTripper` |
//! | Service → address (`--enable-aggregator-routing`: an endpoint, otherwise the ClusterIP) | `apiserver/resolvers.go` |

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue};
use axum::response::Response;
use base64::Engine;
use futures::StreamExt;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::watch::EventType;
use k8s_apiserver::aggregator::{AggregatedRequest, Aggregator};
use k8s_apiserver::ApiServer;
use serde_json::{json, Value};

const GROUP: &str = "apiregistration.k8s.io";
const PREFIX: &str = "/registry/apiregistration.k8s.io/apiservices/";
/// Label on APIServices the server creates for its own groups.
pub const AUTOMANAGED_LABEL: &str = "kube-aggregator.kubernetes.io/automanaged";
const AVAILABILITY_PERIOD: Duration = Duration::from_secs(10);

/// `apiVersionPriorities` (`DefaultGenericAPIServicePriorities` + kube-apiserver's own):
/// (group, version) → (groupPriorityMinimum, versionPriority).
pub fn priorities(group: &str, version: &str) -> Option<(i64, i64)> {
    Some(match (group, version) {
        ("", "v1") => (18000, 1),
        ("apps", "v1") => (17800, 15),
        ("events.k8s.io", "v1") => (17750, 15),
        ("events.k8s.io", "v1beta1") => (17750, 5),
        ("authentication.k8s.io", "v1") => (17700, 15),
        ("authentication.k8s.io", "v1beta1") => (17700, 9),
        ("authorization.k8s.io", "v1") => (17600, 15),
        ("autoscaling", "v1") => (17500, 15),
        ("autoscaling", "v2") => (17500, 30),
        ("batch", "v1") => (17400, 15),
        ("certificates.k8s.io", "v1") => (17300, 15),
        ("networking.k8s.io", "v1") => (17200, 15),
        ("policy", "v1") => (17100, 15),
        ("rbac.authorization.k8s.io", "v1") => (17000, 15),
        ("storage.k8s.io", "v1") => (16800, 15),
        ("apiextensions.k8s.io", "v1") => (16700, 15),
        ("admissionregistration.k8s.io", "v1") => (16700, 15),
        ("scheduling.k8s.io", "v1") => (16600, 15),
        ("coordination.k8s.io", "v1") => (16500, 15),
        ("node.k8s.io", "v1") => (16300, 15),
        ("discovery.k8s.io", "v1") => (16200, 15),
        ("resource.k8s.io", "v1") => (16200, 21),
        ("flowcontrol.apiserver.k8s.io", "v1") => (16100, 21),
        _ => return None,
    })
}

/// Aggregator settings (`--proxy-client-*`, `--enable-aggregator-routing`).
#[derive(Clone, Default)]
pub struct Config {
    /// PEM cert + key the API server presents to extension API servers (front-proxy client).
    pub proxy_client_cert: Option<Vec<u8>>,
    pub proxy_client_key: Option<Vec<u8>>,
    /// Route to an endpoint IP instead of the Service ClusterIP.
    pub enable_aggregator_routing: bool,
}

pub struct KubeAggregator {
    server: Arc<ApiServer>,
    cfg: Config,
    /// APIServices by name, kept current from storage.
    cache: RwLock<BTreeMap<String, Value>>,
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or("")
}

fn has_service(a: &Value) -> bool {
    a.pointer("/spec/service").is_some_and(Value::is_object)
}

fn available(a: &Value) -> bool {
    a.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|cs| cs.iter().any(|c| c["type"] == "Available" && c["status"] == "True"))
}

/// Kube-aware version ordering used for equal priorities (`version.CompareKubeAwareVersionStrings`):
/// GA > beta > alpha, higher numbers first.
fn version_rank(v: &str) -> (u8, u32, u32) {
    let digits = |x: &str| x.parse::<u32>().unwrap_or(0);
    let Some(rest) = v.strip_prefix('v') else { return (0, 0, 0) };
    let major_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let major = digits(&rest[..major_end]);
    let tail = &rest[major_end..];
    if tail.is_empty() {
        (3, major, 0)
    } else if let Some(b) = tail.strip_prefix("beta") {
        (2, major, digits(b))
    } else if let Some(a) = tail.strip_prefix("alpha") {
        (1, major, digits(a))
    } else {
        (0, major, 0)
    }
}

/// `SortedByGroupAndVersion`: groups by descending `groupPriorityMinimum` (then name),
/// versions by descending `versionPriority` (then kube-aware version order).
fn sorted_groups(services: &[Value]) -> Vec<(String, Vec<Value>)> {
    let mut by_group: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for a in services {
        by_group.entry(s(a, "/spec/group").to_string()).or_default().push(a.clone());
    }
    let prio = |a: &Value, p: &str| a.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let mut groups: Vec<(String, Vec<Value>)> = by_group.into_iter().collect();
    for (_, vs) in groups.iter_mut() {
        vs.sort_by(|a, b| {
            prio(b, "/spec/versionPriority")
                .cmp(&prio(a, "/spec/versionPriority"))
                .then_with(|| version_rank(s(b, "/spec/version")).cmp(&version_rank(s(a, "/spec/version"))))
        });
    }
    groups.sort_by(|(ga, a), (gb, b)| {
        let pa = a.iter().map(|x| prio(x, "/spec/groupPriorityMinimum")).max().unwrap_or(0);
        let pb = b.iter().map(|x| prio(x, "/spec/groupPriorityMinimum")).max().unwrap_or(0);
        pb.cmp(&pa).then_with(|| ga.cmp(gb))
    });
    groups
}

/// `convertToDiscoveryAPIGroup`
fn discovery_group(group: &str, versions: &[Value]) -> Value {
    let gv = |a: &Value| json!({"groupVersion": format!("{group}/{}", s(a, "/spec/version")), "version": s(a, "/spec/version")});
    json!({"name": group, "versions": versions.iter().map(gv).collect::<Vec<_>>(), "preferredVersion": gv(&versions[0])})
}

/// The condition the availability controllers compute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    pub status: &'static str,
    pub reason: String,
    pub message: String,
}

impl Availability {
    fn new(status: &'static str, reason: &str, message: String) -> Self {
        Self { status, reason: reason.into(), message }
    }
}

impl KubeAggregator {
    /// Registers the built-in APIServices, follows APIService changes, runs the
    /// availability controllers and puts itself in front of `server`.
    pub async fn start(server: Arc<ApiServer>, cfg: Config) -> Arc<Self> {
        let me = Arc::new(Self { server: server.clone(), cfg, cache: RwLock::new(BTreeMap::new()) });
        me.autoregister().await;
        let rev = me.relist().await;
        let w = me.clone();
        tokio::spawn(async move { w.watch_loop(rev).await });
        let c = me.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(AVAILABILITY_PERIOD);
            loop {
                t.tick().await;
                c.check_all().await;
            }
        });
        server.set_aggregator(me.clone());
        me
    }

    /// Autoregister controller: one APIService per local group/version with a known priority.
    async fn autoregister(&self) {
        for (group, versions) in self.server.registry.groups() {
            for v in versions {
                let Some((gp, vp)) = priorities(&group, &v) else { continue };
                let name = format!("{v}.{group}");
                let info = match self.server.registry.lookup(GROUP, "v1", "apiservices") {
                    Some(i) => i,
                    None => return,
                };
                if self.server.storage.get(&info.key("", &name)).await.is_ok() {
                    continue;
                }
                let obj = json!({
                    "apiVersion": "apiregistration.k8s.io/v1", "kind": "APIService",
                    "metadata": {"name": name, "labels": {AUTOMANAGED_LABEL: "onstart"}},
                    "spec": {"group": group, "version": v, "groupPriorityMinimum": gp, "versionPriority": vp},
                });
                if let Err(e) = self.server.create_internal(GROUP, "v1", "apiservices", "", obj).await {
                    if !e.is_already_exists() {
                        tracing::warn!("autoregister {name}: {}", e.message);
                    }
                }
            }
        }
    }

    async fn relist(&self) -> u64 {
        match self.server.storage.list(PREFIX).await {
            Ok((items, rev)) => {
                let mut c = self.cache.write().unwrap();
                c.clear();
                for a in items {
                    c.insert(s(&a, "/metadata/name").to_string(), a);
                }
                rev
            }
            Err(e) => {
                tracing::warn!("aggregator: listing APIServices: {e}");
                0
            }
        }
    }

    async fn watch_loop(self: Arc<Self>, mut rev: u64) {
        loop {
            match self.server.storage.watch(PREFIX, rev).await {
                Ok(mut w) => {
                    while let Some(Ok(ev)) = w.next().await {
                        rev = ev.revision;
                        let name = s(&ev.object, "/metadata/name").to_string();
                        let mut c = self.cache.write().unwrap();
                        if ev.type_ == EventType::Deleted {
                            c.remove(&name);
                        } else {
                            c.insert(name, ev.object);
                        }
                    }
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    rev = self.relist().await;
                }
            }
        }
    }

    fn services(&self) -> Vec<Value> {
        self.cache.read().unwrap().values().cloned().collect()
    }

    async fn get(&self, group: &str, plural: &str, ns: &str, name: &str) -> Option<Value> {
        let info = self.server.registry.lookup(group, "v1", plural)?;
        self.server.storage.get(&info.key(ns, name)).await.ok()
    }

    async fn endpoint_slices(&self, ns: &str, service: &str) -> Vec<Value> {
        let Some(info) = self.server.registry.lookup("discovery.k8s.io", "v1", "endpointslices") else { return vec![] };
        let prefix = info.prefix(ns);
        self.server
            .storage
            .list(&prefix)
            .await
            .map(|(items, _)| items.into_iter().filter(|e| s(e, "/metadata/labels/kubernetes.io~1service-name") == service).collect())
            .unwrap_or_default()
    }

    /// `ResolveEndpoint` / `ResolveCluster`: where to send requests for an APIService.
    async fn resolve(&self, a: &Value) -> Result<SocketAddr, String> {
        let ns = s(a, "/spec/service/namespace");
        let name = s(a, "/spec/service/name");
        let port = a.pointer("/spec/service/port").and_then(Value::as_i64).unwrap_or(443);
        let svc = self.get("", "services", ns, name).await.ok_or_else(|| format!("service \"{name}\" not found"))?;
        let svc_port = svc
            .pointer("/spec/ports")
            .and_then(Value::as_array)
            .and_then(|ps| ps.iter().find(|p| p["port"].as_i64() == Some(port)))
            .ok_or_else(|| format!("no service port {port} found for service \"{name}\""))?;
        if !self.cfg.enable_aggregator_routing {
            let ip = s(&svc, "/spec/clusterIP");
            if ip.is_empty() || ip == "None" {
                return Err(format!("invalid service \"{name}\" with no cluster IP"));
            }
            return format!("{}:{port}", if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() })
                .parse()
                .map_err(|e| format!("{e}"));
        }
        let port_name = svc_port["name"].as_str().unwrap_or("");
        for slice in self.endpoint_slices(ns, name).await {
            let Some(p) = slice
                .pointer("/ports")
                .and_then(Value::as_array)
                .and_then(|ps| ps.iter().find(|p| p["name"].as_str().unwrap_or("") == port_name))
            else {
                continue;
            };
            let Some(target) = p["port"].as_i64() else { continue };
            for ep in slice.pointer("/endpoints").and_then(Value::as_array).into_iter().flatten() {
                if ep.pointer("/conditions/ready").and_then(Value::as_bool) == Some(false) {
                    continue;
                }
                if let Some(ip) = ep.pointer("/addresses/0").and_then(Value::as_str) {
                    let host = if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() };
                    return format!("{host}:{target}").parse().map_err(|e| format!("{e}"));
                }
            }
        }
        Err(format!("no endpoints available for service \"{name}\""))
    }

    /// The client that talks to an extension API server: front-proxy client certificate,
    /// verified with `caBundle` against `<service>.<namespace>.svc` unless
    /// `insecureSkipTLSVerify`.
    fn client(&self, a: &Value, addr: SocketAddr, timeout: Option<Duration>) -> Result<(reqwest::Client, String), String> {
        k8s_client_go::util::cert::install_crypto_provider();
        let ca = match s(a, "/spec/caBundle") {
            "" => None,
            b => Some(base64::engine::general_purpose::STANDARD.decode(b).map_err(|e| format!("invalid caBundle: {e}"))?),
        };
        let insecure = a.pointer("/spec/insecureSkipTLSVerify").and_then(Value::as_bool) == Some(true);
        let tls = k8s_client_go::util::cert::TlsClientConfig {
            ca_data: ca,
            cert_data: self.cfg.proxy_client_cert.clone(),
            key_data: self.cfg.proxy_client_key.clone(),
            insecure,
            cert_resolver: None,
        }
        .rustls()?;
        let host = format!("{}.{}.svc", s(a, "/spec/service/name"), s(a, "/spec/service/namespace"));
        let mut b =
            reqwest::Client::builder().tls_backend_preconfigured(tls).resolve(&host, addr).redirect(reqwest::redirect::Policy::none());
        if let Some(t) = timeout {
            b = b.timeout(t);
        }
        Ok((b.build().map_err(|e| e.to_string())?, format!("https://{host}:{}", addr.port())))
    }

    /// `AvailableConditionController.sync` (remote) and the local controller.
    pub async fn availability(&self, a: &Value) -> Availability {
        if !has_service(a) {
            return Availability::new("True", "Local", "Local APIServices are always available".into());
        }
        let ns = s(a, "/spec/service/namespace").to_string();
        let name = s(a, "/spec/service/name").to_string();
        let port = a.pointer("/spec/service/port").and_then(Value::as_i64).unwrap_or(443);
        let Some(svc) = self.get("", "services", &ns, &name).await else {
            return Availability::new("False", "ServiceNotFound", format!("service/{name} in {ns:?} is not present"));
        };
        let ty = svc.pointer("/spec/type").and_then(Value::as_str).unwrap_or("ClusterIP");
        if ty == "ClusterIP" {
            let ports = svc.pointer("/spec/ports").and_then(Value::as_array).cloned().unwrap_or_default();
            let Some(p) = ports.iter().find(|p| p["port"].as_i64() == Some(port)) else {
                return Availability::new("False", "ServicePortError", format!("service/{name} in {ns:?} is not listening on port {port}"));
            };
            let port_name = p["name"].as_str().unwrap_or("").to_string();
            let slices = self.endpoint_slices(&ns, &name).await;
            if slices.is_empty() {
                return Availability::new("False", "EndpointsNotFound", format!("cannot find endpointslices for service/{name} in {ns:?}"));
            }
            let has_ready = slices.iter().any(|sl| {
                sl.pointer("/ports")
                    .and_then(Value::as_array)
                    .is_some_and(|ps| ps.iter().any(|p| p["name"].as_str().unwrap_or("") == port_name))
                    && sl
                        .pointer("/endpoints")
                        .and_then(Value::as_array)
                        .is_some_and(|eps| eps.iter().any(|e| e.pointer("/conditions/ready").and_then(Value::as_bool) != Some(false)))
            });
            if !has_ready {
                return Availability::new(
                    "False",
                    "MissingEndpoints",
                    format!("endpointslices for service/{name} in {ns:?} have no addresses with port name {port_name:?}"),
                );
            }
        }
        // Discovery check: GET /apis/<group>/<version> as system:kube-aggregator (5 tries in parallel).
        let path = if s(a, "/metadata/name") == "v1." {
            format!("/api/{}", s(a, "/spec/version"))
        } else {
            format!("/apis/{}/{}", s(a, "/spec/group"), s(a, "/spec/version"))
        };
        let tries = (0..5).map(|_| async {
            let addr = self.resolve(a).await?;
            let shown = format!("https://{addr}{path}");
            let (client, base) = self.client(a, addr, Some(Duration::from_secs(5)))?;
            let req = client
                .get(format!("{base}{path}"))
                .header("X-Remote-User", "system:kube-aggregator")
                .header("X-Remote-Group", "system:masters");
            match req.send().await {
                Ok(r) if r.status().is_success() => Ok(()),
                Ok(r) => Err(format!("failing or missing response from {shown}: bad status from {shown}: {}", r.status().as_u16())),
                Err(e) => Err(format!("failing or missing response from {shown}: {}", error_chain(&e))),
            }
        });
        let results = futures::future::join_all(tries).await;
        match results.iter().find(|r| r.is_ok()) {
            Some(_) => Availability::new("True", "Passed", "all checks passed".into()),
            None => Availability::new(
                "False",
                "FailedDiscoveryCheck",
                results.into_iter().filter_map(Result::err).next_back().unwrap_or_default(),
            ),
        }
    }

    /// Writes the `Available` condition when it changed (`updateAPIServiceStatus`).
    async fn check_all(&self) {
        for a in self.services() {
            let cond = self.availability(&a).await;
            let current = a
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .and_then(|cs| cs.iter().find(|c| c["type"] == "Available"))
                .cloned();
            let same = current.as_ref().is_some_and(|c| {
                c["status"] == cond.status && c["reason"] == cond.reason.as_str() && c["message"] == cond.message.as_str()
            });
            if same {
                continue;
            }
            let transition = match &current {
                Some(c) if c["status"] == cond.status => c["lastTransitionTime"].clone(),
                _ => json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            };
            let name = s(&a, "/metadata/name").to_string();
            if cond.status != "True" || current.as_ref().is_some_and(|c| c["status"] != "True") {
                tracing::info!(apiservice = %name, status = cond.status, reason = %cond.reason, message = %cond.message, "APIService availability changed");
            }
            let Some(info) = self.server.registry.lookup(GROUP, "v1", "apiservices") else { return };
            let key = info.key("", &name);
            let Ok(mut fresh) = self.server.storage.get(&key).await else { continue };
            let mut conds: Vec<Value> = fresh.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default();
            conds.retain(|c| c["type"] != "Available");
            conds.push(json!({"type": "Available", "status": cond.status, "lastTransitionTime": transition, "reason": cond.reason, "message": cond.message}));
            fresh["status"] = json!({"conditions": conds});
            let rv = fresh.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|r| r.parse().ok());
            if let Err(e) = self.server.storage.update(&key, rv, fresh).await {
                tracing::debug!(apiservice = %name, "status update: {e}");
            }
        }
    }

    /// `proxyHandler.ServeHTTP` for one APIService.
    async fn proxy(&self, a: &Value, req: &AggregatedRequest<'_>) -> Result<Response, ApiError> {
        if !available(a) {
            return Ok(text(503, "service unavailable\n"));
        }
        let addr = match self.resolve(a).await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!("error resolving {}/{}: {e}", s(a, "/spec/service/namespace"), s(a, "/spec/service/name"));
                return Ok(text(503, "service unavailable\n"));
            }
        };
        let (client, base) = self.client(a, addr, None).map_err(ApiError::internal)?;
        let url = match req.raw_query.filter(|q| !q.is_empty()) {
            Some(q) => format!("{base}{}?{q}", req.path),
            None => format!("{base}{}", req.path),
        };
        let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes()).map_err(|e| ApiError::internal(e.to_string()))?;
        let mut out = client.request(method, url);
        for (k, v) in req.headers {
            let n = k.as_str();
            if matches!(n, "authorization" | "host" | "connection" | "upgrade" | "content-length" | "transfer-encoding" | "keep-alive")
                || n.starts_with("impersonate-")
                || n.starts_with("x-remote-")
            {
                continue;
            }
            out = out.header(n, v.as_bytes());
        }
        // `SetAuthProxyHeaders`: who the API server authenticated.
        out = out.header("X-Remote-User", req.user.name.as_str());
        for g in &req.user.groups {
            out = out.header("X-Remote-Group", g.as_str());
        }
        for (k, vs) in &req.user.extra {
            for v in vs {
                out = out.header(format!("X-Remote-Extra-{}", header_key_escape(k)), v.as_str());
            }
        }
        if !req.body.is_empty() {
            out = out.body(req.body.clone());
        }
        let resp = match out.send().await {
            Ok(r) => r,
            Err(e) => return Ok(text(503, &format!("error trying to reach service: {}\n", error_chain(&e)))),
        };
        let mut b = Response::builder().status(resp.status().as_u16());
        for (k, v) in resp.headers() {
            if matches!(k.as_str(), "connection" | "transfer-encoding" | "content-length" | "keep-alive") {
                continue;
            }
            if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_str().as_bytes()), HeaderValue::from_bytes(v.as_bytes())) {
                b = b.header(n, val);
            }
        }
        Ok(b.body(Body::from_stream(resp.bytes_stream())).unwrap())
    }
}

fn text(code: u16, msg: &str) -> Response {
    Response::builder().status(code).header("Content-Type", "text/plain; charset=utf-8").body(Body::from(msg.to_string())).unwrap()
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(x) = src {
        s.push_str(&format!(": {x}"));
        src = x.source();
    }
    s
}

/// `headerKeyEscape`: extra keys are percent-encoded except for legal token characters.
fn header_key_escape(k: &str) -> String {
    k.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"!#$&'*+-.^_`|~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[async_trait]
impl Aggregator for KubeAggregator {
    fn api_groups(&self) -> Vec<Value> {
        let remote: Vec<Value> = self.services().into_iter().filter(|a| has_service(a) && !s(a, "/spec/group").is_empty()).collect();
        let local = self.server.registry.groups();
        sorted_groups(&remote).into_iter().filter(|(g, _)| !local.contains_key(g)).map(|(g, vs)| discovery_group(&g, &vs)).collect()
    }

    async fn serve(&self, req: AggregatedRequest<'_>) -> Option<Result<Response, ApiError>> {
        let rest = req.path.strip_prefix("/apis/")?.trim_end_matches('/');
        let mut parts = rest.splitn(3, '/');
        let group = parts.next()?;
        let version = parts.next();
        let services: Vec<Value> = self.services().into_iter().filter(|a| s(a, "/spec/group") == group && has_service(a)).collect();
        if services.is_empty() {
            return None;
        }
        match version {
            // `/apis/<group>`: the group document (`apiGroupHandler`).
            None => {
                let (_, vs) = sorted_groups(&services).into_iter().next()?;
                let mut doc = discovery_group(group, &vs);
                doc["kind"] = json!("APIGroup");
                doc["apiVersion"] = json!("v1");
                Some(Ok(Response::builder()
                    .status(200)
                    .header("Content-Type", "application/json")
                    .body(Body::from(doc.to_string()))
                    .unwrap()))
            }
            Some(v) => {
                let a = services.into_iter().find(|a| s(a, "/spec/version") == v)?;
                Some(self.proxy(&a, &req).await)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, g: &str, v: &str, gp: i64, vp: i64) -> Value {
        json!({"metadata": {"name": name}, "spec": {"group": g, "version": v, "groupPriorityMinimum": gp, "versionPriority": vp,
            "service": {"namespace": "ns", "name": "x"}}})
    }

    #[test]
    fn ordering() {
        let list = vec![
            svc("v1beta1.metrics.k8s.io", "metrics.k8s.io", "v1beta1", 100, 100),
            svc("v1alpha1.metrics.k8s.io", "metrics.k8s.io", "v1alpha1", 100, 100),
            svc("v1.custom.io", "custom.io", "v1", 1000, 15),
        ];
        let g = sorted_groups(&list);
        assert_eq!(g[0].0, "custom.io");
        assert_eq!(s(&g[1].1[0], "/spec/version"), "v1beta1");
        let d = discovery_group("metrics.k8s.io", &g[1].1);
        assert_eq!(d["preferredVersion"]["groupVersion"], "metrics.k8s.io/v1beta1");
        assert_eq!(d["versions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn misc() {
        assert_eq!(priorities("", "v1"), Some((18000, 1)));
        assert_eq!(priorities("metrics.k8s.io", "v1beta1"), None);
        assert_eq!(header_key_escape("authentication.kubernetes.io/credential-id"), "authentication.kubernetes.io%2Fcredential-id");
        assert!(
            version_rank("v2") > version_rank("v1")
                && version_rank("v1") > version_rank("v1beta1")
                && version_rank("v1beta1") > version_rank("v1alpha1")
        );
    }
}
