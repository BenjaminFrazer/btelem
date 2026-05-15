//! TCP source: connect, decode schema, decode packets, push to store.

use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use btelem_capture::Capture;
use btelem_store::{MockStore, Store};
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
    /// registered.
    ///
    /// After the initial successful connection, the background thread
    /// automatically reconnects if the link drops (e.g. target reboot).
    /// On reconnect the schema is re-read and the store is cleared so
    /// stale channel IDs don't conflict with the new schema. The thread
    /// exits only when the [`SourceHandle`] is dropped.
    ///
    /// If `capture` is `Some`, the schema blob and every raw packet are
    /// also pushed into it for later `.btlm` saving.
    pub fn connect(
        addr: impl ToSocketAddrs,
        store: MockStore,
        capture: Option<Capture>,
    ) -> Result<SourceHandle, IngestError> {
        let resolved: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
        if resolved.is_empty() {
            return Err(IngestError::Io(std::io::Error::new(
                ErrorKind::InvalidInput,
                "no socket address resolved",
            )));
        }

        // Initial connect synchronously so the caller learns whether the
        // endpoint is reachable. Subsequent reconnects happen in the
        // background.
        let (stream, schema_buf, map) = connect_once(&resolved, &store)?;
        if let Some(cap) = &capture {
            cap.set_schema(schema_buf);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("btelem-ingest-tcp".into())
            .spawn(move || run_with_reconnect(resolved, stream, store, map, capture, stop_thread))?;

        Ok(SourceHandle {
            stop,
            join: Some(join),
        })
    }
}

/// Perform a single connect + schema read. Used both for the initial
/// connection and for each reconnect attempt.
fn connect_once(
    addrs: &[SocketAddr],
    store: &MockStore,
) -> Result<(TcpStream, Vec<u8>, ChannelMap), IngestError> {
    let mut last_err: Option<IngestError> = None;
    for a in addrs {
        match TcpStream::connect_timeout(a, Duration::from_secs(2)) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_millis(250)))?;
                let mut stream = s;
                let schema_len = read_u32(&mut stream)? as usize;
                let mut buf = vec![0u8; schema_len];
                read_exact_or_eof(&mut stream, &mut buf)?;
                let schema = Schema::decode(&buf)?;
                let map = ChannelMap::build(&schema, store)?;
                return Ok((stream, buf, map));
            }
            Err(e) => last_err = Some(IngestError::Io(e)),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        IngestError::Io(std::io::Error::other("no addresses to connect to"))
    }))
}

/// Drive the packet loop on the current connection, and on disconnect
/// keep reconnecting until `stop` is set. The store and capture are
/// cleared on each reconnect so a new schema doesn't collide with
/// previously registered channels.
fn run_with_reconnect(
    addrs: Vec<SocketAddr>,
    stream: TcpStream,
    store: MockStore,
    map: ChannelMap,
    capture: Option<Capture>,
    stop: Arc<AtomicBool>,
) -> Result<(), IngestError> {
    // Initial session uses the already-connected stream + map.
    if let Err(e) = packet_loop(stream, &store, &map, &capture, &stop) {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Anything other than a clean shutdown falls through to retry.
        let _ = e;
    }
    drop(map); // schema may change across reboots

    let mut delay_ms: u64 = 250;
    while !stop.load(Ordering::SeqCst) {
        // Backoff sleep, broken into 100ms chunks so shutdown is snappy.
        let mut slept = 0u64;
        while slept < delay_ms && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100.min(delay_ms - slept)));
            slept += 100;
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Wipe stale samples + channels before re-registering against
        // the new schema. The viewer treats `revision` changes as a hint
        // to re-resolve channel IDs.
        store.clear();
        if let Some(cap) = &capture {
            cap.clear();
        }

        match connect_once(&addrs, &store) {
            Ok((s, schema_buf, m)) => {
                if let Some(cap) = &capture {
                    cap.set_schema(schema_buf);
                }
                delay_ms = 250;
                if let Err(e) = packet_loop(s, &store, &m, &capture, &stop) {
                    if stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    let _ = e;
                }
            }
            Err(_) => {
                // Exponential backoff up to ~5s between attempts.
                delay_ms = (delay_ms * 2).min(5_000);
            }
        }
    }
    Ok(())
}

fn packet_loop(
    mut stream: TcpStream,
    store: &MockStore,
    map: &ChannelMap,
    capture: &Option<Capture>,
    stop: &Arc<AtomicBool>,
) -> Result<(), IngestError> {
    let mut pkt = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let len = match read_u32(&mut stream) {
            Ok(n) => n as usize,
            Err(IngestError::Closed) => return Err(IngestError::Closed),
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
            map.dispatch(e.id, e.timestamp, e.payload, store);
        }
        if let Some(cap) = capture {
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
