//! The kubelet API (`pkg/kubelet/server`), reached by the API server for
//! `kubectl logs` and `kubectl exec`.
//!
//! * `GET /healthz`
//! * `GET /pods` — pods bound to this node
//! * `GET /containerLogs/{ns}/{pod}/{container}?follow&tailLines&timestamps&previous&sinceSeconds`
//! * `GET /exec/{ns}/{pod}/{container}?command=..&stdin&stdout&stderr&tty` — WebSocket using the
//!   Kubernetes channel protocol (`v5.channel.k8s.io` / `v4.channel.k8s.io`): every binary message
//!   starts with a channel byte — 0 stdin, 1 stdout, 2 stderr, 3 status (JSON), 4 resize, 255 close.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::dockershim::client::Demuxer;
use crate::runtime::{ContainerState, LogOptions};
use crate::status::attempts;
use crate::KubeletCtx;

pub fn router(ctx: Arc<KubeletCtx>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/pods", get(pods))
        .route("/containerLogs/{ns}/{pod}/{container}", get(logs))
        .route("/exec/{ns}/{pod}/{container}", get(exec))
        .with_state(ctx)
}

/// HTTPS settings of the kubelet API (`--tls-cert-file`, `--client-ca-file`, ...).
#[derive(Clone, Debug)]
pub struct TlsServing {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    /// Verifies client certificates (x509 authentication).
    pub client_ca: Option<Vec<u8>>,
    /// `--anonymous-auth`
    pub anonymous: bool,
    /// `--authorization-mode=Webhook` (SubjectAccessReview); otherwise AlwaysAllow.
    pub webhook_authz: bool,
}

/// `pkg/kubelet/server/auth.go`: authenticate (x509, bearer token via TokenReview)
/// and authorize (SubjectAccessReview on `nodes/<subresource>`) every request.
pub struct KubeletAuth {
    ctx: Arc<KubeletCtx>,
    cfg: TlsServing,
    /// token → (user, expires)
    tokens: std::sync::Mutex<std::collections::HashMap<String, (k8s_apiserver::authentication::UserInfo, std::time::Instant)>>,
    /// (user, verb, subresource) → (allowed, expires)
    decisions: std::sync::Mutex<std::collections::HashMap<String, (bool, std::time::Instant)>>,
}

impl KubeletAuth {
    pub fn new(ctx: Arc<KubeletCtx>, cfg: TlsServing) -> Arc<Self> {
        Arc::new(Self { ctx, cfg, tokens: Default::default(), decisions: Default::default() })
    }

    async fn authenticate(&self, parts: &axum::http::request::Parts) -> Result<k8s_apiserver::authentication::UserInfo, StatusCode> {
        use k8s_apiserver::authentication::{subject_of, UserInfo};
        let certs = parts.extensions.get::<k8s_apiserver::server::PeerCerts>().and_then(|p| p.0.clone());
        if let Some(leaf) = certs.as_ref().and_then(|c| c.first()) {
            let (name, groups) = subject_of(leaf).map_err(|_| StatusCode::UNAUTHORIZED)?;
            return Ok(UserInfo { name, groups, ..Default::default() });
        }
        let token = parts.headers.get("authorization").and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).map(str::trim);
        let Some(token) = token else {
            return if self.cfg.anonymous { Ok(UserInfo::anonymous()) } else { Err(StatusCode::UNAUTHORIZED) };
        };
        if let Some((u, exp)) = self.tokens.lock().unwrap().get(token).cloned() {
            if exp > std::time::Instant::now() {
                return Ok(u);
            }
        }
        let body = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {"token": token}});
        let r = self
            .ctx
            .client
            .send_json(reqwest::Method::POST, "/apis/authentication.k8s.io/v1/tokenreviews", &body)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        if r["status"]["authenticated"] != true {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let u = &r["status"]["user"];
        let user = UserInfo {
            name: u["username"].as_str().unwrap_or("").into(),
            uid: u["uid"].as_str().unwrap_or("").into(),
            groups: u["groups"].as_array().into_iter().flatten().filter_map(|g| g.as_str().map(String::from)).collect(),
            ..Default::default()
        };
        self.tokens.lock().unwrap().insert(token.to_string(), (user.clone(), std::time::Instant::now() + Duration::from_secs(120)));
        Ok(user)
    }

    /// Kubelet API path → `nodes` subresource (`GetRequestAttributes`).
    pub fn subresource(path: &str) -> &'static str {
        let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
        match first {
            "stats" => "stats",
            "metrics" => "metrics",
            "logs" => "log",
            "spec" => "spec",
            "healthz" => "healthz",
            "configz" => "configz",
            _ => "proxy",
        }
    }

    async fn authorize(&self, user: &k8s_apiserver::authentication::UserInfo, verb: &str, sub: &str) -> bool {
        if !self.cfg.webhook_authz {
            return true;
        }
        let key = format!("{}\0{}\0{verb}\0{sub}", user.name, user.groups.join(","));
        if let Some((ok, exp)) = self.decisions.lock().unwrap().get(&key).cloned() {
            if exp > std::time::Instant::now() {
                return ok;
            }
        }
        let body = json!({
            "apiVersion": "authorization.k8s.io/v1", "kind": "SubjectAccessReview",
            "spec": {"user": user.name, "groups": user.groups, "uid": user.uid,
                     "resourceAttributes": {"verb": verb, "resource": "nodes", "subresource": sub, "name": self.ctx.node_name}},
        });
        let ok = match self.ctx.client.send_json(reqwest::Method::POST, "/apis/authorization.k8s.io/v1/subjectaccessreviews", &body).await {
            Ok(r) => r["status"]["allowed"] == true,
            Err(e) => {
                tracing::warn!("SubjectAccessReview failed: {e}");
                return false;
            }
        };
        let ttl = if ok { Duration::from_secs(300) } else { Duration::from_secs(30) };
        self.decisions.lock().unwrap().insert(key, (ok, std::time::Instant::now() + ttl));
        ok
    }
}

/// Middleware: rejects unauthenticated (401) and unauthorized (403) requests.
pub async fn auth_middleware(State(auth): State<Arc<KubeletAuth>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let (parts, body) = req.into_parts();
    let user = match auth.authenticate(&parts).await {
        Ok(u) => u,
        Err(code) => return err(code, "Unauthorized"),
    };
    let verb = match parts.method.as_str() {
        "POST" => "create",
        "PUT" => "update",
        "PATCH" => "patch",
        "DELETE" => "delete",
        _ => "get",
    };
    let sub = KubeletAuth::subresource(parts.uri.path());
    if !auth.authorize(&user, verb, sub).await {
        tracing::info!(user = %user.name, verb, subresource = sub, "kubelet API request forbidden");
        return err(StatusCode::FORBIDDEN, format!("Forbidden (user={}, verb={verb}, resource=nodes, subresource={sub})", user.name));
    }
    next.run(axum::extract::Request::from_parts(parts, body)).await
}

/// The router with authentication/authorization in front.
pub fn secure_router(ctx: Arc<KubeletCtx>, cfg: TlsServing) -> Router {
    let auth = KubeletAuth::new(ctx.clone(), cfg);
    router(ctx).layer(axum::middleware::from_fn_with_state(auth, auth_middleware))
}

fn query_pairs(q: &Option<String>) -> Vec<(String, String)> {
    form_urlencoded_pairs(q.as_deref().unwrap_or(""))
}

fn form_urlencoded_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn flag(pairs: &[(String, String)], k: &str) -> bool {
    pairs.iter().any(|(kk, v)| kk == k && (v == "true" || v == "1"))
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, msg.into()).into_response()
}

async fn pods(State(ctx): State<Arc<KubeletCtx>>) -> Response {
    let items: Vec<Value> = ctx
        .pods
        .read()
        .unwrap()
        .as_ref()
        .map(|s| s.list().iter().map(|p| serde_json::to_value(p.as_ref()).unwrap_or_default()).collect())
        .unwrap_or_default();
    axum::Json(json!({"kind": "PodList", "apiVersion": "v1", "metadata": {}, "items": items})).into_response()
}

/// Container id for (ns, pod, container); `previous` picks the attempt before the latest.
fn container_id(ctx: &KubeletCtx, ns: &str, pod: &str, container: &str, previous: bool) -> Result<(String, bool), Response> {
    let uid = ctx
        .pods
        .read()
        .unwrap()
        .as_ref()
        .and_then(|s| s.get_ns(ns, pod))
        .and_then(|p| p.metadata.uid.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("pod {ns}/{pod} not found on this node")))?;
    let (_, containers) = ctx.cache.for_pod(&uid);
    let att = attempts(&containers, container);
    let c = if previous { att.get(1) } else { att.first() };
    match c {
        Some(c) => Ok((c.id.clone(), c.state == ContainerState::Running)),
        None if previous => {
            Err(err(StatusCode::BAD_REQUEST, format!("previous terminated container \"{container}\" in pod \"{pod}\" not found")))
        }
        None => {
            Err(err(StatusCode::BAD_REQUEST, format!("container \"{container}\" in pod \"{pod}\" is waiting to start: ContainerCreating")))
        }
    }
}

async fn logs(
    State(ctx): State<Arc<KubeletCtx>>,
    Path((ns, pod, container)): Path<(String, String, String)>,
    RawQuery(q): RawQuery,
) -> Response {
    let pairs = query_pairs(&q);
    let get = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    let (id, _) = match container_id(&ctx, &ns, &pod, &container, flag(&pairs, "previous")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let opts = LogOptions {
        follow: flag(&pairs, "follow"),
        tail_lines: get("tailLines").and_then(|v| v.parse().ok()),
        timestamps: flag(&pairs, "timestamps"),
        since_seconds: get("sinceSeconds").and_then(|v| v.parse().ok()),
    };
    match ctx.runtime.logs(&id, &opts).await {
        Ok(stream) => {
            let body = Body::from_stream(stream.map(|r| r.map(|(_, b)| b).map_err(std::io::Error::other)));
            Response::builder().header("Content-Type", "text/plain").body(body).unwrap()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub const PROTOCOLS: [&str; 2] = ["v5.channel.k8s.io", "v4.channel.k8s.io"];
const CH_STDIN: u8 = 0;
const CH_STDOUT: u8 = 1;
const CH_STDERR: u8 = 2;
const CH_ERROR: u8 = 3;
const CH_RESIZE: u8 = 4;
const CH_CLOSE: u8 = 255;

async fn exec(
    State(ctx): State<Arc<KubeletCtx>>,
    Path((ns, pod, container)): Path<(String, String, String)>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let pairs = query_pairs(&q);
    let command: Vec<String> = pairs.iter().filter(|(k, _)| k == "command").map(|(_, v)| v.clone()).collect();
    if command.is_empty() {
        return err(StatusCode::BAD_REQUEST, "you must specify at least one command for the container");
    }
    let (id, running) = match container_id(&ctx, &ns, &pod, &container, false) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !running {
        return err(StatusCode::BAD_REQUEST, format!("container {container} is not running"));
    }
    let tty = flag(&pairs, "tty");
    let stdin = flag(&pairs, "stdin");
    ws.protocols(PROTOCOLS).on_upgrade(move |socket| exec_session(ctx, id, command, tty, stdin, socket)).into_response()
}

fn status_message(code: Option<i32>, cmd: &[String], error: Option<String>) -> Bytes {
    let v = match (error, code) {
        (Some(e), _) => json!({"metadata": {}, "status": "Failure", "message": e, "reason": "InternalError"}),
        (None, Some(0)) | (None, None) => json!({"metadata": {}, "status": "Success"}),
        (None, Some(c)) => json!({
            "metadata": {}, "status": "Failure",
            "message": format!("command terminated with non-zero exit code: error executing command {cmd:?}, exit code {c}"),
            "reason": "NonZeroExitCode",
            "details": {"causes": [{"reason": "ExitCode", "message": c.to_string()}]},
        }),
    };
    let mut out = vec![CH_ERROR];
    out.extend(serde_json::to_vec(&v).unwrap_or_default());
    Bytes::from(out)
}

/// Normal closure (1000): kubectl treats any other close code as a stream error.
fn normal_close() -> Message {
    Message::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "".into() }))
}

fn frame(ch: u8, data: &[u8]) -> Message {
    let mut v = Vec::with_capacity(data.len() + 1);
    v.push(ch);
    v.extend_from_slice(data);
    Message::Binary(Bytes::from(v))
}

/// Bridges a WebSocket (channel protocol) and a runtime exec stream.
async fn exec_session(ctx: Arc<KubeletCtx>, id: String, cmd: Vec<String>, tty: bool, stdin: bool, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let sess = match ctx.runtime.exec_stream(&id, &cmd, tty, stdin).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ws_tx.send(Message::Binary(status_message(None, &cmd, Some(e.to_string())))).await;
            let _ = ws_tx.send(normal_close()).await;
            return;
        }
    };
    let exec_id = sess.exec_id.clone();
    let (mut rd, mut wr) = tokio::io::split(sess.io);
    let rt = ctx.runtime.clone();
    let resize_exec = exec_id.clone();
    // Client → container: stdin, resize, close.
    let input = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Some((&ch, payload)) = data.split_first() else { continue };
            match ch {
                CH_STDIN if !payload.is_empty() => {
                    if wr.write_all(payload).await.is_err() {
                        break;
                    }
                }
                CH_RESIZE => {
                    if let Ok(v) = serde_json::from_slice::<Value>(payload) {
                        let w = v["Width"].as_u64().unwrap_or(80) as u16;
                        let h = v["Height"].as_u64().unwrap_or(24) as u16;
                        let _ = rt.exec_resize(&resize_exec, w, h).await;
                    }
                }
                CH_CLOSE if payload.first() == Some(&CH_STDIN) => {
                    let _ = wr.shutdown().await;
                }
                _ => {}
            }
        }
    });
    // Container → client: stdout/stderr.
    let mut demux = Demuxer::default();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let frames: Vec<Message> = if tty {
                    vec![frame(CH_STDOUT, &buf[..n])]
                } else {
                    demux.push(&buf[..n]).into_iter().map(|(s, d)| frame(if s == 2 { CH_STDERR } else { CH_STDOUT }, &d)).collect()
                };
                for f in frames {
                    if ws_tx.send(f).await.is_err() {
                        input.abort();
                        return;
                    }
                }
            }
        }
    }
    input.abort();
    let mut code = None;
    for _ in 0..20 {
        match ctx.runtime.exec_exit_code(&exec_id).await {
            Ok(Some(c)) => {
                code = Some(c);
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(_) => break,
        }
    }
    let _ = ws_tx.send(Message::Binary(status_message(code, &cmd, None))).await;
    let _ = ws_tx.send(normal_close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subresources() {
        assert_eq!(KubeletAuth::subresource("/exec/ns/p/c"), "proxy");
        assert_eq!(KubeletAuth::subresource("/containerLogs/ns/p/c"), "proxy");
        assert_eq!(KubeletAuth::subresource("/stats/summary"), "stats");
        assert_eq!(KubeletAuth::subresource("/healthz"), "healthz");
    }

    #[test]
    fn query_decoding() {
        let p = form_urlencoded_pairs("command=sh&command=-c&command=echo+hi%21&tty=true&stdin=1");
        let cmd: Vec<&str> = p.iter().filter(|(k, _)| k == "command").map(|(_, v)| v.as_str()).collect();
        assert_eq!(cmd, ["sh", "-c", "echo hi!"]);
        assert!(flag(&p, "tty") && flag(&p, "stdin") && !flag(&p, "stderr"));
    }

    #[test]
    fn exit_status() {
        let s = status_message(Some(3), &["false".into()], None);
        assert_eq!(s[0], CH_ERROR);
        let v: Value = serde_json::from_slice(&s[1..]).unwrap();
        assert_eq!(v["reason"], "NonZeroExitCode");
        assert_eq!(v["details"]["causes"][0]["message"], "3");
    }
}
