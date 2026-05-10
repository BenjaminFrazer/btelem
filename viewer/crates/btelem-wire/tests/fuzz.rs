//! Fuzz-style: random byte slices must never panic the decoders.

use btelem_wire::{decode_packet, Schema};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn schema_decode_does_not_panic(data: Vec<u8>) {
        let _ = Schema::decode(&data);
    }

    #[test]
    fn packet_decode_does_not_panic(data: Vec<u8>) {
        let _ = decode_packet(&data);
    }
}
