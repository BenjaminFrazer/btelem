//! Round-trip tests against synthetically-built schema and packet blobs.
//! These exercise the exact byte layout used by the C side.

use btelem_wire::{
    decode_packet, FieldType, Schema, BITFIELD_WIRE_SIZE, ENUM_WIRE_SIZE, FIELD_WIRE_SIZE,
    SCHEMA_HEADER_SIZE, SCHEMA_WIRE_HEADER_SIZE, SCHEMA_WIRE_SIZE,
};

fn write_cstr(buf: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&bytes[..n]);
}

fn build_schema_blob() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0); // little
    buf.extend_from_slice(&1u16.to_le_bytes()); // entry_count

    let mut entry = vec![0u8; SCHEMA_WIRE_SIZE];
    entry[0..2].copy_from_slice(&0u16.to_le_bytes());
    entry[2..4].copy_from_slice(&32u16.to_le_bytes());
    entry[4..6].copy_from_slice(&8u16.to_le_bytes());
    write_cstr(&mut entry[6..6 + 64], "counters");
    write_cstr(
        &mut entry[6 + 64..6 + 64 + 128],
        "Staggered uint32 counters",
    );

    let fpos = SCHEMA_WIRE_HEADER_SIZE;
    for i in 0..8u16 {
        let off = fpos + i as usize * FIELD_WIRE_SIZE;
        let name = format!("c{i}");
        write_cstr(&mut entry[off..off + 64], &name);
        entry[off + 64..off + 66].copy_from_slice(&(i * 4).to_le_bytes());
        entry[off + 66..off + 68].copy_from_slice(&4u16.to_le_bytes());
        entry[off + 68] = FieldType::U32 as u8;
        entry[off + 69] = 1;
    }
    buf.extend_from_slice(&entry);

    // enum section
    buf.extend_from_slice(&1u16.to_le_bytes());
    let mut e = vec![0u8; ENUM_WIRE_SIZE];
    e[0..2].copy_from_slice(&99u16.to_le_bytes());
    e[2..4].copy_from_slice(&0u16.to_le_bytes());
    e[4] = 3;
    let labels = ["idle", "run", "fault"];
    for (i, l) in labels.iter().enumerate() {
        let off = 5 + i * 32;
        write_cstr(&mut e[off..off + 32], l);
    }
    buf.extend_from_slice(&e);

    // bitfield section
    buf.extend_from_slice(&1u16.to_le_bytes());
    let mut b = vec![0u8; BITFIELD_WIRE_SIZE];
    b[0..2].copy_from_slice(&42u16.to_le_bytes());
    b[2..4].copy_from_slice(&0u16.to_le_bytes());
    b[4] = 2;
    let names_pos = 5usize;
    let starts_pos = names_pos + 32 * 32;
    let widths_pos = starts_pos + 32;
    write_cstr(&mut b[names_pos..names_pos + 32], "fault_a");
    write_cstr(&mut b[names_pos + 32..names_pos + 64], "fault_b");
    b[starts_pos] = 0;
    b[starts_pos + 1] = 1;
    b[widths_pos] = 1;
    b[widths_pos + 1] = 1;
    buf.extend_from_slice(&b);

    buf
}

#[test]
fn schema_round_trip_full() {
    let blob = build_schema_blob();
    let s = Schema::decode(&blob).expect("decode");

    assert_eq!(s.entries.len(), 1);
    let e = &s.entries[0];
    assert_eq!(e.id, 0);
    assert_eq!(e.name, "counters");
    assert_eq!(e.payload_size, 32);
    assert_eq!(e.fields.len(), 8);
    for (i, f) in e.fields.iter().enumerate() {
        assert_eq!(f.name, format!("c{i}"));
        assert_eq!(f.offset, (i as u16) * 4);
        assert_eq!(f.size, 4);
        assert_eq!(f.ty, FieldType::U32);
        assert_eq!(f.count, 1);
    }

    let labels = s.enum_labels(99, 0).expect("enum");
    assert_eq!(labels, &["idle", "run", "fault"]);

    let bf = s.bitfield(42, 0).expect("bitfield");
    assert_eq!(bf.bits.len(), 2);
    assert_eq!(bf.bits[0].name, "fault_a");
    assert_eq!(bf.bits[1].start, 1);
}

#[test]
fn schema_decode_rejects_bad_endian() {
    let mut blob = build_schema_blob();
    blob[0] = 1;
    assert!(Schema::decode(&blob).is_err());
}

#[test]
fn schema_decode_rejects_short() {
    assert!(Schema::decode(&[]).is_err());
    assert!(Schema::decode(&[0u8; SCHEMA_HEADER_SIZE - 1]).is_err());
}

fn build_packet() -> Vec<u8> {
    let entry_count = 2u16;
    let payload_size = 8u32;
    let mut buf = Vec::new();
    buf.extend_from_slice(&entry_count.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&payload_size.to_le_bytes());
    buf.extend_from_slice(&7u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());

    let entries = [
        (0u16, 0u32, 1_000u64, 0xdeadbeefu32),
        (0, 4, 2_000, 0xcafebabe),
    ];
    for (id, poff, ts, _) in entries {
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(&4u16.to_le_bytes());
        buf.extend_from_slice(&poff.to_le_bytes());
        buf.extend_from_slice(&ts.to_le_bytes());
    }
    for (_, _, _, v) in entries {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

#[test]
fn packet_round_trip() {
    let buf = build_packet();
    let pkt = decode_packet(&buf).expect("decode");
    assert_eq!(pkt.header.entry_count, 2);
    assert_eq!(pkt.header.dropped, 7);
    assert_eq!(pkt.entries.len(), 2);
    assert_eq!(pkt.entries[0].id, 0);
    assert_eq!(pkt.entries[0].timestamp, 1_000);
    assert_eq!(pkt.entries[0].payload, 0xdeadbeefu32.to_le_bytes());
    assert_eq!(pkt.entries[1].timestamp, 2_000);
    assert_eq!(pkt.entries[1].payload, 0xcafebabeu32.to_le_bytes());
}

#[test]
fn packet_oob_payload_rejected() {
    let mut buf = build_packet();
    let off = 16 + 16 + 4;
    buf[off..off + 4].copy_from_slice(&999u32.to_le_bytes());
    assert!(decode_packet(&buf).is_err());
}

#[test]
fn packet_short_buffer_does_not_panic() {
    let buf = build_packet();
    for n in 0..buf.len() {
        let _ = decode_packet(&buf[..n]);
    }
}
