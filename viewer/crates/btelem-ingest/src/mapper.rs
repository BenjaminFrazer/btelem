//! Schema → store channel mapping.

use std::collections::HashMap;

use btelem_store::{ChannelId, MockStore};
use btelem_wire::{BitDef, FieldDef, FieldType, Schema, SchemaEntry};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MapError {
    #[error("duplicate schema id {0}")]
    DuplicateSchema(u16),
}

#[derive(Default)]
pub struct ChannelMap {
    entries: HashMap<u16, EntryMap>,
}

struct EntryMap {
    fields: Vec<FieldKind>,
}

enum FieldKind {
    Scalar {
        ch: ChannelId,
        ty: FieldType,
        offset: u16,
        size: u16,
    },
    ScalarArray {
        ty: FieldType,
        base_offset: u16,
        elem_size: u16,
        channels: Vec<ChannelId>,
    },
    State {
        ch: ChannelId,
        offset: u16,
    },
    Bitfield {
        offset: u16,
        storage_bytes: u16,
        bits: Vec<(BitDef, ChannelId)>,
    },
    Ignored,
}

impl ChannelMap {
    pub fn build(schema: &Schema, store: &MockStore) -> Result<Self, MapError> {
        let mut map = ChannelMap::default();
        for entry in &schema.entries {
            if map.entries.contains_key(&entry.id) {
                return Err(MapError::DuplicateSchema(entry.id));
            }
            map.entries
                .insert(entry.id, build_entry(entry, schema, store));
        }
        Ok(map)
    }

    pub fn dispatch(&self, schema_id: u16, t: u64, payload: &[u8], store: &MockStore) {
        let Some(em) = self.entries.get(&schema_id) else {
            return;
        };
        for fk in &em.fields {
            match fk {
                FieldKind::Scalar {
                    ch,
                    ty,
                    offset,
                    size,
                } => {
                    if let Some(v) = read_scalar(*ty, payload, *offset, *size) {
                        store.push_scalar(*ch, t, v);
                    }
                }
                FieldKind::ScalarArray {
                    ty,
                    base_offset,
                    elem_size,
                    channels,
                } => {
                    for (i, ch) in channels.iter().enumerate() {
                        let off = *base_offset + (i as u16) * *elem_size;
                        if let Some(v) = read_scalar(*ty, payload, off, *elem_size) {
                            store.push_scalar(*ch, t, v);
                        }
                    }
                }
                FieldKind::State { ch, offset } => {
                    let off = *offset as usize;
                    if off < payload.len() {
                        store.push_state(*ch, t, payload[off] as u32);
                    }
                }
                FieldKind::Bitfield {
                    offset,
                    storage_bytes,
                    bits,
                } => {
                    let off = *offset as usize;
                    let n = *storage_bytes as usize;
                    if off + n <= payload.len() {
                        let mut raw = 0u64;
                        for (i, b) in payload[off..off + n].iter().enumerate() {
                            raw |= (*b as u64) << (i * 8);
                        }
                        for (bit, ch) in bits {
                            let mask = ((1u64 << bit.width) - 1) << bit.start;
                            let v = ((raw & mask) >> bit.start) as u32;
                            store.push_state(*ch, t, v);
                        }
                    }
                }
                FieldKind::Ignored => {}
            }
        }
    }
}

fn build_entry(entry: &SchemaEntry, schema: &Schema, store: &MockStore) -> EntryMap {
    let mut fields = Vec::with_capacity(entry.fields.len());
    for (fi, f) in entry.fields.iter().enumerate() {
        fields.push(build_field(entry.id, fi as u16, f, schema, store));
    }
    EntryMap { fields }
}

fn build_field(
    schema_id: u16,
    field_index: u16,
    f: &FieldDef,
    schema: &Schema,
    store: &MockStore,
) -> FieldKind {
    let entry_name = schema
        .entry(schema_id)
        .map(|e| e.name.as_str())
        .unwrap_or("?")
        .to_owned();
    let path = |suffix: &str| {
        if suffix.is_empty() {
            format!("{entry_name}.{}", f.name)
        } else {
            format!("{entry_name}.{}{suffix}", f.name)
        }
    };

    let is_numeric = matches!(
        f.ty,
        FieldType::U8
            | FieldType::U16
            | FieldType::U32
            | FieldType::U64
            | FieldType::I8
            | FieldType::I16
            | FieldType::I32
            | FieldType::I64
            | FieldType::F32
            | FieldType::F64
    );
    if is_numeric {
        let elem_size = if f.count > 0 {
            f.size / f.count as u16
        } else {
            f.size
        };
        if f.count == 1 {
            let ch = store.add_scalar(path(""));
            return FieldKind::Scalar {
                ch,
                ty: f.ty,
                offset: f.offset,
                size: elem_size,
            };
        }
        let channels: Vec<ChannelId> = (0..f.count as usize)
            .map(|i| store.add_scalar(path(&format!("[{i}]"))))
            .collect();
        return FieldKind::ScalarArray {
            ty: f.ty,
            base_offset: f.offset,
            elem_size,
            channels,
        };
    }
    if f.count != 1 {
        return FieldKind::Ignored;
    }
    match f.ty {
        FieldType::Bool => {
            let ch = store.add_state(path(""), &["false", "true"]);
            FieldKind::State {
                ch,
                offset: f.offset,
            }
        }
        FieldType::Enum => {
            let labels = schema
                .enum_labels(schema_id, field_index)
                .map(|v| v.iter().map(String::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let ch = store.add_state(path(""), &labels);
            FieldKind::State {
                ch,
                offset: f.offset,
            }
        }
        FieldType::Bitfield => {
            let Some(bf) = schema.bitfield(schema_id, field_index) else {
                return FieldKind::Ignored;
            };
            let bits: Vec<(BitDef, ChannelId)> = bf
                .bits
                .iter()
                .map(|b| {
                    let labels: Vec<String> =
                        (0u32..(1u32 << b.width)).map(|v| v.to_string()).collect();
                    let labels_ref: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let ch = store.add_state(path(&format!(".{}", b.name)), &labels_ref);
                    (b.clone(), ch)
                })
                .collect();
            FieldKind::Bitfield {
                offset: f.offset,
                storage_bytes: f.size,
                bits,
            }
        }
        _ => FieldKind::Ignored,
    }
}

fn read_scalar(ty: FieldType, payload: &[u8], offset: u16, size: u16) -> Option<f64> {
    let off = offset as usize;
    let n = size as usize;
    if off + n > payload.len() {
        return None;
    }
    let s = &payload[off..off + n];
    Some(match ty {
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
        _ => return None,
    })
}
