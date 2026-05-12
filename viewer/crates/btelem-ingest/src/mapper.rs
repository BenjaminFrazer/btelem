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
        /// Scalar channel for the whole word (integer storage). `None`
        /// when `storage_bytes` is not a supported width (1/2/4/8); in
        /// that case only the per-bit lanes are dispatched.
        word: Option<ChannelId>,
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
                    word,
                    bits,
                } => {
                    let off = *offset as usize;
                    let n = *storage_bytes as usize;
                    if off + n <= payload.len() {
                        // Read the storage word once; bits are masked from
                        // this value rather than re-reading the payload.
                        let mut raw = 0u64;
                        for (i, b) in payload[off..off + n].iter().enumerate() {
                            raw |= (*b as u64) << (i * 8);
                        }
                        if let Some(ch) = word {
                            store.push_scalar(*ch, t, raw as f64);
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
    let is_integer = matches!(
        f.ty,
        FieldType::U8
            | FieldType::U16
            | FieldType::U32
            | FieldType::U64
            | FieldType::I8
            | FieldType::I16
            | FieldType::I32
            | FieldType::I64
    );
    let add = |p: String| {
        if is_integer {
            store.add_scalar_int(p)
        } else {
            store.add_scalar(p)
        }
    };
    if is_numeric {
        let elem_size = if f.count > 0 {
            f.size / f.count as u16
        } else {
            f.size
        };
        if f.count == 1 {
            let ch = add(path(""));
            return FieldKind::Scalar {
                ch,
                ty: f.ty,
                offset: f.offset,
                size: elem_size,
            };
        }
        let channels: Vec<ChannelId> = (0..f.count as usize)
            .map(|i| add(path(&format!("[{i}]"))))
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
            let word = match f.size {
                1 | 2 | 4 | 8 => Some(store.add_scalar_int(path(""))),
                other => {
                    eprintln!(
                        "btelem-ingest: bitfield {}.{} has unsupported storage width {} bytes; skipping word channel",
                        entry_name, f.name, other
                    );
                    None
                }
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
                word,
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

#[cfg(test)]
mod tests {
    use super::*;
    use btelem_store::{ChannelInfo, ChannelKind, Store};
    use btelem_wire::{BitDef, BitfieldDef, FieldDef, FieldType, Schema, SchemaEntry};

    fn bf_schema() -> Schema {
        Schema {
            entries: vec![SchemaEntry {
                id: 1,
                name: "sys".into(),
                description: String::new(),
                payload_size: 4,
                fields: vec![FieldDef {
                    name: "flags".into(),
                    offset: 0,
                    size: 4,
                    ty: FieldType::Bitfield,
                    count: 1,
                }],
            }],
            enums: Vec::new(),
            bitfields: vec![BitfieldDef {
                schema_id: 1,
                field_index: 0,
                bits: vec![
                    BitDef {
                        name: "ready".into(),
                        start: 0,
                        width: 1,
                    },
                    BitDef {
                        name: "mode".into(),
                        start: 1,
                        width: 2,
                    },
                    BitDef {
                        name: "err".into(),
                        start: 3,
                        width: 1,
                    },
                ],
            }],
        }
    }

    fn find_ch<'a>(channels: &'a [ChannelInfo], path: &str) -> &'a ChannelInfo {
        channels
            .iter()
            .find(|c| c.path == path)
            .unwrap_or_else(|| panic!("channel {path} not registered"))
    }

    #[test]
    fn bitfield_registers_word_and_bits() {
        let schema = bf_schema();
        let store = MockStore::new();
        let map = ChannelMap::build(&schema, &store).unwrap();

        let channels = store.channels();
        let word = find_ch(&channels, "sys.flags");
        assert!(matches!(word.kind, ChannelKind::Scalar));
        assert!(word.integer_storage, "bitfield word must be integer-storage");

        let ready = find_ch(&channels, "sys.flags.ready");
        let mode = find_ch(&channels, "sys.flags.mode");
        let err = find_ch(&channels, "sys.flags.err");
        assert!(matches!(ready.kind, ChannelKind::State { .. }));
        assert!(matches!(mode.kind, ChannelKind::State { .. }));
        assert!(matches!(err.kind, ChannelKind::State { .. }));

        // word = 0b1011 = 11: ready=1, mode=01=1, err=1
        let payload = 0b1011u32.to_le_bytes();
        map.dispatch(1, 100, &payload, &store);
        assert_eq!(store.sample_at(word.id, 100), Some(11.0));
        assert_eq!(store.sample_at(ready.id, 100), Some(1.0));
        assert_eq!(store.sample_at(mode.id, 100), Some(1.0));
        assert_eq!(store.sample_at(err.id, 100), Some(1.0));

        // word = 0b0100 = 4: ready=0, mode=10=2, err=0
        let payload = 0b0100u32.to_le_bytes();
        map.dispatch(1, 200, &payload, &store);
        assert_eq!(store.sample_at(word.id, 200), Some(4.0));
        assert_eq!(store.sample_at(ready.id, 200), Some(0.0));
        assert_eq!(store.sample_at(mode.id, 200), Some(2.0));
        assert_eq!(store.sample_at(err.id, 200), Some(0.0));
    }

    #[test]
    fn bitfield_word_consistent_with_bits() {
        // The dispatch path reads the storage word exactly once and derives
        // every bit channel from it (see FieldKind::Bitfield branch). This
        // test asserts the invariant `word == reassembled(bits)` for several
        // payloads; an accidental re-read of the payload per bit would be
        // visually invisible here but still wrong.
        let schema = bf_schema();
        let store = MockStore::new();
        let map = ChannelMap::build(&schema, &store).unwrap();
        let channels = store.channels();
        let word = find_ch(&channels, "sys.flags").id;
        let ready = find_ch(&channels, "sys.flags.ready").id;
        let mode = find_ch(&channels, "sys.flags.mode").id;
        let err = find_ch(&channels, "sys.flags.err").id;

        for (t, raw) in [(10u64, 0u32), (20, 0b0111), (30, 0b1011), (40, 0b1110)] {
            map.dispatch(1, t, &raw.to_le_bytes(), &store);
            let w = store.sample_at(word, t).unwrap() as u32;
            let r = store.sample_at(ready, t).unwrap() as u32;
            let m = store.sample_at(mode, t).unwrap() as u32;
            let e = store.sample_at(err, t).unwrap() as u32;
            assert_eq!(w, raw, "word channel must mirror raw storage");
            assert_eq!(r | (m << 1) | (e << 3), raw, "bits must reconstruct word");
        }
    }

    #[test]
    fn bitfield_unsupported_width_skips_word_keeps_bits() {
        // 3-byte storage is not 1/2/4/8 — word is skipped but bits still register.
        let schema = Schema {
            entries: vec![SchemaEntry {
                id: 2,
                name: "weird".into(),
                description: String::new(),
                payload_size: 3,
                fields: vec![FieldDef {
                    name: "f".into(),
                    offset: 0,
                    size: 3,
                    ty: FieldType::Bitfield,
                    count: 1,
                }],
            }],
            enums: Vec::new(),
            bitfields: vec![BitfieldDef {
                schema_id: 2,
                field_index: 0,
                bits: vec![BitDef {
                    name: "b0".into(),
                    start: 0,
                    width: 1,
                }],
            }],
        };
        let store = MockStore::new();
        let _map = ChannelMap::build(&schema, &store).unwrap();
        let channels = store.channels();
        assert!(channels.iter().any(|c| c.path == "weird.f.b0"));
        assert!(
            !channels.iter().any(|c| c.path == "weird.f"),
            "word channel must be skipped for unsupported widths"
        );
    }
}
