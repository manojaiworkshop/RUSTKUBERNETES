//! SPDY/3.1 frames (`spdy/types.go`, `read.go`, `write.go`).
//!
//! ```text
//! control: |1| version (3) | type (16) | flags (8) | length (24) | data
//! data:    |0| stream id (31)          | flags (8) | length (24) | data
//! ```
//!
//! Header blocks (`SYN_STREAM`, `SYN_REPLY`, `HEADERS`) are a count followed by
//! length-prefixed name/value pairs (several values joined by NUL), compressed
//! with zlib using [`HEADER_DICTIONARY`]. Each direction keeps one compression
//! context for the life of the connection; every block ends with a sync flush.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::dictionary::HEADER_DICTIONARY;

pub const VERSION: u16 = 3;
pub const TYPE_SYN_STREAM: u16 = 1;
pub const TYPE_SYN_REPLY: u16 = 2;
pub const TYPE_RST_STREAM: u16 = 3;
pub const TYPE_SETTINGS: u16 = 4;
pub const TYPE_PING: u16 = 6;
pub const TYPE_GOAWAY: u16 = 7;
pub const TYPE_HEADERS: u16 = 8;
pub const TYPE_WINDOW_UPDATE: u16 = 9;
pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_UNIDIRECTIONAL: u8 = 0x02;
/// `MaxDataLength`
pub const MAX_DATA_LENGTH: usize = (1 << 24) - 1;
const MAX_HEADER_FIELD_SIZE: usize = 1 << 20;
const MAX_HEADER_COUNT: usize = 1000;

/// `RstStreamStatus`
pub mod status {
    pub const PROTOCOL_ERROR: u32 = 1;
    pub const INVALID_STREAM: u32 = 2;
    pub const REFUSED_STREAM: u32 = 3;
    pub const CANCEL: u32 = 5;
    pub const INTERNAL_ERROR: u32 = 6;
}

/// Header names are lowercase on the wire; multiple values of a name are kept in order.
pub type Headers = Vec<(String, String)>;

/// First value of a header, matched case-insensitively (`http.Header.Get`).
pub fn header<'a>(h: &'a Headers, name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    SynStream { stream_id: u32, associated_to: u32, priority: u8, slot: u8, flags: u8, headers: Headers },
    SynReply { stream_id: u32, flags: u8, headers: Headers },
    RstStream { stream_id: u32, status: u32 },
    Settings { flags: u8, entries: Vec<(u8, u32, u32)> },
    Ping { id: u32 },
    GoAway { last_good_stream_id: u32, status: u32 },
    Headers { stream_id: u32, flags: u8, headers: Headers },
    WindowUpdate { stream_id: u32, delta: u32 },
    Data { stream_id: u32, flags: u8, data: Bytes },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "spdy: {}", self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error(m.into()))
}

/// Writes frames; owns the outgoing header compression context.
pub struct Encoder {
    z: Compress,
}

impl Default for Encoder {
    fn default() -> Self {
        let mut z = Compress::new(Compression::best(), true);
        z.set_dictionary(&HEADER_DICTIONARY).expect("zlib dictionary");
        Self { z }
    }
}

fn header_block(h: &Headers) -> Vec<u8> {
    // Values of a repeated name are joined with NUL, names lowercased (`writeHeaderValueBlock`).
    let mut names: Vec<String> = vec![];
    let mut values: Vec<Vec<&str>> = vec![];
    for (k, v) in h {
        let k = k.to_ascii_lowercase();
        match names.iter().position(|n| *n == k) {
            Some(i) => values[i].push(v),
            None => {
                names.push(k);
                values.push(vec![v]);
            }
        }
    }
    let mut out = Vec::new();
    out.put_u32(names.len() as u32);
    for (n, vs) in names.iter().zip(values) {
        out.put_u32(n.len() as u32);
        out.put_slice(n.as_bytes());
        let v = vs.join("\0");
        out.put_u32(v.len() as u32);
        out.put_slice(v.as_bytes());
    }
    out
}

impl Encoder {
    fn compress(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + 64);
        let start_in = self.z.total_in();
        loop {
            let consumed = (self.z.total_in() - start_in) as usize;
            if out.capacity() - out.len() < 64 {
                out.reserve(out.capacity().max(256));
            }
            let before = out.len();
            self.z.compress_vec(&plain[consumed..], &mut out, FlushCompress::Sync).expect("zlib compress");
            let consumed = (self.z.total_in() - start_in) as usize;
            // Done when all input is consumed and the flush did not fill the buffer.
            if consumed == plain.len() && out.len() < out.capacity() && out.len() >= before {
                return out;
            }
        }
    }

    fn control(out: &mut BytesMut, ty: u16, flags: u8, payload: &[u8]) {
        out.put_u16(0x8000 | VERSION);
        out.put_u16(ty);
        out.put_u32((flags as u32) << 24 | payload.len() as u32);
        out.put_slice(payload);
    }

    pub fn encode(&mut self, f: &Frame, out: &mut BytesMut) {
        match f {
            Frame::SynStream { stream_id, associated_to, priority, slot, flags, headers } => {
                let block = self.compress(&header_block(headers));
                let mut p = Vec::with_capacity(10 + block.len());
                p.put_u32(stream_id & 0x7fff_ffff);
                p.put_u32(associated_to & 0x7fff_ffff);
                p.put_u8(priority << 5);
                p.put_u8(*slot);
                p.extend_from_slice(&block);
                Self::control(out, TYPE_SYN_STREAM, *flags, &p);
            }
            Frame::SynReply { stream_id, flags, headers } | Frame::Headers { stream_id, flags, headers } => {
                let ty = if matches!(f, Frame::SynReply { .. }) { TYPE_SYN_REPLY } else { TYPE_HEADERS };
                let block = self.compress(&header_block(headers));
                let mut p = Vec::with_capacity(4 + block.len());
                p.put_u32(stream_id & 0x7fff_ffff);
                p.extend_from_slice(&block);
                Self::control(out, ty, *flags, &p);
            }
            Frame::RstStream { stream_id, status } => {
                let mut p = Vec::with_capacity(8);
                p.put_u32(*stream_id);
                p.put_u32(*status);
                Self::control(out, TYPE_RST_STREAM, 0, &p);
            }
            Frame::Settings { flags, entries } => {
                let mut p = Vec::with_capacity(4 + entries.len() * 8);
                p.put_u32(entries.len() as u32);
                for (fl, id, v) in entries {
                    p.put_u32((*fl as u32) << 24 | (id & 0x00ff_ffff));
                    p.put_u32(*v);
                }
                Self::control(out, TYPE_SETTINGS, *flags, &p);
            }
            Frame::Ping { id } => Self::control(out, TYPE_PING, 0, &id.to_be_bytes()),
            Frame::GoAway { last_good_stream_id, status } => {
                let mut p = Vec::with_capacity(8);
                p.put_u32(*last_good_stream_id);
                p.put_u32(*status);
                Self::control(out, TYPE_GOAWAY, 0, &p);
            }
            Frame::WindowUpdate { stream_id, delta } => {
                let mut p = Vec::with_capacity(8);
                p.put_u32(*stream_id);
                p.put_u32(*delta);
                Self::control(out, TYPE_WINDOW_UPDATE, 0, &p);
            }
            Frame::Data { stream_id, flags, data } => {
                out.put_u32(stream_id & 0x7fff_ffff);
                out.put_u32((*flags as u32) << 24 | data.len() as u32);
                out.put_slice(data);
            }
        }
    }
}

/// Reads frames; owns the incoming header decompression context.
pub struct Decoder {
    z: Decompress,
}

impl Default for Decoder {
    fn default() -> Self {
        Self { z: Decompress::new(true) }
    }
}

impl Decoder {
    fn decompress(&mut self, block: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = Vec::with_capacity(block.len() * 4 + 256);
        let start_in = self.z.total_in();
        loop {
            let consumed = (self.z.total_in() - start_in) as usize;
            if consumed >= block.len() && out.len() < out.capacity() {
                return Ok(out);
            }
            if out.capacity() - out.len() < 256 {
                out.reserve(out.capacity().max(1024));
            }
            if out.len() > MAX_HEADER_FIELD_SIZE * 2 {
                return err("header block too large");
            }
            match self.z.decompress_vec(&block[consumed..], &mut out, FlushDecompress::Sync) {
                Ok(Status::Ok) | Ok(Status::BufError) | Ok(Status::StreamEnd) => {
                    let now = (self.z.total_in() - start_in) as usize;
                    if now == consumed && out.len() < out.capacity() && now < block.len() {
                        return err("header block: no progress decompressing");
                    }
                }
                Err(e) if e.needs_dictionary().is_some() => {
                    self.z.set_dictionary(&HEADER_DICTIONARY).map_err(|e| Error(format!("zlib dictionary: {e}")))?;
                }
                Err(e) => return err(format!("header block: {e}")),
            }
        }
    }

    fn headers(&mut self, block: &[u8]) -> Result<Headers, Error> {
        let plain = self.decompress(block)?;
        let mut b = &plain[..];
        let need = |b: &&[u8], n: usize| if b.remaining() < n { err("header block truncated") } else { Ok(()) };
        need(&b, 4)?;
        let count = b.get_u32() as usize;
        if count > MAX_HEADER_COUNT {
            return err(format!("too many headers ({count})"));
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            need(&b, 4)?;
            let n = b.get_u32() as usize;
            if n == 0 || n > MAX_HEADER_FIELD_SIZE {
                return err("invalid header name length");
            }
            need(&b, n)?;
            let name = String::from_utf8_lossy(&b[..n]).to_ascii_lowercase();
            b.advance(n);
            need(&b, 4)?;
            let n = b.get_u32() as usize;
            if n > MAX_HEADER_FIELD_SIZE {
                return err("invalid header value length");
            }
            need(&b, n)?;
            let value = String::from_utf8_lossy(&b[..n]).to_string();
            b.advance(n);
            for v in value.split('\0') {
                out.push((name.clone(), v.to_string()));
            }
        }
        Ok(out)
    }

    /// Takes one complete frame off the front of `buf`, or `None` if more bytes are needed.
    /// Unknown control frame types are skipped.
    pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
        loop {
            if buf.len() < 8 {
                return Ok(None);
            }
            let len = (u32::from_be_bytes([0, buf[5], buf[6], buf[7]])) as usize;
            if buf.len() < 8 + len {
                return Ok(None);
            }
            let mut f = buf.split_to(8 + len).freeze();
            let first = f.get_u32();
            let flags_len = f.get_u32();
            let flags = (flags_len >> 24) as u8;
            if first & 0x8000_0000 == 0 {
                return Ok(Some(Frame::Data { stream_id: first & 0x7fff_ffff, flags, data: f }));
            }
            let version = ((first >> 16) & 0x7fff) as u16;
            if version != VERSION {
                return err(format!("unsupported version {version}"));
            }
            let ty = (first & 0xffff) as u16;
            let short = |f: &Bytes, n: usize| if f.len() < n { err("control frame too short") } else { Ok(()) };
            let frame = match ty {
                TYPE_SYN_STREAM => {
                    short(&f, 10)?;
                    let stream_id = f.get_u32() & 0x7fff_ffff;
                    let associated_to = f.get_u32() & 0x7fff_ffff;
                    let priority = f.get_u8() >> 5;
                    let slot = f.get_u8();
                    Frame::SynStream { stream_id, associated_to, priority, slot, flags, headers: self.headers(&f)? }
                }
                TYPE_SYN_REPLY | TYPE_HEADERS => {
                    short(&f, 4)?;
                    let stream_id = f.get_u32() & 0x7fff_ffff;
                    let headers = self.headers(&f)?;
                    if ty == TYPE_SYN_REPLY {
                        Frame::SynReply { stream_id, flags, headers }
                    } else {
                        Frame::Headers { stream_id, flags, headers }
                    }
                }
                TYPE_RST_STREAM => {
                    short(&f, 8)?;
                    Frame::RstStream { stream_id: f.get_u32() & 0x7fff_ffff, status: f.get_u32() }
                }
                TYPE_SETTINGS => {
                    short(&f, 4)?;
                    let n = f.get_u32() as usize;
                    short(&f, n * 8)?;
                    let entries = (0..n)
                        .map(|_| {
                            let fid = f.get_u32();
                            ((fid >> 24) as u8, fid & 0x00ff_ffff, f.get_u32())
                        })
                        .collect();
                    Frame::Settings { flags, entries }
                }
                TYPE_PING => {
                    short(&f, 4)?;
                    Frame::Ping { id: f.get_u32() }
                }
                TYPE_GOAWAY => {
                    short(&f, 8)?;
                    Frame::GoAway { last_good_stream_id: f.get_u32() & 0x7fff_ffff, status: f.get_u32() }
                }
                TYPE_WINDOW_UPDATE => {
                    short(&f, 8)?;
                    Frame::WindowUpdate { stream_id: f.get_u32() & 0x7fff_ffff, delta: f.get_u32() & 0x7fff_ffff }
                }
                _ => continue,
            };
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn roundtrip_with_shared_compression_context() {
        let mut enc = Encoder::default();
        let mut dec = Decoder::default();
        let frames = vec![
            Frame::SynStream {
                stream_id: 1,
                associated_to: 0,
                priority: 0,
                slot: 0,
                flags: 0,
                headers: h(&[("streamtype", "error"), ("port", "8080"), ("requestid", "0")]),
            },
            Frame::SynStream {
                stream_id: 3,
                associated_to: 0,
                priority: 0,
                slot: 0,
                flags: 0,
                headers: h(&[("streamtype", "data"), ("port", "8080"), ("requestid", "0")]),
            },
            Frame::SynReply { stream_id: 1, flags: 0, headers: vec![] },
            Frame::Data { stream_id: 3, flags: FLAG_FIN, data: Bytes::from_static(b"hello") },
            Frame::Ping { id: 1 },
            Frame::RstStream { stream_id: 3, status: status::CANCEL },
            Frame::GoAway { last_good_stream_id: 3, status: 0 },
            Frame::Settings { flags: 0, entries: vec![(0, 7, 65536)] },
            Frame::WindowUpdate { stream_id: 1, delta: 100 },
        ];
        let mut wire = BytesMut::new();
        for f in &frames {
            enc.encode(f, &mut wire);
        }
        // Byte-by-byte delivery exercises partial-frame handling.
        let mut buf = BytesMut::new();
        let mut got = vec![];
        for b in wire {
            buf.put_u8(b);
            while let Some(f) = dec.decode(&mut buf).unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn multi_values_and_case() {
        let mut enc = Encoder::default();
        let mut dec = Decoder::default();
        let mut wire = BytesMut::new();
        enc.encode(&Frame::SynReply { stream_id: 1, flags: 0, headers: h(&[("X-A", "1"), ("x-a", "2")]) }, &mut wire);
        let Some(Frame::SynReply { headers, .. }) = dec.decode(&mut wire).unwrap() else { panic!() };
        assert_eq!(headers, h(&[("x-a", "1"), ("x-a", "2")]));
        assert_eq!(header(&headers, "X-A"), Some("1"));
    }

    #[test]
    fn dictionary_id_matches_spdy3() {
        // zlib dictionary id (adler32) of the SPDY/3 dictionary.
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for x in HEADER_DICTIONARY {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!((b << 16) | a, 0xe3c6a7c2);
    }
}
