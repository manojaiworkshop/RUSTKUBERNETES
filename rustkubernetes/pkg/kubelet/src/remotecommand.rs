//! Exec and attach over SPDY (`cri-streaming/pkg/streaming/remotecommand/httpstream.go`),
//! for clients that do not use the WebSocket protocol (kubectl before 1.29, or
//! `KUBECTL_REMOTE_COMMAND_WEBSOCKETS=false`).
//!
//! After the upgrade the client opens one stream per channel, named by the
//! `streamType` header: `error`, `stdin`, `stdout`, `stderr` and (with a TTY,
//! v4) `resize`. Once all expected streams arrived the command runs; the final
//! status goes to the error stream (JSON `Status` in v4, plain text before).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use spdystream::{Connection, StreamReader, StreamWriter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::dockershim::client::Demuxer;
use crate::runtime::ExecSession;
use crate::KubeletCtx;

pub const STREAM_PROTOCOL_V1: &str = "channel.k8s.io";
pub const STREAM_PROTOCOL_V2: &str = "v2.channel.k8s.io";
pub const STREAM_PROTOCOL_V3: &str = "v3.channel.k8s.io";
pub const STREAM_PROTOCOL_V4: &str = "v4.channel.k8s.io";
/// `SupportedStreamingProtocols` over SPDY, preferred first.
pub const SPDY_PROTOCOLS: [&str; 4] = [STREAM_PROTOCOL_V4, STREAM_PROTOCOL_V3, STREAM_PROTOCOL_V2, STREAM_PROTOCOL_V1];
const STREAM_CREATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `remotecommand.Options` from the kubelet's query parameters (`NewOptions`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub tty: bool,
}

impl Options {
    pub fn from_query(pairs: &[(String, String)]) -> Result<Self, String> {
        let get = |k: &str| pairs.iter().any(|(kk, v)| kk == k && v == "1");
        let mut o = Options { stdin: get("input"), stdout: get("output"), stderr: get("error"), tty: get("tty") };
        if o.tty && o.stderr {
            // Access to exec with tty and stderr is not supported, bypassing stderr.
            o.stderr = false;
        }
        if !o.stdin && !o.stdout && !o.stderr {
            return Err("you must specify at least 1 of stdin, stdout, stderr".into());
        }
        Ok(o)
    }
}

/// What runs once the streams are there.
pub enum Target {
    Exec { container_id: String, cmd: Vec<String> },
    Attach { container_id: String },
}

#[derive(Default)]
struct Streams {
    error: Option<StreamWriter>,
    stdin: Option<StreamReader>,
    stdout: Option<StreamWriter>,
    stderr: Option<StreamWriter>,
    resize: Option<StreamReader>,
}

/// Serves one upgraded SPDY connection (`createHTTPStreamStreams` + `ServeExec` / `ServeAttach`).
pub async fn serve<T>(ctx: Arc<KubeletCtx>, protocol: &'static str, opts: Options, target: Target, io: T)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (conn, mut incoming) = Connection::server(io);
    let mut expected = 1 + opts.stdin as usize + opts.stdout as usize + opts.stderr as usize;
    if opts.tty && protocol != STREAM_PROTOCOL_V1 && protocol != STREAM_PROTOCOL_V2 {
        expected += 1;
    }
    let mut st = Streams::default();
    let mut received = 0;
    let deadline = tokio::time::sleep(STREAM_CREATION_TIMEOUT);
    tokio::pin!(deadline);
    while received < expected {
        tokio::select! {
            s = incoming.recv() => {
                let Some(s) = s else { return };
                let _ = s.reply(vec![]);
                let kind = s.header("streamType").unwrap_or("").to_string();
                let w = s.writer();
                let (r, _) = s.into_split();
                match kind.as_str() {
                    "error" => st.error = Some(w),
                    "stdin" => st.stdin = Some(r),
                    "stdout" => st.stdout = Some(w),
                    "stderr" => st.stderr = Some(w),
                    "resize" => st.resize = Some(r),
                    other => {
                        tracing::warn!("unexpected stream type: {other:?}");
                        continue;
                    }
                }
                received += 1;
            }
            _ = &mut deadline => {
                tracing::warn!("timed out waiting for client to create streams");
                conn.close();
                return;
            }
        }
    }
    let rt = ctx.runtime.clone();
    let (sess, cmd, attach) = match &target {
        Target::Exec { container_id, cmd } => (rt.exec_stream(container_id, cmd, opts.tty, opts.stdin).await, cmd.clone(), false),
        Target::Attach { container_id } => (rt.attach(container_id, opts.stdin, opts.tty).await, vec![], true),
    };
    let status = match sess {
        Err(e) => Some(StatusOut::Internal(format!("error executing command in container: {e}"))),
        Ok(sess) => run(&ctx, sess, st.stdin.take(), st.stdout.clone(), st.stderr.clone(), st.resize.take(), opts.tty, attach).await,
    };
    if let Some(w) = &st.error {
        let body = match (protocol, status) {
            (STREAM_PROTOCOL_V4, s) => Some(s.unwrap_or(StatusOut::Success).json(&cmd)),
            (_, None | Some(StatusOut::Success)) => None,
            (_, Some(s)) => Some(s.message(&cmd)),
        };
        if let Some(b) = body {
            let _ = w.write(Bytes::from(b));
        }
    }
    for w in [st.stdout, st.stderr, st.error].into_iter().flatten() {
        let _ = w.close();
    }
    conn.close();
}

enum StatusOut {
    Success,
    ExitCode(i32),
    Internal(String),
}

impl StatusOut {
    fn message(&self, cmd: &[String]) -> String {
        match self {
            StatusOut::Success => String::new(),
            StatusOut::ExitCode(c) => {
                format!("command terminated with non-zero exit code: error executing command {cmd:?}, exit code {c}")
            }
            StatusOut::Internal(m) => m.clone(),
        }
    }

    fn json(&self, cmd: &[String]) -> String {
        let v = match self {
            StatusOut::Success => serde_json::json!({"metadata": {}, "status": "Success"}),
            StatusOut::ExitCode(c) => serde_json::json!({
                "metadata": {}, "status": "Failure", "message": self.message(cmd), "reason": "NonZeroExitCode",
                "details": {"causes": [{"reason": "ExitCode", "message": c.to_string()}]},
            }),
            StatusOut::Internal(m) => serde_json::json!({
                "metadata": {}, "status": "Failure", "message": format!("Internal error occurred: {m}"),
                "reason": "InternalError", "details": {"causes": [{"message": m}]}, "code": 500,
            }),
        };
        v.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    ctx: &Arc<KubeletCtx>,
    sess: ExecSession,
    stdin: Option<StreamReader>,
    stdout: Option<StreamWriter>,
    stderr: Option<StreamWriter>,
    resize: Option<StreamReader>,
    tty: bool,
    attach: bool,
) -> Option<StatusOut> {
    let exec_id = sess.exec_id.clone();
    let (mut rd, mut wr) = tokio::io::split(sess.io);
    let input = tokio::spawn(async move {
        if let Some(mut r) = stdin {
            while let Some(d) = r.read().await {
                if wr.write_all(&d).await.is_err() {
                    return;
                }
            }
            let _ = wr.shutdown().await;
        }
    });
    let resizer = {
        let rt = ctx.runtime.clone();
        let id = exec_id.clone();
        tokio::spawn(async move {
            let Some(mut r) = resize else { return };
            let mut buf = Vec::new();
            while let Some(d) = r.read().await {
                buf.extend_from_slice(&d);
                // A stream of JSON objects: {"Width":80,"Height":24}
                let mut it = serde_json::Deserializer::from_slice(&buf).into_iter::<serde_json::Value>();
                let mut used = 0;
                while let Some(Ok(v)) = it.next() {
                    used = it.byte_offset();
                    let w = v["Width"].as_u64().unwrap_or(80) as u16;
                    let h = v["Height"].as_u64().unwrap_or(24) as u16;
                    let _ = if attach { rt.resize_container(&id, w, h).await } else { rt.exec_resize(&id, w, h).await };
                }
                buf.drain(..used);
            }
        })
    };
    let mut demux = Demuxer::default();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunks = if tty { vec![(1u8, buf[..n].to_vec())] } else { demux.push(&buf[..n]) };
                for (stream, data) in chunks {
                    let w = if stream == 2 { stderr.as_ref() } else { stdout.as_ref() };
                    if let Some(w) = w {
                        let _ = w.write(Bytes::from(data));
                    }
                }
            }
        }
    }
    input.abort();
    resizer.abort();
    if attach {
        return None;
    }
    for _ in 0..20 {
        match ctx.runtime.exec_exit_code(&exec_id).await {
            Ok(Some(0)) => return None,
            Ok(Some(c)) => return Some(StatusOut::ExitCode(c)),
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(_) => break,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &[(&str, &str)]) -> Vec<(String, String)> {
        s.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn options() {
        assert_eq!(
            Options::from_query(&q(&[("input", "1"), ("output", "1"), ("error", "1"), ("tty", "1")])),
            Ok(Options { stdin: true, stdout: true, stderr: false, tty: true })
        );
        assert!(Options::from_query(&q(&[("tty", "1")])).unwrap_err().contains("at least 1 of stdin, stdout, stderr"));
    }
}
