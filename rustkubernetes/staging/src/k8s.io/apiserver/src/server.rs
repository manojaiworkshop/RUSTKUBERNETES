//! Port of `k8s.io/apiserver/pkg/server` (`GenericAPIServer`).
//!
//! Owns the registry, storage and admission chain, routes every HTTP request
//! (`/api`, `/apis`, `/version`, health checks) and logs requests like the Go
//! `httplog` filter.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::Router;
use k8s_apimachinery::errors::ApiError;
use serde_json::{json, Value};

use crate::admission::AdmissionChain;
use crate::authentication::{AuthRequest, AuthenticatorChain, UserInfo, CURRENT_USER};
use crate::authorization::{forbidden, request_attributes, AlwaysAllow, Authorizer, Decision};
use crate::handlers::{error_response, handle_resource, json_response};
use crate::registry::{Registry, StrategyContext};
use crate::request::{parse_path, Query, RequestTarget};
use crate::storage::Storage;
use crate::{discovery, openapi};

pub struct ApiServerConfig {
    pub registry: Registry,
    pub storage: Arc<dyn Storage>,
    pub admission: AdmissionChain,
    /// Body of `/version`.
    pub version: Value,
    /// Address advertised in `/api` discovery.
    pub advertise_address: String,
    /// Decoder for `application/vnd.kubernetes.protobuf` request bodies.
    pub protobuf_decoder: Option<BodyDecoder>,
    /// `None`: insecure development mode, every request is `system:admin`.
    pub authenticator: Option<Arc<AuthenticatorChain>>,
    /// `None`: `AlwaysAllow`.
    pub authorizer: Option<Arc<dyn Authorizer>>,
    /// TLS client config (CA + `--kubelet-client-certificate`) for `https://` kubelets.
    pub kubelet_tls: Option<Arc<rustls::ClientConfig>>,
    /// Static JSON documents served at fixed paths (e.g. `/openid/v1/jwks`).
    pub extra_paths: Vec<(String, Value)>,
}

/// Verified client certificate chain of the TLS connection a request arrived on.
#[derive(Clone, Debug)]
pub struct PeerCerts(pub Option<Arc<Vec<rustls_pki_types::CertificateDer<'static>>>>);

/// Decodes a non-JSON request body into JSON.
pub type BodyDecoder = fn(&[u8]) -> Result<Value, String>;

pub struct ApiServer {
    pub registry: Arc<Registry>,
    pub storage: Arc<dyn Storage>,
    pub admission: AdmissionChain,
    version: Value,
    advertise_address: String,
    pub(crate) protobuf_decoder: Option<BodyDecoder>,
    pub authenticator: Option<Arc<AuthenticatorChain>>,
    pub authorizer: Arc<dyn Authorizer>,
    pub kubelet_tls: Option<Arc<rustls::ClientConfig>>,
    extra_paths: Vec<(String, Value)>,
    ready: AtomicBool,
}

impl ApiServer {
    pub fn new(cfg: ApiServerConfig) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(cfg.registry),
            storage: cfg.storage,
            admission: cfg.admission,
            version: cfg.version,
            advertise_address: cfg.advertise_address,
            protobuf_decoder: cfg.protobuf_decoder,
            authenticator: cfg.authenticator,
            authorizer: cfg.authorizer.unwrap_or_else(|| Arc::new(AlwaysAllow)),
            kubelet_tls: cfg.kubelet_tls,
            extra_paths: cfg.extra_paths,
            ready: AtomicBool::new(false),
        })
    }

    pub fn ctx(&self) -> StrategyContext<'_> {
        StrategyContext {
            storage: self.storage.as_ref(),
            registry: &self.registry,
            authorizer: self.authorizer.as_ref(),
            authenticator: self.authenticator.as_deref(),
        }
    }

    /// Runs every strategy's `restore` hook with the stored objects
    /// (rebuilds allocators after a restart), then marks the server ready.
    pub async fn restore(&self) -> Result<(), ApiError> {
        for info in self.registry.resources() {
            let (items, _) = self.storage.list(&info.prefix("")).await.map_err(|e| ApiError::internal(e.to_string()))?;
            info.strategy.restore(&items).await;
        }
        Ok(())
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// Create an object on behalf of the server itself (bootstrap).
    pub async fn create_internal(
        self: &Arc<Self>,
        group: &str,
        version: &str,
        plural: &str,
        ns: &str,
        obj: Value,
    ) -> Result<Value, ApiError> {
        let info = self.registry.lookup(group, version, plural).ok_or_else(|| ApiError::internal(format!("unknown resource {plural}")))?;
        crate::handlers::create_object(self, &info, ns, obj, false).await
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new().fallback(dispatch).with_state(self.clone())
    }

    /// Serves HTTP until `shutdown` resolves.
    pub async fn serve(
        self: Arc<Self>,
        addr: SocketAddr,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        self.serve_listener(listener, shutdown).await
    }

    /// Serves on an already-bound listener (used by tests with port 0).
    pub async fn serve_listener(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        tracing::info!("serving on http://{}", listener.local_addr()?);
        let signaled = Arc::new(tokio::sync::Notify::new());
        let notify = signaled.clone();
        let graceful = axum::serve(listener, self.router()).with_graceful_shutdown(async move {
            shutdown.await;
            notify.notify_one();
        });
        // In-flight requests get a short drain period; long-running watches are then
        // cut so clients reconnect to the next server instead of a stale one.
        tokio::select! {
            r = graceful => r,
            _ = async {
                signaled.notified().await;
                tokio::time::sleep(SHUTDOWN_DRAIN).await;
            } => {
                tracing::info!("drain period over, closing remaining (watch) connections");
                Ok(())
            }
        }
    }
}

impl ApiServer {
    /// Serves HTTPS until `shutdown` resolves. Client certificates (if any) are
    /// verified by `tls` and handed to the authenticators.
    pub async fn serve_tls(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
        tls: rustls::ServerConfig,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        tracing::info!("serving securely on https://{}", listener.local_addr()?);
        serve_router_tls(self.router(), listener, tls, shutdown).await
    }
}

/// Serves `router` over HTTPS (HTTP/1.1 with upgrades) until `shutdown` resolves.
/// Each request carries the connection's verified client certificates as [`PeerCerts`].
/// Shared by the API server and the kubelet.
pub async fn serve_router_tls(
    router: Router,
    listener: tokio::net::TcpListener,
    tls: rustls::ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use tower::ServiceExt;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::pin!(shutdown);
    loop {
        let (tcp, _peer) = tokio::select! {
            _ = &mut shutdown => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    continue;
                }
            },
        };
        let (acceptor, router, mut stop) = (acceptor.clone(), router.clone(), stop_rx.clone());
        tokio::spawn(async move {
            let stream = match tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!("TLS handshake failed: {e}");
                    return;
                }
                Err(_) => return,
            };
            let certs = PeerCerts(
                stream.get_ref().1.peer_certificates().map(|c| Arc::new(c.iter().map(|c| c.clone().into_owned()).collect::<Vec<_>>())),
            );
            let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let mut req = req.map(axum::body::Body::new);
                req.extensions_mut().insert(certs.clone());
                router.clone().oneshot(req)
            });
            let conn =
                hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream), svc).with_upgrades();
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = stop.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(SHUTDOWN_DRAIN, conn).await;
                }
            }
        });
    }
    tracing::info!("shutdown signal received, draining connections");
    let _ = stop_tx.send(true);
    tokio::time::sleep(SHUTDOWN_DRAIN).await;
    Ok(())
}

/// How long shutdown waits for in-flight requests (`--shutdown-delay-duration`-like).
pub const SHUTDOWN_DRAIN: std::time::Duration = std::time::Duration::from_secs(3);

/// `WithAuthentication` → `WithImpersonation` → `WithAuthorization`.
async fn authn_authz(s: &Arc<ApiServer>, parts: &axum::http::request::Parts, q: &Query, websocket: bool) -> Result<UserInfo, ApiError> {
    let user = match &s.authenticator {
        None => UserInfo::insecure_admin(),
        Some(chain) => {
            let certs = parts.extensions.get::<PeerCerts>().and_then(|p| p.0.clone());
            let req = AuthRequest { headers: &parts.headers, peer_certs: certs.as_deref().map(Vec::as_slice) };
            chain.authenticate(&req).await.map_err(|e| {
                tracing::debug!("authentication failed: {e}");
                ApiError::unauthorized("Unauthorized")
            })?
        }
    };
    let user = crate::impersonation::impersonate(s.authorizer.as_ref(), user, &parts.headers).await?;
    let attrs = request_attributes(&user, &parts.method, parts.uri.path(), q, websocket);
    match s.authorizer.authorize(&attrs).await {
        Decision::Allow(_) => Ok(user),
        Decision::Deny(r) | Decision::NoOpinion(r) => {
            tracing::debug!(user = %user.name, verb = %attrs.verb, resource = %attrs.resource_with_sub(), path = %attrs.path, "forbidden");
            Err(forbidden(&attrs, &r))
        }
    }
}

async fn dispatch(State(s): State<Arc<ApiServer>>, req: axum::extract::Request) -> Response {
    let start = Instant::now();
    let (mut parts, body) = req.into_parts();
    let (method, uri, headers) = (parts.method.clone(), parts.uri.clone(), parts.headers.clone());
    let path = uri.path().to_string();
    let q = Query::parse(uri.query());
    let watching = q.flag("watch");
    let is_upgrade = headers.get("upgrade").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let user = match authn_authz(&s, &parts, &q, is_upgrade).await {
        Ok(u) => u,
        Err(e) => {
            tracing::info!(verb = %method, uri = %uri, status = e.code, latency = ?start.elapsed(), "HTTP");
            return error_response(&e);
        }
    };
    let username = user.name.clone();
    let resp = CURRENT_USER
        .scope(user, async {
            if is_upgrade {
                match upgrade(&s, &mut parts, &path, &q).await {
                    Ok(r) => r,
                    Err(e) => error_response(&e),
                }
            } else {
                match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
                    Ok(body) => match route(&s, &method, &path, &q, &headers, body).await {
                        Ok(r) => r,
                        Err(e) => error_response(&e),
                    },
                    Err(e) => error_response(&ApiError::bad_request(format!("reading body: {e}"))),
                }
            }
        })
        .await;
    let ua = headers.get("user-agent").and_then(|h| h.to_str().ok()).unwrap_or("");
    let level_info = !(path.ends_with("z") || method == Method::GET && !watching);
    if level_info {
        tracing::info!(verb = %method, uri = %uri, status = resp.status().as_u16(), latency = ?start.elapsed(), user = %username, user_agent = ua, "HTTP");
    } else {
        tracing::debug!(verb = %method, uri = %uri, status = resp.status().as_u16(), latency = ?start.elapsed(), user = %username, user_agent = ua, "HTTP");
    }
    resp
}

/// WebSocket upgrades: `pods/exec` and `pods/attach` are relayed to the kubelet.
async fn upgrade(s: &Arc<ApiServer>, parts: &mut axum::http::request::Parts, path: &str, q: &Query) -> Result<Response, ApiError> {
    use axum::extract::FromRequestParts;
    let RequestTarget::Resource(req) = parse_path(path) else {
        return Err(ApiError::bad_request("upgrade requests are only supported for pods/exec and pods/attach"));
    };
    let (Some(name), Some(sub)) = (req.name.as_deref(), req.subresource.as_deref()) else {
        return Err(ApiError::bad_request("upgrade requests need a subresource"));
    };
    if sub != "exec" && sub != "attach" {
        return Err(ApiError::bad_request(format!("upgrade is not supported for {sub}")));
    }
    let info = s
        .registry
        .lookup(&req.group, &req.version, &req.resource)
        .ok_or_else(|| ApiError::new(404, "NotFound", "the server could not find the requested resource"))?;
    let url = crate::handlers::connect_url(s, &info, &req.namespace, name, sub, q).await?;
    let ws = axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, &())
        .await
        .map_err(|e| ApiError::bad_request(format!("invalid websocket upgrade: {e}")))?;
    Ok(crate::proxy::proxy_websocket(ws, url, s.kubelet_tls.clone()))
}

async fn route(s: &Arc<ApiServer>, method: &Method, path: &str, q: &Query, headers: &HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    match path {
        "/healthz" | "/livez" => return Ok("ok".into_response()),
        "/readyz" => {
            return Ok(if s.ready.load(Ordering::SeqCst) {
                "ok".into_response()
            } else {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "[-]bootstrap not finished\nreadyz check failed").into_response()
            });
        }
        "/version" => return Ok(json_response(200, &s.version)),
        p if s.extra_paths.iter().any(|(x, _)| x == p) => {
            let doc = s.extra_paths.iter().find(|(x, _)| x == p).map(|(_, v)| v.clone()).unwrap_or_default();
            return Ok(json_response(200, &doc));
        }
        "/openapi/v3" => {
            let hash = s.version.get("gitVersion").and_then(Value::as_str).unwrap_or("0").replace(['+', '.'], "-");
            return Ok(json_response(200, &openapi::root(&s.registry, &hash)));
        }
        "/" => {
            let mut paths = vec![
                "/api".to_string(),
                "/api/v1".into(),
                "/apis".into(),
                "/healthz".into(),
                "/livez".into(),
                "/readyz".into(),
                "/version".into(),
            ];
            for (g, vs) in s.registry.groups() {
                if !g.is_empty() {
                    paths.push(format!("/apis/{g}"));
                    paths.extend(vs.iter().map(|v| format!("/apis/{g}/{v}")));
                }
            }
            return Ok(json_response(200, &json!({"paths": paths})));
        }
        _ => {}
    }
    let not_found = || ApiError::new(404, "NotFound", "the server could not find the requested resource");
    if let Some(gv_path) = path.strip_prefix("/openapi/v3/") {
        let ver = s.version.get("gitVersion").and_then(Value::as_str).unwrap_or("");
        return openapi::group_version(&s.registry, gv_path, ver).map(|v| json_response(200, &v)).ok_or_else(not_found);
    }
    match parse_path(path) {
        RequestTarget::ApiVersions => Ok(json_response(200, &discovery::api_versions(&s.advertise_address))),
        RequestTarget::ApiGroupList => Ok(json_response(200, &discovery::api_group_list(&s.registry))),
        RequestTarget::ApiGroup(g) => discovery::api_group(&s.registry, &g).map(|v| json_response(200, &v)).ok_or_else(not_found),
        RequestTarget::ResourceList { group, version } => {
            discovery::api_resource_list(&s.registry, &group, &version).map(|v| json_response(200, &v)).ok_or_else(not_found)
        }
        RequestTarget::Resource(req) => handle_resource(s, method, req, q, headers, body).await,
        RequestTarget::NotFound => Err(not_found()),
    }
}
