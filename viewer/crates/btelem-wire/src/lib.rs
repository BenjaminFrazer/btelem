//! Decoder for the btelem TCP wire format.
//!
//! On-the-wire layout, all little-endian (the C side advertises endianness in
//! the schema header but only LE is currently supported):
//!
//! ```text
//! u32 schema_blob_len
//! [u8; schema_blob_len] schema_blob:
//!     u8  endianness            # 0 = little
//!     u16 entry_count
//!     SchemaWire[entry_count]   # 1318 bytes each
//!     u16 enum_count
//!     EnumWire[enum_count]      # 2053 bytes each
//!     u16 bitfield_count
//!     BitfieldWire[bf_count]    # 1093 bytes each
//!
//! repeated:
//!     u32 packet_len
//!     [u8; packet_len] packet:
//!         PacketHeader              # 16 bytes
//!         EntryHeader[entry_count]  # 16 bytes each
//!         payload_buffer            # variable; offsets into here
//! ```
//!
//! See `include/btelem/btelem_types.h` for the authoritative definitions.

#![forbid(unsafe_code)]

use thiserror::Error;

pub mod packet;
pub mod schema;
pub mod value;

pub use packet::{decode_packet, DecodedEntry, Packet, PacketHeader};
pub use schema::{BitDef, BitfieldDef, EnumDef, FieldDef, FieldType, Schema, SchemaEntry};
pub use value::field_as_f64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("buffer too short: need {need} bytes, got {got}")]
    Short { need: usize, got: usize },
    #[error("unsupported endianness: {0}")]
    BadEndian(u8),
    #[error("invalid utf-8 in name field")]
    BadName,
    #[error("entry_count {0} exceeds maximum")]
    TooManyEntries(u16),
    #[error("field_count {0} exceeds maximum")]
    TooManyFields(u16),
    #[error("payload_offset {offset}+{size} out of bounds for buffer of {buf}")]
    PayloadOob {
        offset: usize,
        size: usize,
        buf: usize,
    },
    #[error("unknown field type {0}")]
    UnknownType(u8),
}

pub type Result<T> = std::result::Result<T, WireError>;

// Wire-format constants — must match include/btelem/btelem_types.h
pub const NAME_MAX: usize = 64;
pub const DESC_MAX: usize = 128;
pub const MAX_FIELDS: usize = 16;
pub const MAX_SCHEMA_ENTRIES: usize = 256;
pub const ENUM_LABEL_MAX: usize = 32;
pub const ENUM_MAX_VALUES: usize = 64;
pub const BITFIELD_MAX_BITS: usize = 32;
pub const BIT_NAME_MAX: usize = 32;

pub const SCHEMA_HEADER_SIZE: usize = 3;
pub const FIELD_WIRE_SIZE: usize = NAME_MAX + 2 + 2 + 1 + 1; // 70
pub const SCHEMA_WIRE_HEADER_SIZE: usize = 2 + 2 + 2 + NAME_MAX + DESC_MAX; // 198
pub const SCHEMA_WIRE_SIZE: usize = SCHEMA_WIRE_HEADER_SIZE + MAX_FIELDS * FIELD_WIRE_SIZE; // 1318
pub const ENUM_WIRE_SIZE: usize = 2 + 2 + 1 + ENUM_MAX_VALUES * ENUM_LABEL_MAX; // 2053
pub const BITFIELD_WIRE_SIZE: usize =
    2 + 2 + 1 + BITFIELD_MAX_BITS * BIT_NAME_MAX + BITFIELD_MAX_BITS + BITFIELD_MAX_BITS; // 1093

pub const PACKET_HEADER_SIZE: usize = 16;
pub const ENTRY_HEADER_SIZE: usize = 16;

// Compile-time sanity vs the C header.
const _: () = assert!(FIELD_WIRE_SIZE == 70);
const _: () = assert!(SCHEMA_WIRE_SIZE == 1318);
const _: () = assert!(ENUM_WIRE_SIZE == 2053);
const _: () = assert!(BITFIELD_WIRE_SIZE == 1093);

// --- low-level LE helpers ---

#[inline]
pub(crate) fn need(buf: &[u8], n: usize) -> Result<()> {
    if buf.len() < n {
        Err(WireError::Short {
            need: n,
            got: buf.len(),
        })
    } else {
        Ok(())
    }
}

#[inline]
pub(crate) fn read_u8(buf: &[u8], off: usize) -> Result<u8> {
    need(buf, off + 1)?;
    Ok(buf[off])
}

#[inline]
pub(crate) fn read_u16(buf: &[u8], off: usize) -> Result<u16> {
    need(buf, off + 2)?;
    Ok(u16::from_le_bytes([buf[off], buf[off + 1]]))
}

#[inline]
pub(crate) fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    need(buf, off + 4)?;
    Ok(u32::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
    ]))
}

#[inline]
pub(crate) fn read_u64(buf: &[u8], off: usize) -> Result<u64> {
    need(buf, off + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    Ok(u64::from_le_bytes(a))
}

#[inline]
pub(crate) fn read_cstr(buf: &[u8], off: usize, max: usize) -> Result<String> {
    need(buf, off + max)?;
    let slice = &buf[off..off + max];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end])
        .map(str::to_owned)
        .map_err(|_| WireError::BadName)
}
