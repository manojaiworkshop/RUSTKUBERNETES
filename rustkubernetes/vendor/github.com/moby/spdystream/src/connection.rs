//! A SPDY/3.1 connection (`connection.go`, `stream.go`) on tokio.
//!
//! One task reads frames, one task writes them (all writes, including header
//! compression, are serialized through it). Behaviour follows moby/spdystream:
//!
//! * streams opened by the peer must use the peer's parity and increasing ids,
//!   otherwise they are reset with `PROTOCOL_ERROR`;
//! * pings with the peer's parity are echoed; replies to our own pings complete
//!   [`Connection::ping`];
//! * a `FIN` (data or control flag) ends the peer's half of a stream, a
//!   `RST_STREAM` ends both; `GOAWAY` stops new streams;
//! * flow control (`WINDOW_UPDATE`, `SETTINGS`) is ignored, as spdystream does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

use crate::frame::{status, Decoder, Encoder, Frame, Headers, FLAG_FIN};

/// How long [`Connection::create_stream`] waits for `SYN_REPLY`.
pub const CREATE_STREAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

struct Inner {
    out: mpsc::UnboundedSender<Frame>,
    server: bool,
    next_stream_id: AtomicU32,
    next_ping_id: AtomicU32,
    /// Peer-opened and locally-opened streams still able to receive data.
    streams: Mutex<HashMap<u32, mpsc::UnboundedSender<Bytes>>>,
    replies: Mutex<HashMap<u32, oneshot::Sender<Headers>>>,
    pings: Mutex<HashMap<u32, oneshot::Sender<()>>>,
    received_stream_id: AtomicU32,
    gone_away: AtomicBool,
    closed: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

/// A stream: its headers, the peer's data (`None` once the peer finished or reset
/// it) and a handle to write.
pub struct Stream {
    pub id: u32,
    pub headers: Headers,
    reader: StreamReader,
    writer: StreamWriter,
}

pub struct StreamReader {
    rx: mpsc::UnboundedReceiver<Bytes>,
}

#[derive(Clone)]
pub struct StreamWriter {
    id: u32,
    inner: Arc<Inner>,
}

impl StreamReader {
    /// Next chunk of data; `None` at FIN, reset or connection close.
    pub async fn read(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }
}

impl StreamWriter {
    fn send(&self, f: Frame) -> std::io::Result<()> {
        self.inner.out.send(f).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "spdy connection closed"))
    }

    /// `SendReply`: accepts a stream the peer opened.
    pub fn reply(&self, headers: Headers) -> std::io::Result<()> {
        self.send(Frame::SynReply { stream_id: self.id, flags: 0, headers })
    }

    pub fn write(&self, data: Bytes) -> std::io::Result<()> {
        for chunk in data.chunks(crate::frame::MAX_DATA_LENGTH) {
            self.send(Frame::Data { stream_id: self.id, flags: 0, data: data.slice_ref(chunk) })?;
        }
        Ok(())
    }

    /// `Close`: half-closes our side (an empty data frame with FIN).
    pub fn close(&self) -> std::io::Result<()> {
        self.send(Frame::Data { stream_id: self.id, flags: FLAG_FIN, data: Bytes::new() })
    }

    /// `Reset`: aborts the stream in both directions (`CANCEL`).
    pub fn reset(&self) -> std::io::Result<()> {
        self.inner.streams.lock().unwrap().remove(&self.id);
        self.send(Frame::RstStream { stream_id: self.id, status: status::CANCEL })
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl Stream {
    pub fn header(&self, name: &str) -> Option<&str> {
        crate::frame::header(&self.headers, name)
    }

    pub fn writer(&self) -> StreamWriter {
        self.writer.clone()
    }

    pub async fn read(&mut self) -> Option<Bytes> {
        self.reader.read().await
    }

    pub fn into_split(self) -> (StreamReader, StreamWriter) {
        (self.reader, self.writer)
    }
}

impl std::ops::Deref for Stream {
    type Target = StreamWriter;
    fn deref(&self) -> &StreamWriter {
        &self.writer
    }
}

impl Connection {
    /// Server side of an upgraded connection (`NewServerConnection`): returns the
    /// connection and the streams the client opens.
    pub fn server<T: AsyncRead + AsyncWrite + Send + 'static>(io: T) -> (Connection, mpsc::UnboundedReceiver<Stream>) {
        Self::start(io, true)
    }

    /// Client side (`NewClientConnection`).
    pub fn client<T: AsyncRead + AsyncWrite + Send + 'static>(io: T) -> (Connection, mpsc::UnboundedReceiver<Stream>) {
        Self::start(io, false)
    }

    fn start<T: AsyncRead + AsyncWrite + Send + 'static>(io: T, server: bool) -> (Connection, mpsc::UnboundedReceiver<Stream>) {
        let (rd, wr) = tokio::io::split(io);
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            out: out_tx,
            server,
            // Servers open even stream ids and send even pings; clients odd ones.
            next_stream_id: AtomicU32::new(if server { 2 } else { 1 }),
            next_ping_id: AtomicU32::new(if server { 2 } else { 1 }),
            streams: Mutex::new(HashMap::new()),
            replies: Mutex::new(HashMap::new()),
            pings: Mutex::new(HashMap::new()),
            received_stream_id: AtomicU32::new(0),
            gone_away: AtomicBool::new(false),
            closed,
        });
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(wr, out_rx, inner.clone()));
        tokio::spawn(read_loop(rd, inner.clone(), in_tx));
        (Connection { inner }, in_rx)
    }

    /// Opens a stream and waits for the peer's `SYN_REPLY` (`CreateStream` + `WaitTimeout`).
    pub async fn create_stream(&self, headers: Headers, fin: bool) -> std::io::Result<Stream> {
        let id = self.inner.next_stream_id.fetch_add(2, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner.streams.lock().unwrap().insert(id, tx);
        self.inner.replies.lock().unwrap().insert(id, reply_tx);
        let frame = Frame::SynStream {
            stream_id: id,
            associated_to: 0,
            priority: 0,
            slot: 0,
            flags: if fin { FLAG_FIN } else { 0 },
            headers: headers.clone(),
        };
        self.inner.out.send(frame).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "spdy connection closed"))?;
        match tokio::time::timeout(CREATE_STREAM_RESPONSE_TIMEOUT, reply_rx).await {
            Ok(Ok(_)) => Ok(Stream { id, headers, reader: StreamReader { rx }, writer: StreamWriter { id, inner: self.inner.clone() } }),
            Ok(Err(_)) => Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "stream reset before reply")),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out waiting for stream reply")),
        }
    }

    /// Sends a ping and waits for the echo.
    pub async fn ping(&self) -> std::io::Result<Duration> {
        let id = self.inner.next_ping_id.fetch_add(2, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pings.lock().unwrap().insert(id, tx);
        let start = std::time::Instant::now();
        self.inner.out.send(Frame::Ping { id }).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"))?;
        rx.await.map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "connection closed"))?;
        Ok(start.elapsed())
    }

    /// `Close`: GOAWAY, then the connection is shut down once queued frames are written.
    pub fn close(&self) {
        let last = self.inner.received_stream_id.load(Ordering::SeqCst);
        let _ = self.inner.out.send(Frame::GoAway { last_good_stream_id: last, status: 0 });
        self.inner.gone_away.store(true, Ordering::SeqCst);
        let _ = self.inner.closed.send(true);
    }

    /// Resolves when the connection is closed (peer gone, read error, or [`Connection::close`]).
    pub async fn closed(&self) {
        let mut rx = self.inner.closed.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn is_server(&self) -> bool {
        self.inner.server
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<Frame>, inner: Arc<Inner>) {
    let mut enc = Encoder::default();
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut closed = inner.closed.subscribe();
    loop {
        let frame = tokio::select! {
            f = rx.recv() => match f { Some(f) => f, None => break },
            _ = closed.changed() => {
                // Flush what is queued (e.g. GOAWAY), then stop.
                while let Ok(f) = rx.try_recv() {
                    enc.encode(&f, &mut buf);
                }
                let _ = wr.write_all(&buf).await;
                break;
            }
        };
        enc.encode(&frame, &mut buf);
        // Batch whatever else is already queued into one write.
        while let Ok(f) = rx.try_recv() {
            enc.encode(&f, &mut buf);
            if buf.len() > 256 * 1024 {
                break;
            }
        }
        if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
            break;
        }
        buf.clear();
    }
    let _ = wr.shutdown().await;
    let _ = inner.closed.send(true);
}

async fn read_loop<R: AsyncRead + Unpin>(mut rd: R, inner: Arc<Inner>, incoming: mpsc::UnboundedSender<Stream>) {
    let mut dec = Decoder::default();
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut closed = inner.closed.subscribe();
    'outer: loop {
        loop {
            match dec.decode(&mut buf) {
                Ok(Some(f)) => handle(&inner, &incoming, f),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("spdy: {e}");
                    break 'outer;
                }
            }
        }
        tokio::select! {
            r = rd.read_buf(&mut buf) => match r {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            },
            _ = closed.changed() => break,
        }
    }
    // Peer gone: every stream ends.
    inner.streams.lock().unwrap().clear();
    inner.replies.lock().unwrap().clear();
    inner.pings.lock().unwrap().clear();
    let _ = inner.closed.send(true);
}

fn handle(inner: &Arc<Inner>, incoming: &mpsc::UnboundedSender<Stream>, f: Frame) {
    match f {
        Frame::SynStream { stream_id, flags, headers, .. } => {
            // validateStreamId: the peer's parity, strictly increasing.
            let peer_parity = if inner.server { 1 } else { 0 };
            let last = inner.received_stream_id.load(Ordering::SeqCst);
            if inner.gone_away.load(Ordering::SeqCst) {
                return;
            }
            if stream_id & 1 != peer_parity || (last != 0 && stream_id <= last) {
                let _ = inner.out.send(Frame::RstStream { stream_id, status: status::PROTOCOL_ERROR });
                return;
            }
            inner.received_stream_id.store(stream_id, Ordering::SeqCst);
            let (tx, rx) = mpsc::unbounded_channel();
            if flags & FLAG_FIN == 0 {
                inner.streams.lock().unwrap().insert(stream_id, tx);
            }
            let s = Stream {
                id: stream_id,
                headers,
                reader: StreamReader { rx },
                writer: StreamWriter { id: stream_id, inner: inner.clone() },
            };
            let _ = incoming.send(s);
        }
        Frame::SynReply { stream_id, flags, headers } => {
            if let Some(tx) = inner.replies.lock().unwrap().remove(&stream_id) {
                let _ = tx.send(headers);
            }
            if flags & FLAG_FIN != 0 {
                inner.streams.lock().unwrap().remove(&stream_id);
            }
        }
        Frame::Headers { stream_id, flags, .. } => {
            if flags & FLAG_FIN != 0 {
                inner.streams.lock().unwrap().remove(&stream_id);
            }
        }
        Frame::Data { stream_id, flags, data } => {
            let mut streams = inner.streams.lock().unwrap();
            if let Some(tx) = streams.get(&stream_id) {
                if !data.is_empty() {
                    let _ = tx.send(data);
                }
            }
            if flags & FLAG_FIN != 0 {
                streams.remove(&stream_id);
            }
        }
        Frame::RstStream { stream_id, .. } => {
            inner.streams.lock().unwrap().remove(&stream_id);
            inner.replies.lock().unwrap().remove(&stream_id);
        }
        Frame::Ping { id } => {
            let ours = if inner.server { 0 } else { 1 };
            if id & 1 != ours {
                let _ = inner.out.send(Frame::Ping { id });
            } else if let Some(tx) = inner.pings.lock().unwrap().remove(&id) {
                let _ = tx.send(());
            }
        }
        Frame::GoAway { .. } => {
            inner.gone_away.store(true, Ordering::SeqCst);
        }
        Frame::Settings { .. } | Frame::WindowUpdate { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[tokio::test]
    async fn client_server_streams() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (client, _) = Connection::client(a);
        let (_server, mut incoming) = Connection::server(b);
        // Server: echo every stream's data back, then FIN.
        tokio::spawn(async move {
            while let Some(s) = incoming.recv().await {
                let (mut r, w) = s.into_split();
                w.reply(vec![]).unwrap();
                tokio::spawn(async move {
                    while let Some(d) = r.read().await {
                        w.write(d).unwrap();
                    }
                    w.close().unwrap();
                });
            }
        });
        let s = client.create_stream(h(&[("streamType", "data"), ("port", "80")]), false).await.unwrap();
        assert_eq!(s.id, 1);
        assert_eq!(s.header("streamtype"), Some("data"));
        let (mut r, w) = s.into_split();
        w.write(Bytes::from_static(b"ping")).unwrap();
        assert_eq!(r.read().await.unwrap(), Bytes::from_static(b"ping"));
        w.close().unwrap();
        assert!(r.read().await.is_none());
        let s2 = client.create_stream(h(&[("streamType", "error")]), false).await.unwrap();
        assert_eq!(s2.id, 3);
        assert!(client.ping().await.is_ok());
    }

    #[tokio::test]
    async fn rejects_bad_stream_ids() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (_server, mut incoming) = Connection::server(b);
        let (mut rd, mut wr) = tokio::io::split(a);
        let mut enc = Encoder::default();
        let mut buf = BytesMut::new();
        // An even id from a client is a protocol error.
        enc.encode(&Frame::SynStream { stream_id: 2, associated_to: 0, priority: 0, slot: 0, flags: 0, headers: vec![] }, &mut buf);
        wr.write_all(&buf).await.unwrap();
        let mut dec = Decoder::default();
        let mut inb = BytesMut::new();
        let f = loop {
            if let Some(f) = dec.decode(&mut inb).unwrap() {
                break f;
            }
            rd.read_buf(&mut inb).await.unwrap();
        };
        assert_eq!(f, Frame::RstStream { stream_id: 2, status: status::PROTOCOL_ERROR });
        assert!(incoming.try_recv().is_err());
    }
}
