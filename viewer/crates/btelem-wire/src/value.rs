//! Field-value extraction.
//!
//! Numeric scalar fields collapse to `f64` per the viewer contract. Bytes /
//! arrays / bitfields are not handled here — the ingest layer interprets those.

use crate::{FieldDef, FieldType};

/// Read a scalar field as `f64`. Returns `None` if the payload is too short,
/// the field is an array (count > 1), or the type is non-numeric (Bytes,
/// Bitfield — which require multi-channel expansion).
pub fn field_as_f64(field: &FieldDef, payload: &[u8]) -> Option<f64> {
    if field.count != 1 {
        return None;
    }
    let off = field.offset as usize;
    let size = field.size as usize;
    if off + size > payload.len() {
        return None;
    }
    let s = &payload[off..off + size];
    Some(match field.ty {
        FieldType::U8 => s[0] as f64,
        FieldType::U16 => u16::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::U32 => u32::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::U64 => u64::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::I8 => s[0] as i8 as f64,
        FieldType::I16 => i16::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::I32 => i32::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::I64 => i64::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::F32 => f32::from_le_bytes(s.try_into().ok()?) as f64,
        FieldType::F64 => f64::from_le_bytes(s.try_into().ok()?),
        FieldType::Bool => (s[0] != 0) as u8 as f64,
        FieldType::Enum => s[0] as f64,
        FieldType::Bytes | FieldType::Bitfield | FieldType::String => return None,
    })
}

/// Read a string field as a Rust `String`. Returns `None` if the payload
/// is too short or the field is not a `String` type. The raw bytes are
/// truncated at the first null byte and decoded as UTF-8 (lossy).
pub fn field_as_string(field: &FieldDef, payload: &[u8]) -> Option<String> {
    if field.ty != FieldType::String {
        return None;
    }
    let off = field.offset as usize;
    let size = field.size as usize;
    if off + size > payload.len() {
        return None;
    }
    let raw = &payload[off..off + size];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}
