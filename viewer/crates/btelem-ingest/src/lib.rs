//! Ingest sources for btelem.
//!
//! Currently provides a TCP source that connects to a btelem `btelem_serve`
//! endpoint, decodes the schema + packet stream, and pushes samples into a
//! [`btelem_store::MockStore`].
//!
//! All work happens on a single background thread per source. The thread
//! exits cleanly when the connection closes or the [`SourceHandle`] is
//! dropped.

#![forbid(unsafe_code)]

mod mapper;
mod tcp;

pub use mapper::{ChannelMap, MapError};
pub use tcp::{SourceHandle, TcpSource};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire: {0}")]
    Wire(#[from] btelem_wire::WireError),
    #[error("mapping: {0}")]
    Map(#[from] MapError),
    #[error("connection closed")]
    Closed,
}
