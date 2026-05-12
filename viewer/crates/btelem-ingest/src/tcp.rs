//! TCP source: connect, decode schema, decode packets, push to store.

use std::io::{ErrorKind, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use btelem_capture::Capture;
use btelem_store::MockStore;
use btelem_wire::{decode_packet, Schema};

use crate::{ChannelMap, IngestError};

/// Owned handle to a running TCP ingest thread. Drop to request shutdown
/// (the thread will exit on next IO timeout).
pub struct SourceHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<(), IngestError>>>,
}

impl SourceHandle {
    /// Block until the ingest thread exits.
    pub fn join(mut self) -> Result<(), IngestError> {
        self.stop.store(true, Ordering::SeqCst);
        match self.join.take() {
            Some(h) => h.join().expect("ingest thread panicked"),
            None => Ok(()),
        }
    }

    /// True if the thread is still running.
    pub fn is_alive(&self) -> bool {
        self.join.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// TCP ingest source.
pub struct TcpSource;

impl TcpSource {
    /// Connect to `addr`, spawn a background thread that decodes the stream
    /// into `store`. Returns once the schema has been read and channels
    /// registered; the packet loop continues in the background until EOF or
    /// the returned handle is dropped.
    ///
    /// If `capture` is `Some`, the schema blob and every raw packet are
    /// also pushed into it for later `.btlm` saving.
    pub fn connect(
        addr: impl ToSocketAddrs,
        store: MockStore,
        capture: Option<Capture>,
    ) -> Result<SourceHandle, IngestError> {
        let mut stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        // Read schema (u32 length + blob)
        let schema_len = read_u32(&mut stream)? as usize;
        let mut buf = vec![0u8; schema_len];
        read_exact_or_eof(&mut stream, &mut buf)?;
        let schema = Schema::decode(&buf)?;
        let map = ChannelMap::build(&schema, &store)?;
        if let Some(cap) = &capture {
            cap.set_schema(buf);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("btelem-ingest-tcp".into())
            .spawn(move || packet_loop(stream, store, map, capture, stop_thread))?;

        Ok(SourceHandle {
            stop,
            join: Some(join),
        })
    }
}

fn packet_loop(
    mut stream: TcpStream,
    store: MockStore,
    map: ChannelMap,
    capture: Option<Capture>,
    stop: Arc<AtomicBool>,
) -> Result<(), IngestError> {
    let mut pkt = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let len = match read_u32(&mut stream) {
            Ok(n) => n as usize,
            Err(IngestError::Closed) => return Ok(()),
            Err(IngestError::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        pkt.resize(len, 0);
        read_exact_or_eof(&mut stream, &mut pkt)?;
        let p = decode_packet(&pkt)?;
        for e in &p.entries {
            map.dispatch(e.id, e.timestamp, e.payload, &store);
        }
        if let Some(cap) = &capture {
            // Decode already succeeded; tee a copy of the raw bytes.
            let _ = cap.push_packet(pkt.clone());
        }
    }
    Ok(())
}

fn read_u32(stream: &mut TcpStream) -> Result<u32, IngestError> {
    let mut b = [0u8; 4];
    read_exact_or_eof(stream, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Like `Read::read_exact` but maps EOF (0 bytes) to `IngestError::Closed`
/// and propagates timeouts so the caller can re-check `stop`.
fn read_exact_or_eof(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), IngestError> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(IngestError::Closed),
            Ok(n) => filled += n,
            Err(e)
                if e.kind() == ErrorKind::Interrupted
                    || e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::TimedOut =>
            {
                if filled == 0 {
                    return Err(IngestError::Io(e));
                }
                // Mid-read: keep trying.
                continue;
            }
            Err(e) => return Err(IngestError::Io(e)),
        }
    }
    Ok(())
}
