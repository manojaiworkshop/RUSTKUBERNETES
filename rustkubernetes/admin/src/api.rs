//! HTTP API (`/api/*`) and the embedded web UI.
//!
//! Every `/api` call except `/api/login` needs the session cookie. Writes must be
//! JSON and same-origin (CSRF protection on top of `SameSite=Strict`). Console
//! actions check console permissions; anything inside a cluster goes through
//! `/api/kube/<cluster>/<kubernetes path>` with the user impersonated, so the
//! cluster's RBAC has the last word.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{self, Principal, COOKIE};
use crate::clusters;
use crate::machines;
use crate::state::{dns_label, now, Cluster, Grant, Machine, Ports, User, PERMISSIONS};
use crate::transport::SshPaths;
use crate::App;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

type S = State<Arc<App>>;
type R = Result<Response, ApiErr>;

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn bad(m: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, m.into())
}
fn forbidden(m: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::FORBIDDEN, m.into())
}
fn not_found(m: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::NOT_FOUND, m.into())
}
fn internal(e: impl std::fmt::Display) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn ok(v: Value) -> R {
    Ok(Json(v).into_response())
}

/// The signed-in user, or 401.
fn principal(app: &App, h: &HeaderMap) -> Result<Principal, ApiErr> {
    let unauth = || ApiErr(StatusCode::UNAUTHORIZED, "not signed in".into());
    let token = auth::cookie_value(h, COOKIE).ok_or_else(unauth)?;
    let user = app.sessions.user(&token).ok_or_else(unauth)?;
    if user == app.super_user {
        return Ok(Principal {
            username: user,
            superadmin: true,
            permissions: PERMISSIONS.iter().map(|(p, _)| p.to_string()).collect(),
            grants: vec![],
        });
    }
    let u = app.store.read(|d| d.users.iter().find(|u| u.username == user).cloned()).ok_or_else(unauth)?;
    if u.disabled {
        return Err(unauth());
    }
    Ok(Principal { username: u.username, superadmin: false, permissions: u.permissions, grants: u.grants })
}

fn need(p: &Principal, perm: &str) -> Result<(), ApiErr> {
    if p.can(perm) {
        Ok(())
    } else {
        Err(forbidden(format!("you need the {perm} permission")))
    }
}

pub fn router(app: Arc<App>, dev_origin: Option<String>) -> Router {
    let dev = dev_origin.clone();
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
        .route("/api/me/password", post(change_password))
        .route("/api/meta", get(meta))
        .route("/api/events", get(events))
        .route("/api/users", get(users_list).post(users_create))
        .route("/api/users/update", post(users_update))
        .route("/api/users/delete", post(users_delete))
        .route("/api/machines", get(machines_list).post(machines_add))
        .route("/api/machines/install", post(machines_install))
        .route("/api/machines/test", post(machines_test))
        .route("/api/machines/delete", post(machines_delete))
        .route("/api/bundle/build", post(bundle_build))
        .route("/api/clusters", get(clusters_list).post(clusters_create))
        .route("/api/clusters/import", post(clusters_import))
        .route("/api/clusters/action", post(clusters_action))
        .route("/api/clusters/services", get(clusters_services))
        .route("/api/clusters/service-log", get(clusters_service_log))
        .route("/api/clusters/kubeconfig", get(clusters_kubeconfig))
        .route("/api/clusters/apply", post(clusters_apply))
        .route("/api/kube/{cluster}/{*path}", any(kube_proxy))
        .route("/api/audit", get(audit))
        .route("/api/jobs", get(jobs))
        .fallback(static_files)
        .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let dev = dev.clone();
            async move { csrf(req, next, dev).await }
        }))
        .with_state(app)
}

/// Writes must come from our own origin (or the dev server) and be JSON.
async fn csrf(req: axum::extract::Request, next: axum::middleware::Next, dev: Option<String>) -> Response {
    let m = req.method().clone();
    if req.uri().path().starts_with("/api/") && m != Method::GET && m != Method::HEAD {
        let h = req.headers();
        if let Some(origin) = h.get("origin").and_then(|v| v.to_str().ok()) {
            let host = h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
            let same = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) == Some(host);
            if !same && dev.as_deref() != Some(origin) {
                return ApiErr(StatusCode::FORBIDDEN, "cross-origin request refused".into()).into_response();
            }
        }
        let ct = h.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
        let kube = req.uri().path().starts_with("/api/kube/");
        if !kube && !ct.starts_with("application/json") {
            return ApiErr(StatusCode::UNSUPPORTED_MEDIA_TYPE, "send JSON".into()).into_response();
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(State(app): S, Json(r): Json<LoginReq>) -> R {
    let username = r.username.trim().to_string();
    let ok_super = auth::same(&username, &app.super_user) && auth::same(&r.password, &app.super_password);
    let ok_user = !ok_super
        && app.store.read(|d| {
            d.users.iter().any(|u| u.username == username && !u.disabled && auth::verify_password(&r.password, &u.password_hash))
        });
    if !ok_super && !ok_user {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        app.store.audit(&username, "login.failed", "");
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "wrong username or password".into()));
    }
    let token = app.sessions.create(&username);
    app.store.audit(&username, "login", "");
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200");
    Ok(([("set-cookie", cookie)], Json(json!({"ok": true}))).into_response())
}

async fn logout(State(app): S, h: HeaderMap) -> R {
    if let Some(t) = auth::cookie_value(&h, COOKIE) {
        app.sessions.remove(&t);
    }
    Ok(([("set-cookie", format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"))], Json(json!({"ok": true}))).into_response())
}

async fn session(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    ok(json!({"user": p.to_json(), "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Deserialize)]
struct PasswordReq {
    current: String,
    new: String,
}

async fn change_password(State(app): S, h: HeaderMap, Json(r): Json<PasswordReq>) -> R {
    let p = principal(&app, &h)?;
    if p.superadmin {
        return Err(bad("the super admin's password comes from RUSTKUBEADMIN_PASSWORD"));
    }
    if r.new.len() < 8 {
        return Err(bad("the new password needs at least 8 characters"));
    }
    let okay = app.store.write(|d| match d.users.iter_mut().find(|u| u.username == p.username) {
        Some(u) if auth::verify_password(&r.current, &u.password_hash) => {
            u.password_hash = auth::hash_password(&r.new);
            true
        }
        _ => false,
    });
    if !okay {
        return Err(bad("the current password is wrong"));
    }
    app.store.audit(&p.username, "password.change", "");
    ok(json!({"ok": true}))
}

async fn meta(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    let key = app.ssh.ensure_admin_key().await.unwrap_or_default();
    ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "superUser": app.super_user,
        "permissions": PERMISSIONS.iter().map(|(k, d)| json!({"id": k, "description": d})).collect::<Vec<_>>(),
        "bundle": app.bundle.info(),
        "missingBinaries": app.bundle.missing_binaries(),
        "sshPublicKey": key,
        "localAddress": machines::local_address().await,
        "localUser": std::env::var("USER").unwrap_or_default(),
        "hasLocalMachine": app.store.read(|d| d.machines.iter().any(|m| m.transport == "local")),
        "kubeIdentity": p.kube_identity().map(|(u, _)| u),
    }))
}

// ---------------------------------------------------------------------------
// live events
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TopicsQ {
    topics: String,
}

fn visible(app: &App, p: &Principal, topic: &str, v: Value) -> Option<Value> {
    match topic {
        "clusters" => {
            if p.superadmin || p.can("clusters.manage") {
                return Some(v);
            }
            Some(Value::Array(v.as_array()?.iter().filter(|c| p.has_cluster(c["name"].as_str().unwrap_or(""))).cloned().collect()))
        }
        "machines" => Some(v),
        "jobs" => {
            if p.can("audit.view") {
                return Some(v);
            }
            Some(Value::Array(v.as_array()?.iter().filter(|j| j["user"] == p.username).cloned().collect()))
        }
        "users" => p.can("users.manage").then_some(v),
        t => {
            let name = t.strip_prefix("cluster:")?;
            if !p.has_cluster(name) {
                return None;
            }
            let _ = app;
            Some(clusters::filter_snapshot(v, &p.visible_namespaces(name)))
        }
    }
}

async fn events(State(app): S, h: HeaderMap, Query(q): Query<TopicsQ>) -> R {
    let p = principal(&app, &h)?;
    let topics: Vec<String> = q.topics.split(',').filter(|t| !t.is_empty()).map(String::from).collect();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let mut sub = app.hub.subscribe();
    for t in &topics {
        let v = app.hub.latest(t).or_else(|| {
            t.starts_with("cluster:").then(
                || json!({"reachable": false, "nodes": [], "pods": [], "deployments": [], "services": [], "namespaces": [], "events": []}),
            )
        });
        if let Some(v) = v.and_then(|v| visible(&app, &p, t, v)) {
            let _ = tx.send(Ok(Event::default().event(t.clone()).data(v.to_string()))).await;
        }
    }
    let app2 = app.clone();
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok((t, v)) => {
                    if !topics.contains(&t) {
                        continue;
                    }
                    if let Some(v) = visible(&app2, &p, &t, v) {
                        if tx.send(Ok(Event::default().event(t).data(v.to_string()))).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    Ok(Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response())
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

fn user_json(u: &User) -> Value {
    json!({"username": u.username, "permissions": u.permissions, "grants": u.grants, "disabled": u.disabled, "created": u.created_at, "createdBy": u.created_by})
}

pub fn publish_users(app: &App) {
    let v: Vec<Value> = app.store.read(|d| d.users.iter().map(user_json).collect());
    app.hub.publish("users", Value::Array(v));
}

fn valid_username(u: &str) -> bool {
    let b = u.as_bytes();
    !u.is_empty()
        && u.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && u.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

fn check_grants(app: &App, grants: &[Grant]) -> Result<(), ApiErr> {
    for g in grants {
        if !["admin", "edit", "view"].contains(&g.role.as_str()) {
            return Err(bad(format!("unknown role {:?} (admin, edit or view)", g.role)));
        }
        if app.store.cluster(&g.cluster).is_none() {
            return Err(bad(format!("unknown cluster {:?}", g.cluster)));
        }
        if !g.namespace.is_empty() && dns_label(&g.namespace) != g.namespace {
            return Err(bad(format!("invalid namespace {:?}", g.namespace)));
        }
    }
    Ok(())
}

fn check_permissions(perms: &[String]) -> Result<(), ApiErr> {
    for p in perms {
        if !PERMISSIONS.iter().any(|(k, _)| k == p) {
            return Err(bad(format!("unknown permission {p:?}")));
        }
    }
    Ok(())
}

async fn users_list(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    need(&p, "users.manage")?;
    ok(json!(app.store.read(|d| d.users.iter().map(user_json).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct UserReq {
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    disabled: bool,
}

async fn users_create(State(app): S, h: HeaderMap, Json(r): Json<UserReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "users.manage")?;
    let name = r.username.trim().to_string();
    if !valid_username(&name) || name == app.super_user {
        return Err(bad("usernames use lowercase letters, digits, . _ - (up to 32) and must differ from the super admin"));
    }
    if r.password.len() < 8 {
        return Err(bad("the password needs at least 8 characters"));
    }
    check_permissions(&r.permissions)?;
    check_grants(&app, &r.grants)?;
    if !p.superadmin && r.permissions.iter().any(|x| !p.can(x)) {
        return Err(forbidden("you can only give permissions you have"));
    }
    let created = app.store.write(|d| {
        if d.users.iter().any(|u| u.username == name) {
            return false;
        }
        d.users.push(User {
            username: name.clone(),
            password_hash: auth::hash_password(&r.password),
            permissions: r.permissions.clone(),
            grants: r.grants.clone(),
            disabled: false,
            created_at: now(),
            created_by: p.username.clone(),
        });
        true
    });
    if !created {
        return Err(bad(format!("user {name} already exists")));
    }
    app.store.audit(&p.username, "user.create", &format!("{name} permissions={:?} grants={}", r.permissions, r.grants.len()));
    publish_users(&app);
    clusters::sync_grants(&app, None).await;
    ok(json!({"ok": true}))
}

async fn users_update(State(app): S, h: HeaderMap, Json(r): Json<UserReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "users.manage")?;
    check_permissions(&r.permissions)?;
    check_grants(&app, &r.grants)?;
    if !p.superadmin && r.permissions.iter().any(|x| !p.can(x)) {
        return Err(forbidden("you can only give permissions you have"));
    }
    if !r.password.is_empty() && r.password.len() < 8 {
        return Err(bad("the password needs at least 8 characters"));
    }
    let found = app.store.write(|d| match d.users.iter_mut().find(|u| u.username == r.username) {
        Some(u) => {
            u.permissions = r.permissions.clone();
            u.grants = r.grants.clone();
            u.disabled = r.disabled;
            if !r.password.is_empty() {
                u.password_hash = auth::hash_password(&r.password);
            }
            true
        }
        None => false,
    });
    if !found {
        return Err(not_found("no such user"));
    }
    if r.disabled || !r.password.is_empty() {
        app.sessions.remove_user(&r.username);
    }
    app.store.audit(
        &p.username,
        "user.update",
        &format!("{} permissions={:?} grants={} disabled={}", r.username, r.permissions, r.grants.len(), r.disabled),
    );
    publish_users(&app);
    clusters::sync_grants(&app, None).await;
    ok(json!({"ok": true}))
}

#[derive(Deserialize)]
struct NameReq {
    #[serde(alias = "username", alias = "id")]
    name: String,
}

async fn users_delete(State(app): S, h: HeaderMap, Json(r): Json<NameReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "users.manage")?;
    if r.name == p.username {
        return Err(bad("you cannot delete yourself"));
    }
    app.store.write(|d| d.users.retain(|u| u.username != r.name));
    app.sessions.remove_user(&r.name);
    app.store.audit(&p.username, "user.delete", &r.name);
    publish_users(&app);
    clusters::sync_grants(&app, None).await;
    ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// machines
// ---------------------------------------------------------------------------

async fn machines_list(State(app): S, h: HeaderMap) -> R {
    principal(&app, &h)?;
    ok(app.hub.latest("machines").unwrap_or(json!([])))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MachineReq {
    name: String,
    address: String,
    transport: String,
    #[serde(default)]
    ssh_user: String,
    #[serde(default)]
    ssh_port: u16,
    #[serde(default)]
    ssh_auth: String,
    #[serde(default)]
    private_key: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "yes")]
    install: bool,
}

fn yes() -> bool {
    true
}

async fn machines_add(State(app): S, h: HeaderMap, Json(r): Json<MachineReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "machines.manage")?;
    let name = dns_label(r.name.trim());
    if name.is_empty() {
        return Err(bad("give the machine a name"));
    }
    let address = r.address.trim().to_string();
    if address.is_empty() || address.contains(char::is_whitespace) {
        return Err(bad("give the machine's IP address"));
    }
    if !["local", "ssh"].contains(&r.transport.as_str()) {
        return Err(bad("transport must be local or ssh"));
    }
    let prefix = if r.prefix.trim().is_empty() { "$HOME/.local/rustkube".to_string() } else { r.prefix.trim().to_string() };
    machines::check_prefix(&prefix).map_err(|e| bad(e.to_string()))?;
    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let auth = if r.transport == "ssh" {
        if r.ssh_user.trim().is_empty() {
            return Err(bad("SSH needs a user name"));
        }
        match r.ssh_auth.as_str() {
            "password" if r.password.is_empty() => return Err(bad("enter the SSH password")),
            "password" => SshPaths::store_secret(&app.ssh.machine_password(&id), &r.password).map_err(internal)?,
            "key" if !r.private_key.contains("PRIVATE KEY") => return Err(bad("paste a private key (PEM / OpenSSH format)")),
            "key" => SshPaths::store_secret(&app.ssh.machine_key(&id), r.private_key.trim()).map_err(internal)?,
            _ => {}
        }
        if r.ssh_auth.is_empty() {
            "admin-key".to_string()
        } else {
            r.ssh_auth.clone()
        }
    } else {
        String::new()
    };
    let dup = app.store.read(|d| d.machines.iter().any(|m| m.name == name || (r.transport == "local" && m.transport == "local")));
    if dup {
        return Err(bad(if r.transport == "local" {
            "this machine is already added".to_string()
        } else {
            format!("a machine named {name} exists")
        }));
    }
    let m = Machine {
        id: id.clone(),
        name: name.clone(),
        address,
        transport: r.transport.clone(),
        ssh_user: r.ssh_user.trim().into(),
        ssh_port: if r.ssh_port == 0 { 22 } else { r.ssh_port },
        ssh_auth: auth,
        prefix,
        status: "new".into(),
        created_at: now(),
        created_by: p.username.clone(),
        ..Default::default()
    };
    app.store.write(|d| d.machines.push(m));
    app.store.audit(&p.username, "machine.add", &format!("{name} ({})", r.transport));
    machines::publish(&app);
    let job = if r.install { Some(spawn_install(&app, &id, &name, &p.username)) } else { None };
    ok(json!({"id": id, "job": job}))
}

fn spawn_install(app: &Arc<App>, id: &str, name: &str, user: &str) -> String {
    let job = app.jobs.start("install", &format!("Install rustkubernetes on {name}"), id, user);
    let jid = job.id.clone();
    let (app2, id2) = (app.clone(), id.to_string());
    tokio::spawn(async move {
        let r = machines::install(app2, id2, job.clone()).await;
        job.finish(&r);
    });
    jid
}

async fn machines_install(State(app): S, h: HeaderMap, Json(r): Json<NameReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "machines.manage")?;
    let m = app.store.machine(&r.name).ok_or_else(|| not_found("no such machine"))?;
    if app.jobs.running_for(&m.id) {
        return Err(bad("an install is already running on this machine"));
    }
    app.store.audit(&p.username, "machine.install", &m.name);
    ok(json!({"job": spawn_install(&app, &m.id, &m.name, &p.username)}))
}

async fn machines_test(State(app): S, h: HeaderMap, Json(r): Json<NameReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "machines.manage")?;
    match machines::test(&app, &r.name).await {
        Ok(out) => ok(json!({"ok": true, "output": out})),
        Err(e) => ok(json!({"ok": false, "output": format!("{e:#}")})),
    }
}

async fn machines_delete(State(app): S, h: HeaderMap, Json(r): Json<NameReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "machines.manage")?;
    let m = app.store.machine(&r.name).ok_or_else(|| not_found("no such machine"))?;
    let used = app.store.read(|d| d.clusters.iter().find(|c| c.master == m.id || c.workers.contains(&m.id)).map(|c| c.name.clone()));
    if let Some(c) = used {
        return Err(bad(format!("{} is part of cluster {c}: delete the cluster first", m.name)));
    }
    let _ = machines::uninstall(&app, &m.id).await;
    app.store.write(|d| d.machines.retain(|x| x.id != m.id));
    let _ = std::fs::remove_file(app.ssh.machine_key(&m.id));
    let _ = std::fs::remove_file(app.ssh.machine_password(&m.id));
    app.store.audit(&p.username, "machine.delete", &m.name);
    machines::publish(&app);
    ok(json!({"ok": true}))
}

async fn bundle_build(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    need(&p, "machines.manage")?;
    let job = app.jobs.start("bundle", "Build the offline bundle", "bundle", &p.username);
    let jid = job.id.clone();
    let app2 = app.clone();
    tokio::spawn(async move {
        let r = app2.bundle.ensure(&job).await.map(|_| ());
        job.finish(&r);
    });
    ok(json!({"job": jid}))
}

// ---------------------------------------------------------------------------
// clusters
// ---------------------------------------------------------------------------

async fn clusters_list(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    let v = app.hub.latest("clusters").unwrap_or(json!([]));
    ok(visible(&app, &p, "clusters", v).unwrap_or(json!([])))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterReq {
    name: String,
    master: String,
    #[serde(default)]
    workers: Vec<String>,
    #[serde(default)]
    ports: Option<Ports>,
    #[serde(default = "yes")]
    dns: bool,
    #[serde(default = "yes")]
    kube_proxy: bool,
}

fn valid_cluster_name(app: &App, n: &str) -> Result<(), ApiErr> {
    if n.is_empty() || n.len() > 30 || dns_label(n) != n {
        return Err(bad("cluster names use lowercase letters, digits and - (up to 30)"));
    }
    if app.store.cluster(n).is_some() {
        return Err(bad(format!("cluster {n} exists")));
    }
    Ok(())
}

fn grant_creator(app: &App, p: &Principal, cluster: &str) {
    if p.superadmin {
        return;
    }
    app.store.write(|d| {
        if let Some(u) = d.users.iter_mut().find(|u| u.username == p.username) {
            u.grants.push(Grant { cluster: cluster.into(), namespace: String::new(), role: "admin".into() });
        }
    });
    publish_users(app);
}

async fn clusters_create(State(app): S, h: HeaderMap, Json(r): Json<ClusterReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "clusters.manage")?;
    let name = r.name.trim().to_string();
    valid_cluster_name(&app, &name)?;
    let mut ids = vec![r.master.clone()];
    ids.extend(r.workers.iter().filter(|w| **w != r.master).cloned());
    for id in &ids {
        let m = app.store.machine(id).ok_or_else(|| bad(format!("unknown machine {id}")))?;
        if m.status != "ready" {
            return Err(bad(format!("machine {} is not installed yet (status {})", m.name, m.status)));
        }
        let used = app.store.read(|d| d.clusters.iter().find(|c| c.master == *id || c.workers.contains(id)).map(|c| c.name.clone()));
        if let Some(c) = used {
            return Err(bad(format!("machine {} already belongs to cluster {c}", m.name)));
        }
    }
    let master = app.store.machine(&r.master).unwrap();
    if master.address.parse::<std::net::IpAddr>().is_err() {
        return Err(bad(format!("the master's address must be an IP address (machine {} has {:?})", master.name, master.address)));
    }
    let ports = r.ports.unwrap_or_default();
    app.store.write(|d| {
        d.clusters.push(Cluster {
            name: name.clone(),
            kind: "managed".into(),
            status: "provisioning".into(),
            endpoint: format!("https://{}:{}", master.address, ports.api),
            master: r.master.clone(),
            workers: ids[1..].to_vec(),
            ports,
            dns: r.dns,
            kube_proxy: r.kube_proxy,
            created_at: now(),
            created_by: p.username.clone(),
            ..Default::default()
        })
    });
    grant_creator(&app, &p, &name);
    app.store.audit(&p.username, "cluster.create", &format!("{name} master={} workers={}", master.name, ids.len() - 1));
    machines::publish(&app);
    clusters::publish_clusters(&app);
    ok(json!({"job": spawn_provision(&app, &name, &p.username)}))
}

fn spawn_provision(app: &Arc<App>, name: &str, user: &str) -> String {
    let job = app.jobs.start("provision", &format!("Create cluster {name}"), name, user);
    let jid = job.id.clone();
    let (app2, n) = (app.clone(), name.to_string());
    tokio::spawn(async move {
        let r = clusters::provision(app2.clone(), n.clone(), job.clone()).await;
        if let Err(e) = &r {
            app2.store.update_cluster(&n, |c| {
                c.status = "failed".into();
                c.message = format!("{e:#}");
            });
        }
        clusters::publish_clusters(&app2);
        job.finish(&r);
    });
    jid
}

#[derive(Deserialize)]
struct ImportReq {
    name: String,
    kubeconfig: String,
}

async fn clusters_import(State(app): S, h: HeaderMap, Json(r): Json<ImportReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "clusters.manage")?;
    let name = r.name.trim().to_string();
    valid_cluster_name(&app, &name)?;
    clusters::import(&app, &name, &r.kubeconfig, &p.username).await.map_err(|e| {
        let _ = std::fs::remove_dir_all(app.store.cluster_dir(&name));
        bad(format!("{e:#}"))
    })?;
    grant_creator(&app, &p, &name);
    app.store.audit(&p.username, "cluster.import", &name);
    clusters::publish_clusters(&app);
    ok(json!({"ok": true}))
}

#[derive(Deserialize)]
struct ActionReq {
    name: String,
    action: String,
}

async fn clusters_action(State(app): S, h: HeaderMap, Json(r): Json<ActionReq>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "clusters.manage")?;
    let c = app.store.cluster(&r.name).ok_or_else(|| not_found("no such cluster"))?;
    if app.jobs.running_for(&c.name) {
        return Err(bad("another operation is running on this cluster"));
    }
    if c.kind == "imported" && r.action != "delete" {
        return Err(bad("imported clusters can only be removed from the console"));
    }
    app.store.audit(&p.username, &format!("cluster.{}", r.action), &c.name);
    if r.action == "retry" {
        if c.status != "failed" {
            return Err(bad("only a failed cluster can be retried"));
        }
        app.store.update_cluster(&c.name, |x| {
            x.status = "provisioning".into();
            x.message.clear();
        });
        clusters::publish_clusters(&app);
        return ok(json!({"job": spawn_provision(&app, &c.name, &p.username)}));
    }
    let title = match r.action.as_str() {
        "start" => format!("Start cluster {}", c.name),
        "stop" => format!("Stop cluster {}", c.name),
        "delete" => format!("Delete cluster {}", c.name),
        _ => return Err(bad("action must be start, stop, delete or retry")),
    };
    let job = app.jobs.start(&r.action, &title, &c.name, &p.username);
    let jid = job.id.clone();
    let (app2, action) = (app.clone(), r.action.clone());
    tokio::spawn(async move {
        let name = c.name.clone();
        let res: anyhow::Result<()> = async {
            match action.as_str() {
                "start" => {
                    clusters::node_command(&app2, &name, "start", &job, false).await?;
                    app2.store.update_cluster(&name, |x| x.status = "running".into());
                    clusters::start_runtime(&app2, &name);
                }
                "stop" => {
                    clusters::stop_runtime(&app2, &name);
                    clusters::node_command(&app2, &name, "stop", &job, false).await?;
                    app2.store.update_cluster(&name, |x| x.status = "stopped".into());
                }
                _ => {
                    app2.store.update_cluster(&name, |x| x.status = "deleting".into());
                    clusters::publish_clusters(&app2);
                    clusters::stop_runtime(&app2, &name);
                    if c.kind == "managed" {
                        clusters::node_command(&app2, &name, "remove", &job, true).await?;
                    }
                    let _ = std::fs::remove_dir_all(app2.store.cluster_dir(&name));
                    app2.store.write(|d| {
                        d.clusters.retain(|x| x.name != name);
                        for u in d.users.iter_mut() {
                            u.grants.retain(|g| g.cluster != name);
                        }
                    });
                    publish_users(&app2);
                    machines::publish(&app2);
                    job.log(format!("cluster {name} removed"));
                }
            }
            Ok(())
        }
        .await;
        clusters::publish_clusters(&app2);
        job.finish(&res);
    });
    ok(json!({"job": jid}))
}

#[derive(Deserialize)]
struct ClusterQ {
    name: String,
    #[serde(default)]
    machine: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    lines: Option<u32>,
}

async fn clusters_services(State(app): S, h: HeaderMap, Query(q): Query<ClusterQ>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "clusters.manage")?;
    let c = app.store.cluster(&q.name).ok_or_else(|| not_found("no such cluster"))?;
    if c.kind != "managed" {
        return ok(json!([]));
    }
    ok(clusters::node_status(&app, &q.name).await.map_err(|e| bad(format!("{e:#}")))?)
}

async fn clusters_service_log(State(app): S, h: HeaderMap, Query(q): Query<ClusterQ>) -> R {
    let p = principal(&app, &h)?;
    need(&p, "clusters.manage")?;
    let text = clusters::service_log(&app, &q.name, &q.machine, &q.service, q.lines.unwrap_or(300).min(5000))
        .await
        .map_err(|e| bad(format!("{e:#}")))?;
    ok(json!({"log": text}))
}

async fn clusters_kubeconfig(State(app): S, h: HeaderMap, Query(q): Query<ClusterQ>) -> R {
    let p = principal(&app, &h)?;
    if !p.has_cluster(&q.name) {
        return Err(forbidden("you have no access to this cluster"));
    }
    let c = app.store.cluster(&q.name).ok_or_else(|| not_found("no such cluster"))?;
    let kc = clusters::user_kubeconfig(&app, &c, &p.username, p.superadmin).await.map_err(|e| bad(format!("{e:#}")))?;
    app.store.audit(&p.username, "cluster.kubeconfig", &c.name);
    Ok((
        [
            ("content-type", "application/yaml".to_string()),
            ("content-disposition", format!("attachment; filename=\"{}-{}.kubeconfig\"", c.name, p.username)),
        ],
        kc,
    )
        .into_response())
}

#[derive(Deserialize)]
struct ApplyReq {
    name: String,
    yaml: String,
    #[serde(default)]
    namespace: String,
}

fn cluster_client(app: &App, p: &Principal, name: &str) -> Result<k8s_client_go::Client, ApiErr> {
    if !p.has_cluster(name) {
        return Err(forbidden("you have no access to this cluster"));
    }
    let rt = app.runtimes.lock().unwrap().get(name).cloned().ok_or_else(|| bad(format!("cluster {name} is not running")))?;
    Ok(match p.kube_identity() {
        Some((u, g)) => rt.client.impersonating(&u, &g),
        None => rt.client.clone(),
    })
}

async fn clusters_apply(State(app): S, h: HeaderMap, Json(r): Json<ApplyReq>) -> R {
    let p = principal(&app, &h)?;
    let client = cluster_client(&app, &p, &r.name)?;
    let ns = if r.namespace.is_empty() { "default" } else { &r.namespace };
    let out = clusters::apply_yaml(&client, &r.yaml, ns).await.map_err(|e| bad(format!("{e:#}")))?;
    app.store.audit(&p.username, "cluster.apply", &format!("{}: {}", r.name, out.join("; ")));
    ok(json!({"results": out}))
}

/// `/api/kube/<cluster>/<path>` → the cluster's API server, as the signed-in user.
async fn kube_proxy(
    State(app): S,
    h: HeaderMap,
    Path((cluster, path)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> R {
    let p = principal(&app, &h)?;
    let client = cluster_client(&app, &p, &cluster)?;
    let full = match uri.query() {
        Some(q) => format!("/{path}?{q}"),
        None => format!("/{path}"),
    };
    if !(full.starts_with("/api/")
        || full.starts_with("/apis/")
        || full == "/version"
        || full.starts_with("/api?")
        || full == "/api"
        || full == "/apis")
    {
        return Err(bad("only Kubernetes API paths can be proxied"));
    }
    let ct = h.get("content-type").and_then(|v| v.to_str().ok());
    let m = clusters::reqwest_method(method.as_str());
    let (status, ctype, bytes) = client
        .raw(m, &full, ct.filter(|_| !body.is_empty()), body.to_vec())
        .await
        .map_err(|e| ApiErr(StatusCode::BAD_GATEWAY, e.to_string()))?;
    if method != Method::GET {
        app.store.audit(&p.username, &format!("kube.{}", method.as_str().to_lowercase()), &format!("{cluster}{full} → {status}"));
    }
    Ok(Response::builder().status(status).header("content-type", ctype).body(Body::from(bytes)).unwrap())
}

#[derive(Deserialize)]
struct LimitQ {
    #[serde(default)]
    limit: Option<usize>,
}

async fn audit(State(app): S, h: HeaderMap, Query(q): Query<LimitQ>) -> R {
    let p = principal(&app, &h)?;
    let all = app.store.audit_tail(q.limit.unwrap_or(300).min(5000) * if p.can("audit.view") { 1 } else { 10 });
    let v: Vec<Value> = if p.can("audit.view") { all } else { all.into_iter().filter(|e| e["user"] == p.username).collect() };
    ok(json!(v))
}

async fn jobs(State(app): S, h: HeaderMap) -> R {
    let p = principal(&app, &h)?;
    ok(visible(&app, &p, "jobs", app.jobs.list()).unwrap_or(json!([])))
}

// ---------------------------------------------------------------------------
// static UI
// ---------------------------------------------------------------------------

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn static_files(uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return not_found("no such API endpoint").into_response();
    }
    let candidates = if path == "/" {
        vec!["/index.html".to_string()]
    } else {
        let p = path.trim_end_matches('/');
        vec![p.to_string(), format!("{p}.html"), format!("{p}/index.html")]
    };
    for c in candidates {
        if let Some((name, body)) = assets::ASSETS.iter().find(|(n, _)| *n == c) {
            let cache = if name.starts_with("/_next/static/") { "public, max-age=31536000, immutable" } else { "no-cache" };
            return ([("content-type", content_type(name)), ("cache-control", cache)], *body).into_response();
        }
    }
    if assets::ASSETS.is_empty() {
        return (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], "<h1>rustkubeadmin</h1><p>The web UI is not built: run <code>npm install && npm run build</code> in <code>admin/web</code>, then rebuild rustkubeadmin.</p>").into_response();
    }
    match assets::ASSETS.iter().find(|(n, _)| *n == "/404.html") {
        Some((_, body)) => (StatusCode::NOT_FOUND, [("content-type", "text/html; charset=utf-8")], *body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
