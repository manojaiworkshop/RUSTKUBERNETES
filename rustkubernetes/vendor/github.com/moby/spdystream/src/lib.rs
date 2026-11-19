//! SPDY/3.1 framing and connections, after `github.com/moby/spdystream` (which
//! Kubernetes vendors): the transport of `kubectl port-forward` (and the older
//! SPDY flavour of exec/attach) between kubectl and the kubelet.
//!
//! | Module | Go |
//! |--------|----|
//! | [`frame`] | `spdy/types.go`, `read.go`, `write.go` |
//! | [`dictionary`] | `spdy/dictionary.go` |
//! | [`connection`] | `connection.go`, `stream.go` |

pub mod connection;
pub mod dictionary;
pub mod frame;

pub use connection::{Connection, Stream, StreamReader, StreamWriter};
pub use frame::{header, Frame, Headers};

/// `HeaderSpdy31`
pub const HEADER_SPDY31: &str = "SPDY/3.1";
