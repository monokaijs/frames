//! Deterministic packet fixtures use a virtual arrival clock, not timing sleeps.
use frames_core::{media::*, model::WindowId};
use std::time::{Duration, Instant};

fn frame(number: u64, length: usize) -> EncodedFrame {
    EncodedFrame {
        window: WindowId {
            id: 31,
            generation: 4,
        },
        generation: 9,
        number,
        captured_us: 1234,
        encoded_us: 5678,
        keyframe: number.is_multiple_of(60),
        bytes: (0..length)
            .map(|i| (i.wrapping_mul(37) % 251) as u8)
            .collect(),
    }
}
#[test]
fn virtual_motion_with_bursts_parity_loss_duplicates_and_mtu_changes() {
    let mut receiver = Reassembler::default();
    let start = Instant::now();
    // Different frame sizes cover tiny partial blocks and multiple striped blocks.
    let frames = if cfg!(debug_assertions) { 600 } else { 36_000 };
    for number in 0..frames {
        let source = frame(number, [37, 5_003, 25_000, 90_000][number as usize % 4]);
        let mtu = [1200, 1181, 960][number as usize % 3];
        let packets = fragment_fec(
            &source,
            mtu,
            FecProfile { parity_per_32: 8 },
            source.captured_us,
        )
        .unwrap();
        let at = start + Duration::from_micros(number * 16_667);
        let mut delivered = None;
        let mut erased = std::collections::HashMap::<u16, usize>::new();
        // Erase a four-packet burst, within each block's parity allowance.
        let packets: Vec<_> = packets
            .into_iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let block = u16::from_be_bytes([p[40], p[41]]);
                let lost = erased.entry(block).or_default();
                if i < 4 && *lost < p[45] as usize {
                    *lost += 1;
                    None
                } else {
                    Some(p)
                }
            })
            .collect();
        // Reverse short groups to model jitter, with duplicate deliveries.
        for (group, chunk) in packets.chunks(5).enumerate() {
            for packet in chunk.iter().rev() {
                let time = at + Duration::from_micros(100 * group as u64);
                if let Some(done) = receiver.push(packet, time) {
                    assert!(delivered.replace(done).is_none());
                }
                assert!(receiver.push(packet, time).is_none());
            }
        }
        let delivered = delivered.expect("loss within parity capacity must not need recovery");
        assert_eq!(delivered.bytes, source.bytes);
        assert_eq!(delivered.encoded_us, source.encoded_us);
        receiver.expire(at + Duration::from_millis(51));
        assert!(receiver.take_expired().is_empty());
        assert!(receiver.retained_bytes() < MAX_REASSEMBLY_BYTES);
    }
}
#[test]
fn parity_only_loss_is_reported_and_an_over_capacity_frame_cannot_resurrect() {
    let now = Instant::now();
    let source = frame(1, 20_000);
    let packets = fragment(&source, 1200).unwrap();
    let mut receiver = Reassembler::default();
    let mut done = None;
    for p in packets.iter().filter(|p| p[46] < p[44]) {
        done = receiver.push(p, now).or(done);
    }
    assert_eq!(done.unwrap().bytes, source.bytes);
    receiver.expire(now + Duration::from_millis(26));
    assert_eq!(
        receiver.stats(source.window).missing_shards,
        packets.iter().filter(|p| p[46] >= p[44]).count() as u64
    );
    let source = frame(2, 20_000);
    let packets = fragment(&source, 1200).unwrap();
    receiver.push(&packets[0], now);
    receiver.expire(now + Duration::from_millis(26));
    for p in &packets {
        assert!(receiver.push(p, now + Duration::from_millis(27)).is_none());
    }
    assert_eq!(receiver.take_expired().len(), 1);
}
#[test]
fn arbitrary_header_mutations_never_allocate_above_the_session_budget() {
    let packets = fragment(&frame(1, 90_000), 1200).unwrap();
    let mut receiver = Reassembler::default();
    let now = Instant::now();
    for index in 0..HEADER_BYTES {
        for value in [0, 1, 7, 127, 255] {
            let mut p = packets[0].clone();
            p[index] = value;
            receiver.push(&p, now);
            assert!(receiver.retained_bytes() <= MAX_REASSEMBLY_BYTES);
        }
    }
}
