//! Port of `client-go/rest`: connection config and a low-level REST client.

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use k8s_apimachinery::errors::{ApiError, Status};
pub use reqwest::Method;
use serde_json::Value;

use crate::tools::clientcmd;
use crate::util::cert::TlsClientConfig;

/// Where pods find their ServiceAccount credentials (`rest.InClusterConfig`).
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server returned a `Status` failure (404, 409, 422, ...).
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("http error: {0}")]
    Http(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("config error: {0}")]
    Config(String),
}

impl Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Api(e) if e.is_not_found())
    }
    pub fn is_conflict(&self) -> bool {
        matches!(self, Error::Api(e) if e.is_conflict())
    }
    pub fn is_already_exists(&self) -> bool {
        matches!(self, Error::Api(e) if e.is_already_exists())
    }
    pub fn is_gone(&self) -> bool {
        matches!(self, Error::Api(e) if e.code == 410)
    }
}

/// `rest.Config`
#[derive(Clone)]
pub struct Config {
    /// `https://127.0.0.1:6443` (or `http://` for an insecure dev server)
    pub server: String,
    pub bearer_token: Option<String>,
    /// Re-read on every request (projected ServiceAccount tokens rotate).
    pub bearer_token_file: Option<PathBuf>,
    pub tls: TlsClientConfig,
    /// `Impersonate-User` / `Impersonate-Group` (`rest.ImpersonationConfig`).
    pub impersonate_user: Option<String>,
    pub impersonate_groups: Vec<String>,
    pub user_agent: String,
    pub timeout: Duration,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("bearer_token_file", &self.bearer_token_file)
            .field("tls", &self.tls)
            .finish()
    }
}

impl Config {
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into().trim_end_matches('/').to_string(),
            bearer_token: None,
            bearer_token_file: None,
            tls: TlsClientConfig::default(),
            impersonate_user: None,
            impersonate_groups: vec![],
            user_agent: format!("rustkubernetes-client/{}", env!("CARGO_PKG_VERSION")),
            timeout: Duration::from_secs(30),
        }
    }

    /// Loads `$KUBECONFIG` or `~/.kube/config`, else the in-cluster config
    /// (`clientcmd.BuildConfigFromFlags("", "")`).
    pub fn infer() -> Result<Self, Error> {
        match clientcmd::default_kubeconfig_path() {
            Some(path) => clientcmd::load_config(&path, None),
            None => {
                Self::in_cluster().map_err(|e| Error::Config(format!("no kubeconfig found (set KUBECONFIG) and not in a cluster: {e}")))
            }
        }
    }

    /// `rest.InClusterConfig`: the `kubernetes` Service plus the pod's ServiceAccount token and CA.
    pub fn in_cluster() -> Result<Self, Error> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| Error::Config("KUBERNETES_SERVICE_HOST not set".into()))?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").map_err(|_| Error::Config("KUBERNETES_SERVICE_PORT not set".into()))?;
        let dir = PathBuf::from(SERVICE_ACCOUNT_DIR);
        let ca = std::fs::read(dir.join("ca.crt")).map_err(|e| Error::Config(format!("reading ca.crt: {e}")))?;
        let mut cfg = Self::new(format!("https://{host}:{port}"));
        cfg.tls.ca_data = Some(ca);
        cfg.bearer_token_file = Some(dir.join("token"));
        Ok(cfg)
    }

    /// The bearer token to send (file contents win over a static token).
    pub fn token(&self) -> Option<String> {
        if let Some(f) = &self.bearer_token_file {
            if let Ok(t) = std::fs::read_to_string(f) {
                return Some(t.trim().to_string());
            }
        }
        self.bearer_token.clone()
    }

    /// `reqwest` client builder with this config's TLS settings.
    pub fn http_builder(&self) -> Result<reqwest::ClientBuilder, Error> {
        // reqwest initialises rustls even for plain-HTTP clients.
        crate::util::cert::install_crypto_provider();
        let b = reqwest::Client::builder().user_agent(self.user_agent.clone());
        if self.server.starts_with("https://") {
            let tls = self.tls.rustls().map_err(Error::Config)?;
            Ok(b.tls_backend_preconfigured(tls))
        } else {
            Ok(b)
        }
    }
}

/// A cheap-to-clone HTTP client bound to one API server.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    stream_http: reqwest::Client,
    config: Config,
}

impl Client {
    pub fn new(config: Config) -> Result<Self, Error> {
        let http = config.http_builder()?.timeout(config.timeout).build().map_err(|e| Error::Config(e.to_string()))?;
        // Watches are long-running: no overall timeout.
        let stream_http = config.http_builder()?.build().map_err(|e| Error::Config(e.to_string()))?;
        Ok(Self { http, stream_http, config })
    }

    /// Client from `$KUBECONFIG` / `~/.kube/config`.
    pub fn try_default() -> Result<Self, Error> {
        Self::new(Config::infer()?)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The same connection acting as another user (requests carry `Impersonate-*`
    /// headers; the server checks the `impersonate` verb).
    pub fn impersonating(&self, user: &str, groups: &[String]) -> Self {
        let mut c = self.clone();
        c.config.impersonate_user = Some(user.to_string());
        c.config.impersonate_groups = groups.to_vec();
        c
    }

    /// Raw request with a custom body type (e.g. server-side apply `application/apply-patch+yaml`).
    pub async fn request_raw(&self, method: Method, path: &str, content_type: &str, body: Vec<u8>) -> Result<Value, Error> {
        self.request(method, path, Some((content_type, body))).await
    }

    /// Any request, returning status, content type and body unchanged (API passthrough).
    pub async fn raw(&self, method: Method, path: &str, content_type: Option<&str>, body: Vec<u8>) -> Result<(u16, String, Bytes), Error> {
        let mut b = self.builder_accept(&self.http, method, path, "application/json, */*");
        if let Some(ct) = content_type {
            b = b.header("Content-Type", ct).body(body);
        }
        let resp = b.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/json").to_string();
        let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
        Ok((status, ct, bytes))
    }

    /// GET returning the body as text (pod logs).
    pub async fn get_text(&self, path: &str) -> Result<String, Error> {
        let resp = self.builder_accept(&self.http, Method::GET, path, "*/*").send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Api(decode_error(status.as_u16(), &bytes)));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.server, path)
    }

    fn builder(&self, http: &reqwest::Client, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.builder_accept(http, method, path, "application/json")
    }

    fn builder_accept(&self, http: &reqwest::Client, method: Method, path: &str, accept: &str) -> reqwest::RequestBuilder {
        let mut b = http.request(method, self.url(path)).header("Accept", accept);
        if let Some(t) = self.config.token() {
            b = b.bearer_auth(t);
        }
        if let Some(u) = &self.config.impersonate_user {
            b = b.header("Impersonate-User", u);
            for g in &self.config.impersonate_groups {
                b = b.header("Impersonate-Group", g);
            }
        }
        b
    }

    /// Sends a request and decodes the JSON response; non-2xx become [`Error::Api`].
    pub async fn request(&self, method: Method, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<Value, Error> {
        let mut b = self.builder(&self.http, method, path);
        if let Some((ct, bytes)) = body {
            b = b.header("Content-Type", ct).body(bytes);
        }
        let resp = b.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Api(decode_error(status.as_u16(), &bytes)));
        }
        serde_json::from_slice(&bytes).map_err(|e| Error::Decode(format!("{e}: {}", String::from_utf8_lossy(&bytes))))
    }

    pub async fn get_json(&self, path: &str) -> Result<Value, Error> {
        self.request(Method::GET, path, None).await
    }

    /// GET with a custom `Accept` header (e.g. server-side Table printing).
    pub async fn get_with_accept(&self, path: &str, accept: &str) -> Result<Value, Error> {
        let resp = self.builder_accept(&self.http, Method::GET, path, accept).send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Api(decode_error(status.as_u16(), &bytes)));
        }
        serde_json::from_slice(&bytes).map_err(|e| Error::Decode(e.to_string()))
    }

    pub async fn send_json(&self, method: Method, path: &str, body: &Value) -> Result<Value, Error> {
        self.request(method, path, Some(("application/json", serde_json::to_vec(body).unwrap()))).await
    }

    /// Opens a watch and yields one JSON value per event line.
    pub async fn watch_lines(&self, path: &str) -> Result<impl Stream<Item = Result<Value, Error>> + use<>, Error> {
        let resp = self.builder(&self.stream_http, Method::GET, path).send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.bytes().await.unwrap_or_default();
            return Err(Error::Api(decode_error(status.as_u16(), &bytes)));
        }
        let mut body = resp.bytes_stream();
        Ok(async_stream::stream! {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk: Bytes = match chunk {
                    Ok(c) => c,
                    Err(e) => { yield Err(Error::Http(e.to_string())); break; }
                };
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = &line[..line.len() - 1];
                    if line.iter().all(u8::is_ascii_whitespace) { continue; }
                    yield serde_json::from_slice::<Value>(line).map_err(|e| Error::Decode(e.to_string()));
                }
            }
        })
    }
}

fn decode_error(code: u16, body: &[u8]) -> ApiError {
    match serde_json::from_slice::<Status>(body) {
        Ok(s) if s.kind.as_deref() == Some("Status") => ApiError::from_status(&s),
        _ => ApiError::new(code, "Unknown", String::from_utf8_lossy(body).trim().to_string()),
    }
}
