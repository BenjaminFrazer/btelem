//! In-memory packet ring + `.btlm` writer for the btelem viewer.
//!
//! Modelled on Wireshark: while a source is connected, every raw packet is
//! teed into a bounded FIFO ring alongside the schema blob the source
//! announced. The user can [`Capture::save_btlm`] the ring to disk at any
//! time (interop with the existing Python `.btlm` tooling), or
//! [`Capture::clear`] to start fresh.
//!
//! The ring drops oldest packets when its byte budget is exceeded.
//!
//! The on-disk format mirrors `python/btelem/storage.py`:
//!
//! ```text
//! [magic "BTLM" 4] [version u16 LE] [schema_len u32 LE]
//! [schema blob]
//! [packet 0] [packet 1] ... [packet N]
//! [index_entry x (N+1)]   // 28 bytes each: offset/ts_min/ts_max/entry_count
//! [index_footer]          // 16 bytes: index_offset u64 / count u32 / magic u32 = "BTLI"
//! ```

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// `.btlm` file magic.
pub const MAGIC: &[u8; 4] = b"BTLM";
/// `.btlm` file format version this crate emits.
pub const VERSION: u16 = 1;
/// Footer magic ("BTLI" little-endian).
pub const INDEX_MAGIC: u32 = 0x494C5442;
/// Default ring byte budget (256 MiB).
pub const DEFAULT_RING_BYTES: usize = 256 * 1024 * 1024;

// Packet wire layout (mirror of btelem-wire's private constants).
const PACKET_HEADER_SIZE: usize = 16;
const ENTRY_HEADER_SIZE: usize = 16;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("no schema set; cannot write .btlm")]
    NoSchema,
    #[error("packet too short ({0} bytes) to contain a header")]
    BadPacket(usize),
    #[error("not a .btlm file (bad magic)")]
    BadMagic,
    #[error("unsupported .btlm version: {0}")]
    UnsupportedVersion(u16),
    #[error("truncated .btlm file")]
    Truncated,
}

/// Per-packet metadata extracted at push time. Kept alongside the bytes so
/// `save_btlm` doesn't have to re-scan.
#[derive(Clone, Copy, Debug)]
struct PacketMeta {
    ts_min: u64,
    ts_max: u64,
    entry_count: u32,
}

/// Snapshot of capture state for UI display.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureStats {
    /// Number of packets currently held in the ring.
    pub packets: u64,
    /// Total bytes held in the ring (packet payloads only, not schema).
    pub bytes: u64,
    /// Total packets ever pushed (including those evicted by the ring).
    pub packets_total: u64,
    /// Packets dropped due to the byte budget (FIFO eviction).
    pub packets_dropped: u64,
    /// Earliest packet timestamp currently in the ring.
    pub ts_min: Option<u64>,
    /// Latest packet timestamp currently in the ring.
    pub ts_max: Option<u64>,
    /// Wall-clock time since the schema was first set, or since last clear.
    pub age_secs: f64,
    /// True if a schema blob has been pushed (i.e. a source is connected).
    pub has_schema: bool,
}

struct Inner {
    schema: Option<Vec<u8>>,
    /// Front = oldest. We push to back and pop from front on overflow.
    packets: VecDeque<(Vec<u8>, PacketMeta)>,
    bytes: usize,
    cap_bytes: usize,
    packets_total: u64,
    packets_dropped: u64,
    epoch: Option<Instant>,
}

impl Inner {
    fn new(cap_bytes: usize) -> Self {
        Self {
            schema: None,
            packets: VecDeque::new(),
            bytes: 0,
            cap_bytes,
            packets_total: 0,
            packets_dropped: 0,
            epoch: None,
        }
    }

    fn clear(&mut self) {
        self.schema = None;
        self.packets.clear();
        self.bytes = 0;
        self.packets_total = 0;
        self.packets_dropped = 0;
        self.epoch = None;
    }

    fn evict_to_fit(&mut self) {
        while self.bytes > self.cap_bytes {
            let Some((pkt, _)) = self.packets.pop_front() else {
                break;
            };
            self.bytes -= pkt.len();
            self.packets_dropped += 1;
        }
    }
}

/// Cheap-clone, thread-safe capture handle.
///
/// Construct one at app startup, hand a clone to each ingest source, and
/// keep one for the viewer UI. All methods are internally locked; callers
/// can invoke them from any thread.
#[derive(Clone)]
pub struct Capture {
    inner: Arc<Mutex<Inner>>,
}

impl Default for Capture {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RING_BYTES)
    }
}

impl Capture {
    /// New capture with a custom byte budget.
    pub fn with_capacity(cap_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(cap_bytes))),
        }
    }

    /// Set/replace the schema blob. Returns `true` if the previous schema
    /// was non-empty and differed (caller should usually `clear` first).
    pub fn set_schema(&self, blob: Vec<u8>) -> bool {
        let mut g = self.inner.lock().unwrap();
        let differs = matches!(&g.schema, Some(s) if s != &blob);
        g.schema = Some(blob);
        if g.epoch.is_none() {
            g.epoch = Some(Instant::now());
        }
        differs
    }

    /// Return the currently-recorded schema blob (if any). Used by the
    /// viewer to detect schema rotation on reconnect.
    pub fn schema(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().schema.clone()
    }

    /// Append a raw packet (as it would appear on the wire, *without* the
    /// u32 length prefix). FIFO-evicts oldest packets if the byte budget
    /// is exceeded.
    pub fn push_packet(&self, bytes: Vec<u8>) -> Result<(), CaptureError> {
        let meta = extract_meta(&bytes)?;
        let mut g = self.inner.lock().unwrap();
        g.bytes += bytes.len();
        g.packets.push_back((bytes, meta));
        g.packets_total += 1;
        if g.epoch.is_none() {
            g.epoch = Some(Instant::now());
        }
        g.evict_to_fit();
        Ok(())
    }

    /// Drop schema, packets, and counters. Bumps the logical revision.
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// Current stats snapshot (cheap).
    pub fn stats(&self) -> CaptureStats {
        let g = self.inner.lock().unwrap();
        let (ts_min, ts_max) = if g.packets.is_empty() {
            (None, None)
        } else {
            let lo = g.packets.iter().map(|(_, m)| m.ts_min).min();
            let hi = g.packets.iter().map(|(_, m)| m.ts_max).max();
            (lo, hi)
        };
        let age_secs = g.epoch.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        CaptureStats {
            packets: g.packets.len() as u64,
            bytes: g.bytes as u64,
            packets_total: g.packets_total,
            packets_dropped: g.packets_dropped,
            ts_min,
            ts_max,
            age_secs,
            has_schema: g.schema.is_some(),
        }
    }

    /// Returns true if the ring has at least one packet.
    pub fn has_data(&self) -> bool {
        let g = self.inner.lock().unwrap();
        !g.packets.is_empty()
    }

    /// Write the ring out as a `.btlm` file (header + schema + packets +
    /// index + footer). Requires a schema to have been pushed.
    pub fn save_btlm(&self, path: &Path) -> Result<SaveReport, CaptureError> {
        // Hold the lock for the whole write so an in-flight reconnect or
        // clear can't tear the snapshot.
        let g = self.inner.lock().unwrap();
        let schema = g.schema.as_deref().ok_or(CaptureError::NoSchema)?;
        let file = File::create(path)?;
        let mut w = BufWriter::new(file);
        write_btlm_inner(&mut w, schema, &g.packets)?;
        w.flush()?;
        let bytes_written = w.stream_position()?;
        Ok(SaveReport {
            packets: g.packets.len() as u64,
            bytes: bytes_written,
            schema_bytes: schema.len() as u64,
        })
    }
}

/// Summary of a successful [`Capture::save_btlm`] call.
#[derive(Clone, Copy, Debug)]
pub struct SaveReport {
    pub packets: u64,
    pub bytes: u64,
    pub schema_bytes: u64,
}

/// In-memory snapshot of a `.btlm` file produced by [`read_btlm`].
///
/// `packets` are the raw packet bodies in file order, ready to feed
/// straight back through `btelem_wire::decode_packet`.
#[derive(Debug, Clone)]
pub struct LoadedCapture {
    pub schema: Vec<u8>,
    pub packets: Vec<Vec<u8>>,
}

/// Read a `.btlm` file from disk. Validates the magic + version, then
/// uses the trailer index to slice out each packet. Returns the schema
/// blob and packet bytes; the caller is responsible for decoding /
/// dispatching them.
pub fn read_btlm(path: &Path) -> Result<LoadedCapture, CaptureError> {
    let file = File::open(path)?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(CaptureError::BadMagic);
    }
    let mut ver = [0u8; 2];
    r.read_exact(&mut ver)?;
    let version = u16::from_le_bytes(ver);
    if version != VERSION {
        return Err(CaptureError::UnsupportedVersion(version));
    }
    let mut slen = [0u8; 4];
    r.read_exact(&mut slen)?;
    let schema_len = u32::from_le_bytes(slen) as usize;
    let mut schema = vec![0u8; schema_len];
    r.read_exact(&mut schema)?;

    // Walk the trailer index for per-packet offsets/lengths. The index
    // footer is exactly 16 bytes at end of file: index_offset u64,
    // count u32, magic u32.
    let total = r.seek(SeekFrom::End(0))?;
    if total < 16 {
        return Err(CaptureError::Truncated);
    }
    r.seek(SeekFrom::End(-16))?;
    let mut footer = [0u8; 16];
    r.read_exact(&mut footer)?;
    let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
    let count = u32::from_le_bytes(footer[8..12].try_into().unwrap()) as usize;
    let magic_le = u32::from_le_bytes(footer[12..16].try_into().unwrap());
    if magic_le != INDEX_MAGIC {
        return Err(CaptureError::BadMagic);
    }

    // Each index entry is 28 bytes (offset u64 + ts_min u64 + ts_max u64 + entries u32).
    let mut entries: Vec<u64> = Vec::with_capacity(count);
    r.seek(SeekFrom::Start(index_offset))?;
    for _ in 0..count {
        let mut buf = [0u8; 28];
        r.read_exact(&mut buf)?;
        let off = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        entries.push(off);
    }
    // Compute each packet's length from its successor's offset (or the
    // index offset for the last one).
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(count);
    for i in 0..count {
        let start = entries[i];
        let end = if i + 1 < count {
            entries[i + 1]
        } else {
            index_offset
        };
        if end < start {
            return Err(CaptureError::Truncated);
        }
        let len = (end - start) as usize;
        r.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        packets.push(buf);
    }

    Ok(LoadedCapture { schema, packets })
}

/// UTC-flavoured suggested filename: `btelem-YYYYMMDD-HHMMSS.btlm`.
pub fn suggested_filename() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Tiny UTC formatter — no chrono dep needed for a filename.
    let (y, m, d, hh, mm, ss) = utc_components(secs);
    format!("btelem-{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}.btlm")
}

fn utc_components(unix_secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (unix_secs % 60) as u32;
    let mm = ((unix_secs / 60) % 60) as u32;
    let hh = ((unix_secs / 3600) % 24) as u32;
    let mut days = unix_secs / 86_400;
    let mut year = 1970u32;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31u64,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for md in month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    let day = days as u32 + 1;
    (year, month, day, hh, mm, ss)
}

fn extract_meta(buf: &[u8]) -> Result<PacketMeta, CaptureError> {
    if buf.len() < PACKET_HEADER_SIZE {
        return Err(CaptureError::BadPacket(buf.len()));
    }
    let entry_count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let need = PACKET_HEADER_SIZE + entry_count * ENTRY_HEADER_SIZE;
    if buf.len() < need {
        return Err(CaptureError::BadPacket(buf.len()));
    }
    if entry_count == 0 {
        return Ok(PacketMeta {
            ts_min: 0,
            ts_max: 0,
            entry_count: 0,
        });
    }
    let mut ts_min = u64::MAX;
    let mut ts_max = 0u64;
    for i in 0..entry_count {
        let off = PACKET_HEADER_SIZE + i * ENTRY_HEADER_SIZE + 8;
        let ts = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        ts_min = ts_min.min(ts);
        ts_max = ts_max.max(ts);
    }
    Ok(PacketMeta {
        ts_min,
        ts_max,
        entry_count: entry_count as u32,
    })
}

fn write_btlm_inner<W: Write + Seek>(
    w: &mut W,
    schema: &[u8],
    packets: &VecDeque<(Vec<u8>, PacketMeta)>,
) -> Result<(), CaptureError> {
    // File header: 4s + u16 + u32 = 10 bytes.
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&(schema.len() as u32).to_le_bytes())?;
    w.write_all(schema)?;

    let mut offsets: Vec<(u64, PacketMeta)> = Vec::with_capacity(packets.len());
    for (pkt, meta) in packets {
        let off = w.stream_position()?;
        w.write_all(pkt)?;
        offsets.push((off, *meta));
    }

    let index_offset = w.stream_position()?;
    for (off, meta) in &offsets {
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&meta.ts_min.to_le_bytes())?;
        w.write_all(&meta.ts_max.to_le_bytes())?;
        w.write_all(&meta.entry_count.to_le_bytes())?;
    }
    w.write_all(&index_offset.to_le_bytes())?;
    w.write_all(&(offsets.len() as u32).to_le_bytes())?;
    w.write_all(&INDEX_MAGIC.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Build a minimal valid packet with `entries` (id, ts, payload).
    fn build_packet(entries: &[(u16, u64, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let entry_count = entries.len() as u16;
        let payload_size: u32 = entries.iter().map(|(_, _, p)| p.len() as u32).sum();
        // header: entry_count u16 / flags u16 / payload_size u32 / dropped u32 / reserved u32
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&payload_size.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut payload_off: u32 = 0;
        for (id, ts, p) in entries {
            // entry header: id u16 / payload_size u16 / payload_offset u32 / timestamp u64
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(p.len() as u16).to_le_bytes());
            out.extend_from_slice(&payload_off.to_le_bytes());
            out.extend_from_slice(&ts.to_le_bytes());
            payload_off += p.len() as u32;
        }
        for (_, _, p) in entries {
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn push_extracts_ts_range_and_counts() {
        let cap = Capture::default();
        cap.set_schema(b"DUMMYSCHEMA".to_vec());
        cap.push_packet(build_packet(&[(1, 100, &[0u8; 4]), (1, 200, &[0u8; 4])]))
            .unwrap();
        cap.push_packet(build_packet(&[(2, 50, &[0u8; 8])])).unwrap();
        let s = cap.stats();
        assert_eq!(s.packets, 2);
        assert_eq!(s.packets_total, 2);
        assert_eq!(s.packets_dropped, 0);
        assert_eq!(s.ts_min, Some(50));
        assert_eq!(s.ts_max, Some(200));
        assert!(s.has_schema);
    }

    #[test]
    fn ring_drops_oldest_at_cap() {
        let cap = Capture::with_capacity(128); // tiny
        cap.set_schema(b"S".to_vec());
        for i in 0..32u64 {
            cap.push_packet(build_packet(&[(1, i, &[0u8; 16])])).unwrap();
        }
        let s = cap.stats();
        assert!(s.bytes <= 128, "bytes={} > cap=128", s.bytes);
        assert_eq!(s.packets_total, 32);
        assert!(s.packets_dropped > 0);
        // ts_min must be from a recent packet, not 0
        assert!(s.ts_min.unwrap() > 0);
    }

    #[test]
    fn clear_resets_everything() {
        let cap = Capture::default();
        cap.set_schema(b"S".to_vec());
        cap.push_packet(build_packet(&[(1, 1, &[0u8; 4])])).unwrap();
        cap.clear();
        let s = cap.stats();
        assert_eq!(s.packets, 0);
        assert_eq!(s.bytes, 0);
        assert!(!s.has_schema);
        assert_eq!(cap.schema(), None);
    }

    #[test]
    fn save_btlm_round_trip() {
        let cap = Capture::default();
        let schema_blob: Vec<u8> = (0..37).collect();
        cap.set_schema(schema_blob.clone());
        let p1 = build_packet(&[(1, 100, &[0xAA, 0xBB, 0xCC, 0xDD])]);
        let p2 = build_packet(&[(2, 200, b"hello!!!"), (3, 250, &[0u8; 2])]);
        cap.push_packet(p1.clone()).unwrap();
        cap.push_packet(p2.clone()).unwrap();

        let dir = std::env::temp_dir().join(format!("btelem-cap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.btlm");
        let r = cap.save_btlm(&path).unwrap();
        assert_eq!(r.packets, 2);
        assert_eq!(r.schema_bytes, 37);
        assert!(r.bytes > 10 + 37 + p1.len() as u64 + p2.len() as u64);

        // Read back and check header + schema + packets verbatim + footer.
        let mut bytes = Vec::new();
        std::fs::File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes[0..4], MAGIC);
        let ver = u16::from_le_bytes([bytes[4], bytes[5]]);
        assert_eq!(ver, VERSION);
        let slen = u32::from_le_bytes(bytes[6..10].try_into().unwrap()) as usize;
        assert_eq!(slen, 37);
        assert_eq!(&bytes[10..10 + slen], schema_blob.as_slice());
        let mut off = 10 + slen;
        assert_eq!(&bytes[off..off + p1.len()], p1.as_slice());
        off += p1.len();
        assert_eq!(&bytes[off..off + p2.len()], p2.as_slice());

        // Footer = last 16 bytes: index_offset u64 / count u32 / magic u32.
        let n = bytes.len();
        let footer_magic = u32::from_le_bytes(bytes[n - 4..n].try_into().unwrap());
        assert_eq!(footer_magic, INDEX_MAGIC);
        let count = u32::from_le_bytes(bytes[n - 8..n - 4].try_into().unwrap()) as usize;
        assert_eq!(count, 2);
        let index_off = u64::from_le_bytes(bytes[n - 16..n - 8].try_into().unwrap()) as usize;
        // 28 bytes per index entry × 2 + 16 byte footer = 72 trailing bytes
        assert_eq!(index_off, n - 16 - 28 * 2);

        // First index entry should reference packet 1 at offset 10+schema_len
        let ie_off = u64::from_le_bytes(bytes[index_off..index_off + 8].try_into().unwrap());
        assert_eq!(ie_off as usize, 10 + slen);
        let ie_tsmin = u64::from_le_bytes(bytes[index_off + 8..index_off + 16].try_into().unwrap());
        assert_eq!(ie_tsmin, 100);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn save_with_no_schema_errors() {
        let cap = Capture::default();
        let path = std::env::temp_dir().join("btelem-noschema.btlm");
        match cap.save_btlm(&path) {
            Err(CaptureError::NoSchema) => {}
            other => panic!("expected NoSchema, got {other:?}"),
        }
    }

    #[test]
    fn set_schema_reports_diff() {
        let cap = Capture::default();
        assert!(!cap.set_schema(b"A".to_vec()));
        assert!(!cap.set_schema(b"A".to_vec())); // identical
        assert!(cap.set_schema(b"B".to_vec())); // diff
    }

    #[test]
    fn read_btlm_round_trips_save_btlm() {
        let cap = Capture::default();
        let schema_blob = vec![0xAAu8; 40];
        cap.set_schema(schema_blob.clone());
        let p1 = build_packet(&[(1, 100, &[1u8; 4])]);
        let p2 = build_packet(&[(2, 200, b"hello!!!"), (3, 250, &[0u8; 2])]);
        cap.push_packet(p1.clone()).unwrap();
        cap.push_packet(p2.clone()).unwrap();
        let dir =
            std::env::temp_dir().join(format!("btelem-read-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.btlm");
        cap.save_btlm(&path).unwrap();

        let loaded = read_btlm(&path).unwrap();
        assert_eq!(loaded.schema, schema_blob);
        assert_eq!(loaded.packets.len(), 2);
        assert_eq!(loaded.packets[0], p1);
        assert_eq!(loaded.packets[1], p2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn read_btlm_rejects_bad_magic() {
        let dir =
            std::env::temp_dir().join(format!("btelem-bad-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.btlm");
        std::fs::write(&path, b"NOPE\x01\x00\x00\x00\x00\x00").unwrap();
        match read_btlm(&path) {
            Err(CaptureError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
