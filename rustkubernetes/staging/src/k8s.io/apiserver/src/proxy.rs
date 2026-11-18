//! Proxying `CONNECT`-style subresources to a backend (`registry/generic/rest/proxy.go`,
//! `UpgradeAwareHandler`): streamed HTTP for `pods/log`, a WebSocket relay for
//! `pods/exec` and `pods/attach`.

use axum::body::Body;
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use k8s_apimachinery::errors::ApiError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;

/// Subprotocols the kubelet speaks for exec/attach.
pub const STREAM_PROTOCOLS: [&str; 2] = ["v5.channel.k8s.io", "v4.channel.k8s.io"];

/// Streams a GET response from the backend (follows `kubectl logs -f`).
/// `tls` authenticates the API server to an `https://` kubelet.
pub async fn proxy_http(url: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Result<Response, ApiError> {
    k8s_client_go::util::cert::install_crypto_provider();
    let mut b = reqwest::Client::builder();
    if let (true, Some(t)) = (url.starts_with("https://"), tls) {
        b = b.tls_backend_preconfigured((*t).clone());
    }
    let client = b.build().map_err(|e| ApiError::internal(format!("kubelet client: {e}")))?;
    let resp = client.get(url).send().await.map_err(|e| ApiError::internal(format!("error dialing backend: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_default();
        return Err(ApiError::new(status.as_u16(), "BadRequest", msg.trim().to_string()));
    }
    let body = Body::from_stream(resp.bytes_stream());
    Ok(Response::builder().status(200).header("Content-Type", "text/plain").body(body).unwrap())
}

fn to_backend(m: AxMessage) -> Option<TMessage> {
    Some(match m {
        AxMessage::Binary(b) => TMessage::Binary(b),
        AxMessage::Text(t) => TMessage::Text(t.as_str().into()),
        AxMessage::Ping(p) => TMessage::Ping(p),
        AxMessage::Pong(p) => TMessage::Pong(p),
        AxMessage::Close(c) => TMessage::Close(
            c.map(|c| tokio_tungstenite::tungstenite::protocol::CloseFrame { code: c.code.into(), reason: c.reason.as_str().into() }),
        ),
    })
}

fn to_client(m: TMessage) -> Option<AxMessage> {
    Some(match m {
        TMessage::Binary(b) => AxMessage::Binary(b),
        TMessage::Text(t) => AxMessage::Text(t.as_str().into()),
        TMessage::Ping(p) => AxMessage::Ping(p),
        TMessage::Pong(p) => AxMessage::Pong(p),
        TMessage::Close(c) => AxMessage::Close(Some(axum::extract::ws::CloseFrame {
            code: c.as_ref().map(|c| u16::from(c.code)).unwrap_or(1000),
            reason: c.map(|c| c.reason.as_str().into()).unwrap_or_default(),
        })),
        TMessage::Frame(_) => return None,
    })
}

/// Accepts the client's WebSocket and relays every frame to/from the backend.
pub fn proxy_websocket(ws: WebSocketUpgrade, http_url: String, tls: Option<Arc<rustls::ClientConfig>>) -> Response {
    ws.protocols(STREAM_PROTOCOLS)
        .on_upgrade(move |client| async move {
            let protocol = client.protocol().and_then(|p| p.to_str().ok()).unwrap_or(STREAM_PROTOCOLS[0]).to_string();
            if let Err(e) = relay(client, &http_url, &protocol, tls).await {
                tracing::warn!("exec proxy: {e}");
            }
        })
        .into_response()
}

/// A plain or TLS byte stream.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Opens the backend WebSocket, over TLS for `https://` URLs.
async fn connect_backend(
    http_url: &str,
    protocol: &str,
    tls: Option<Arc<rustls::ClientConfig>>,
) -> Result<tokio_tungstenite::WebSocketStream<Box<dyn Io>>, String> {
    let secure = http_url.starts_with("https://");
    let url = if secure { http_url.replacen("https://", "wss://", 1) } else { http_url.replacen("http://", "ws://", 1) };
    let mut req = url.as_str().into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert("Sec-WebSocket-Protocol", protocol.parse().map_err(|_| "bad protocol")?);
    let host = req.uri().host().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_string();
    let port = req.uri().port_u16().unwrap_or(if secure { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|e| format!("error dialing backend: {e}"))?;
    let stream = if secure {
        let cfg = tls.ok_or("no TLS configuration for an https kubelet")?;
        let name = rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let s = tokio_rustls::TlsConnector::from(cfg).connect(name, tcp).await.map_err(|e| format!("error dialing backend: {e}"))?;
        Box::new(s) as Box<dyn Io>
    } else {
        Box::new(tcp)
    };
    let (ws, _) = tokio_tungstenite::client_async(req, stream).await.map_err(|e| format!("error dialing backend: {e}"))?;
    Ok(ws)
}

async fn relay(client: WebSocket, http_url: &str, protocol: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Result<(), String> {
    let backend = connect_backend(http_url, protocol, tls).await?;
    let (mut c_tx, mut c_rx) = client.split();
    let (mut b_tx, mut b_rx) = backend.split();
    let up = async {
        while let Some(Ok(m)) = c_rx.next().await {
            let Some(m) = to_backend(m) else { continue };
            let closing = m.is_close();
            if b_tx.send(m).await.is_err() || closing {
                break;
            }
        }
    };
    let down = async {
        let mut closed = false;
        while let Some(Ok(m)) = b_rx.next().await {
            let Some(m) = to_client(m) else { continue };
            closed = matches!(m, AxMessage::Close(_));
            if c_tx.send(m).await.is_err() || closed {
                break;
            }
        }
        if !closed {
            // Backend vanished without a close handshake: still end cleanly for the client.
            let _ = c_tx.send(AxMessage::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "".into() }))).await;
        }
    };
    // The session ends when the backend finishes (command exited).
    tokio::select! {
        _ = down => {},
        _ = up => {},
    }
    Ok(())
}
