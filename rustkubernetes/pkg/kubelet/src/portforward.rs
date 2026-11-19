//! Port forwarding over SPDY (`cri-streaming/pkg/streaming/portforward/httpstream.go`).
//!
//! `POST /portForward/{namespace}/{pod}` is upgraded to SPDY/3.1 with the
//! `portforward.k8s.io` protocol. For every forwarded connection the client
//! opens two streams with the headers `streamType` (`error` / `data`), `port`
//! and `requestID`; once both arrived, the data stream is copied to
//! `localhost:port` in the pod (`Runtime::port_forward`). A failure is written
//! to the error stream (`error forwarding port P to pod SANDBOX, uid : …`) and
//! both streams are reset; a pair that is not complete within 30s gets
//! `timed out waiting for streams`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use spdystream::{Connection, Stream, StreamWriter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::runtime::SandboxState;
use crate::KubeletCtx;

/// `ProtocolV1Name`
pub const PROTOCOL_V1: &str = "portforward.k8s.io";
pub const HEADER_PROTOCOL_VERSION: &str = "X-Stream-Protocol-Version";
pub const HEADER_ACCEPTED_PROTOCOL_VERSIONS: &str = "X-Accepted-Stream-Protocol-Versions";
/// `DefaultStreamCreationTimeout`
const STREAM_CREATION_TIMEOUT: Duration = Duration::from_secs(30);
/// `--streaming-connection-idle-timeout` default.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Default)]
struct Pair {
    data: Option<Stream>,
    error: Option<StreamWriter>,
}

/// `httpStreamReceived`: required headers and their values.
fn check_stream(s: &Stream) -> Result<(), String> {
    let port = s.header("port").filter(|p| !p.is_empty()).ok_or("\"port\" header is required")?;
    let n: u64 = port.parse().map_err(|e| format!("unable to parse {port:?} as a port: {e}"))?;
    if n > u16::MAX as u64 {
        return Err(format!("unable to parse {port:?} as a port: value out of range"));
    }
    if n < 1 {
        return Err(format!("port {port:?} must be > 0"));
    }
    match s.header("streamType").filter(|t| !t.is_empty()) {
        None => Err("\"streamType\" header is required".into()),
        Some("error" | "data") => Ok(()),
        Some(t) => Err(format!("invalid stream type {t:?}")),
    }
}

/// `requestID` header, or (old clients) derived from the stream id.
fn request_id(s: &Stream) -> String {
    match s.header("requestID").filter(|r| !r.is_empty()) {
        Some(r) => r.to_string(),
        None => match s.header("streamType") {
            Some("data") => (s.id as i64 - 2).to_string(),
            _ => s.id.to_string(),
        },
    }
}

/// Serves one upgraded connection for the pod `uid` until the client goes away.
pub async fn serve<T>(ctx: Arc<KubeletCtx>, pod_uid: String, io: T)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (conn, mut incoming) = Connection::server(io);
    let pairs: Arc<Mutex<HashMap<String, Pair>>> = Arc::default();
    loop {
        let next = tokio::select! {
            s = incoming.recv() => s,
            _ = tokio::time::sleep(IDLE_TIMEOUT) => None,
        };
        let Some(stream) = next else { break };
        if let Err(e) = check_stream(&stream) {
            tracing::warn!("Stream rejected: {e}");
            let _ = stream.reset();
            continue;
        }
        let _ = stream.reply(vec![]);
        let rid = request_id(&stream);
        let is_error = stream.header("streamType") == Some("error");
        let mut map = pairs.lock().unwrap();
        let created = !map.contains_key(&rid);
        let pair = map.entry(rid.clone()).or_default();
        let dup = if is_error { pair.error.is_some() } else { pair.data.is_some() };
        if dup {
            let msg =
                format!("error processing stream for request {rid}: {} stream already assigned", if is_error { "error" } else { "data" });
            tracing::warn!("{msg}");
            if let Some(e) = &pair.error {
                let _ = e.write(Bytes::from(msg));
            }
            continue;
        }
        if is_error {
            pair.error = Some(stream.writer());
            // Keep the error stream's reader alive; the client half-closes it.
            tokio::spawn(async move {
                let mut s = stream;
                while s.read().await.is_some() {}
            });
        } else {
            pair.data = Some(stream);
        }
        if pair.error.is_some() && pair.data.is_some() {
            let p = map.remove(&rid).unwrap();
            tokio::spawn(forward(ctx.clone(), pod_uid.clone(), p));
        } else if created {
            let pairs = pairs.clone();
            tokio::spawn(async move {
                tokio::time::sleep(STREAM_CREATION_TIMEOUT).await;
                if let Some(p) = pairs.lock().unwrap().remove(&rid) {
                    let msg = format!("(conn=spdy, request={rid}) timed out waiting for streams");
                    tracing::warn!("{msg}");
                    if let Some(e) = p.error {
                        let _ = e.write(Bytes::from(msg));
                    }
                }
            });
        }
    }
    conn.close();
}

/// `httpStreamHandler.portForward`
async fn forward(ctx: Arc<KubeletCtx>, pod_uid: String, p: Pair) {
    let (Some(data), Some(error)) = (p.data, p.error) else { return };
    let port: u16 = data.header("port").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (sandboxes, _) = ctx.cache.for_pod(&pod_uid);
    let sandbox = sandboxes.into_iter().filter(|s| s.state == SandboxState::Ready).max_by_key(|s| s.attempt);
    let (mut reader, writer) = data.into_split();
    let result = match &sandbox {
        None => Err((String::new(), anyhow::anyhow!("failed to find sandbox for pod {pod_uid}"))),
        Some(sb) => {
            let (local, remote) = tokio::io::duplex(64 * 1024);
            let (mut l_rd, mut l_wr) = tokio::io::split(local);
            // Client → pod.
            let inbound = tokio::spawn(async move {
                while let Some(chunk) = reader.read().await {
                    if l_wr.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
                let _ = l_wr.shutdown().await;
            });
            // Pod → client.
            let w = writer.clone();
            let outbound = tokio::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match l_rd.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if w.write(Bytes::copy_from_slice(&buf[..n])).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let r = ctx.runtime.port_forward(&sb.id, port, remote).await;
            let _ = outbound.await;
            inbound.abort();
            r.map_err(|e| (sb.id.clone(), e))
        }
    };
    match result {
        Ok(()) => {
            let _ = writer.close();
            let _ = error.close();
        }
        Err((sandbox_id, e)) => {
            // The CRI streaming server names the pod by its sandbox id and has no uid.
            let msg = format!("error forwarding port {port} to pod {sandbox_id}, uid : {e:#}");
            tracing::warn!("{msg}");
            let _ = error.write(Bytes::from(msg));
            let _ = writer.reset();
            let _ = error.reset();
        }
    }
}
