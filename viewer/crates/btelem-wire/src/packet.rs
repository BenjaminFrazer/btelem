//! Packet decode.

use crate::*;

/// Decoded packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    pub entry_count: u16,
    pub flags: u16,
    pub payload_size: u32,
    pub dropped: u32,
}

/// One entry within a packet, with the payload borrowed from the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedEntry<'a> {
    pub id: u16,
    pub timestamp: u64,
    pub payload: &'a [u8],
}

/// Whole packet: header plus entries borrowing from the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub header: PacketHeader,
    pub entries: Vec<DecodedEntry<'a>>,
}

/// Decode one packet (the bytes that follow the u32 length prefix on the wire).
pub fn decode_packet(buf: &[u8]) -> Result<Packet<'_>> {
    need(buf, PACKET_HEADER_SIZE)?;
    let entry_count = read_u16(buf, 0)?;
    let flags = read_u16(buf, 2)?;
    let payload_size = read_u32(buf, 4)?;
    let dropped = read_u32(buf, 8)?;
    let header = PacketHeader {
        entry_count,
        flags,
        payload_size,
        dropped,
    };

    let table_off = PACKET_HEADER_SIZE;
    let payload_base = table_off + entry_count as usize * ENTRY_HEADER_SIZE;
    need(buf, payload_base)?;
    need(buf, payload_base + payload_size as usize)?;

    let mut entries = Vec::with_capacity(entry_count as usize);
    for i in 0..entry_count as usize {
        let off = table_off + i * ENTRY_HEADER_SIZE;
        let id = read_u16(buf, off)?;
        let psize = read_u16(buf, off + 2)? as usize;
        let poff = read_u32(buf, off + 4)? as usize;
        let ts = read_u64(buf, off + 8)?;
        if poff + psize > payload_size as usize {
            return Err(WireError::PayloadOob {
                offset: poff,
                size: psize,
                buf: payload_size as usize,
            });
        }
        let p_start = payload_base + poff;
        entries.push(DecodedEntry {
            id,
            timestamp: ts,
            payload: &buf[p_start..p_start + psize],
        });
    }

    Ok(Packet { header, entries })
}
