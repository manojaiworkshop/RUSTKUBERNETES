//! A minimal Docker Engine API client over the Unix socket (HTTP/1.1).
//!
//! Supports plain JSON requests, streamed responses (image pulls, logs) and
//! hijacked connections (`POST /exec/<id>/start` upgrades to a raw stream, which
//! is how interactive exec works).

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

/// API version prefix (Docker ≥ 25 supports 1.44).
const API: &str = "/v1.44";

#[derive(Clone, Debug)]
pub struct DockerClient {
    socket: PathBuf,
}

impl DockerClient {
    /// `endpoint` like `unix:///var/run/docker.sock` or a plain path.
    pub fn new(endpoint: &str) -> Self {
        Self { socket: PathBuf::from(endpoint.trim_start_matches("unix://")) }
    }

    /// Request with a raw body (e.g. `PUT /containers/{id}/archive` with a tar stream).
    pub async fn call_raw(&self, method: Method, path: &str, content_type: &str, body: Bytes) -> Result<()> {
        let stream =
            UnixStream::connect(&self.socket).await.with_context(|| format!("connecting to docker at {}", self.socket.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let req = Request::builder()
            .method(method)
            .uri(format!("http://docker{API}{path}"))
            .header("Host", "docker")
            .header("Content-Type", content_type)
            .body(Full::new(body))?;
        let (status, bytes) = Self::read_body(sender.send_request(req).await?).await?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v["message"].as_str().map(String::from));
            return Err(DockerError {
                status: status.as_u16(),
                message: msg.unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
            }
            .into());
        }
        Ok(())
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>, upgrade: bool) -> Result<Response<Incoming>> {
        let stream =
            UnixStream::connect(&self.socket).await.with_context(|| format!("connecting to docker at {}", self.socket.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                tracing::trace!("docker connection closed: {e}");
            }
        });
        let mut req = Request::builder().method(method).uri(format!("http://docker{API}{path}")).header("Host", "docker");
        if upgrade {
            req = req.header("Connection", "Upgrade").header("Upgrade", "tcp");
        }
        let body = match body {
            Some(v) => {
                req = req.header("Content-Type", "application/json");
                Full::new(Bytes::from(serde_json::to_vec(v)?))
            }
            None => Full::new(Bytes::new()),
        };
        Ok(sender.send_request(req.body(body)?).await?)
    }

    async fn read_body(resp: Response<Incoming>) -> Result<(StatusCode, Bytes)> {
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((status, bytes))
    }

    /// Request expecting a JSON (or empty) body; non-2xx become errors.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let (status, bytes) = Self::read_body(self.send(method, path, body, false).await?).await?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v["message"].as_str().map(String::from));
            return Err(DockerError {
                status: status.as_u16(),
                message: msg.unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
            }
            .into());
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| anyhow!("decoding docker response: {e}"))
    }

    /// Streams the raw response body (logs, pulls).
    pub async fn stream(&self, method: Method, path: &str) -> Result<impl Stream<Item = Result<Bytes>> + use<>> {
        let resp = self.send(method, path, None, false).await?;
        if !resp.status().is_success() {
            let (status, bytes) = Self::read_body(resp).await?;
            bail!(DockerError { status: status.as_u16(), message: String::from_utf8_lossy(&bytes).into_owned() });
        }
        Ok(resp.into_body().into_data_stream().map(|r| r.map_err(|e| anyhow!(e))))
    }

    /// `POST` that upgrades to a raw bidirectional stream (exec/attach hijacking).
    pub async fn hijack(&self, path: &str, body: &Value) -> Result<TokioIo<hyper::upgrade::Upgraded>> {
        let resp = self.send(Method::POST, path, Some(body), true).await?;
        let status = resp.status();
        if status != StatusCode::SWITCHING_PROTOCOLS && !status.is_success() {
            let (s, b) = Self::read_body(resp).await?;
            bail!(DockerError { status: s.as_u16(), message: String::from_utf8_lossy(&b).into_owned() });
        }
        let upgraded = hyper::upgrade::on(resp).await.context("docker did not upgrade the connection")?;
        Ok(TokioIo::new(upgraded))
    }
}

/// An error status returned by the Docker daemon.
#[derive(Debug)]
pub struct DockerError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "docker: {} (HTTP {})", self.message, self.status)
    }
}

impl std::error::Error for DockerError {}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DockerError>().is_some_and(|d| d.status == 404)
}

/// Splits the multiplexed stdout/stderr stream used for non-TTY containers:
/// frames are `[stream(1), 0, 0, 0, size(4, BE)] payload`.
#[derive(Default)]
pub struct Demuxer {
    buf: Vec<u8>,
}

impl Demuxer {
    /// Feeds bytes, returns complete `(stream, payload)` frames (1 = stdout, 2 = stderr).
    pub fn push(&mut self, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buf.extend_from_slice(data);
        let mut out = vec![];
        loop {
            if self.buf.len() < 8 {
                break;
            }
            let size = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            if self.buf.len() < 8 + size {
                break;
            }
            let stream = self.buf[0];
            let payload = self.buf[8..8 + size].to_vec();
            self.buf.drain(..8 + size);
            out.push((stream, payload));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demux_frames() {
        let mut d = Demuxer::default();
        let mut data = vec![1, 0, 0, 0, 0, 0, 0, 3];
        data.extend(b"abc");
        data.extend([2, 0, 0, 0, 0, 0, 0, 2]);
        let (a, b) = data.split_at(10);
        assert!(d.push(a).is_empty());
        let frames = d.push(b);
        assert_eq!(frames, vec![(1, b"abc".to_vec())]);
        assert_eq!(d.push(b"er"), vec![(2, b"er".to_vec())]);
    }
}
