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

// ---------------------------------------------------------------------------
// SPDY upgrades (pods/portforward): `UpgradeAwareHandler` and `TunnelingHandler`
// ---------------------------------------------------------------------------

/// `WebsocketsSPDYTunnelingPrefix`
pub const SPDY_TUNNELING_PREFIX: &str = "SPDY/3.1+";
const KUBERNETES_SUFFIX: &str = ".k8s.io";

/// The SPDY protocols carried in a WebSocket subprotocol list (`spdyProtocolsFromWebsocketProtocols`).
pub fn spdy_protocols_from_websocket(protocols: &[String]) -> Vec<String> {
    protocols
        .iter()
        .filter_map(|p| p.strip_prefix(SPDY_TUNNELING_PREFIX).filter(|p| p.ends_with(KUBERNETES_SUFFIX)).map(String::from))
        .collect()
}

/// An upgraded backend connection.
pub struct Upgraded {
    pub io: Box<dyn Io>,
    /// `X-Stream-Protocol-Version` the backend chose.
    pub protocol: String,
    /// Bytes read past the response headers.
    pub leftover: Vec<u8>,
}

/// Opens `http_url` on the backend with `Upgrade: SPDY/3.1` (`createSPDYRequest`, POST).
/// A non-101 answer becomes `Err((status, body))`.
pub async fn dial_spdy(http_url: &str, protocols: &[String], tls: Option<Arc<rustls::ClientConfig>>) -> Result<Upgraded, (u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let internal = |e: String| (500u16, format!("error dialing backend: {e}"));
    let uri: axum::http::Uri = http_url.parse().map_err(|e| internal(format!("{e}")))?;
    let secure = uri.scheme_str() == Some("https");
    let host = uri.host().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_string();
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|e| internal(e.to_string()))?;
    let mut io: Box<dyn Io> = if secure {
        let cfg = tls.ok_or_else(|| internal("no TLS configuration for an https kubelet".into()))?;
        let name = rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| internal(e.to_string()))?;
        Box::new(tokio_rustls::TlsConnector::from(cfg).connect(name, tcp).await.map_err(|e| internal(e.to_string()))?)
    } else {
        Box::new(tcp)
    };
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let host_hdr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("POST {path} HTTP/1.1\r\nHost: {host_hdr}\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n");
    for p in protocols {
        req.push_str(&format!("X-Stream-Protocol-Version: {p}\r\n"));
    }
    req.push_str("Content-Length: 0\r\n\r\n");
    io.write_all(req.as_bytes()).await.map_err(|e| internal(e.to_string()))?;
    // Response head.
    let mut buf = Vec::with_capacity(1024);
    let end = loop {
        let mut chunk = [0u8; 1024];
        let n = io.read(&mut chunk).await.map_err(|e| internal(e.to_string()))?;
        if n == 0 {
            return Err(internal("backend closed the connection".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(internal("response headers too large".into()));
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(502);
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let hdr = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut leftover = buf[end..].to_vec();
    if status != 101 {
        let len = hdr("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(64 * 1024).min(64 * 1024);
        while leftover.len() < len {
            let mut chunk = [0u8; 4096];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&chunk[..n]),
            }
        }
        return Err((status, String::from_utf8_lossy(&leftover).trim().to_string()));
    }
    Ok(Upgraded { io, protocol: hdr("x-stream-protocol-version").unwrap_or_default(), leftover })
}

/// `TunnelingHandler`: kubectl speaks SPDY inside WebSocket binary messages; the
/// bytes are relayed unchanged to the backend's SPDY connection.
pub fn tunnel_websocket(ws: WebSocketUpgrade, backend: Upgraded) -> Response {
    let protocol = format!("{SPDY_TUNNELING_PREFIX}{}", backend.protocol);
    ws.protocols([protocol])
        .on_upgrade(move |client| async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut c_tx, mut c_rx) = client.split();
            let (mut b_rd, mut b_wr) = tokio::io::split(backend.io);
            if !backend.leftover.is_empty() && c_tx.send(AxMessage::Binary(backend.leftover.into())).await.is_err() {
                return;
            }
            let up = async {
                while let Some(Ok(m)) = c_rx.next().await {
                    match m {
                        AxMessage::Binary(b) => {
                            if b_wr.write_all(&b).await.is_err() {
                                break;
                            }
                        }
                        AxMessage::Close(_) => break,
                        _ => {}
                    }
                }
                let _ = b_wr.shutdown().await;
            };
            let down = async {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match b_rd.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if c_tx.send(AxMessage::Binary(buf[..n].to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = c_tx.send(AxMessage::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "".into() }))).await;
            };
            tokio::select! {
                _ = up => {},
                _ = down => {},
            }
        })
        .into_response()
}

/// `UpgradeAwareHandler` for a client that upgrades to SPDY itself: answers 101 with
/// the backend's protocol, then copies bytes both ways.
pub fn passthrough_spdy(on_upgrade: hyper::upgrade::OnUpgrade, backend: Upgraded) -> Response {
    use tokio::io::AsyncWriteExt;
    let protocol = backend.protocol.clone();
    tokio::spawn(async move {
        let Ok(up) = on_upgrade.await else { return };
        let mut client = hyper_util::rt::TokioIo::new(up);
        let mut b = backend.io;
        if client.write_all(&backend.leftover).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut b).await;
    });
    let mut r = Response::builder().status(101).header("Connection", "Upgrade").header("Upgrade", "SPDY/3.1");
    if !protocol.is_empty() {
        r = r.header("X-Stream-Protocol-Version", protocol);
    }
    r.body(Body::empty()).unwrap()
}
