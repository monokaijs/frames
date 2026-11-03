use super::*;

fn video(seq: u32, shard: &[u8]) -> (MediaHeader, Vec<u8>) {
    let header = MediaHeader {
        kind: MediaKind::Video,
        flags: MediaFlags::END_OF_FRAME,
        stream: 0x0102_0304,
        generation: 7,
        seq,
        send_us: 0xFFFF_FFF0,
        frame: 42,
        capture_us: 123_456,
        slice: 3,
        slices: 4,
        reference: Reference::Back(1),
        mark_long_term: Some(2),
        slice_len: 2_000,
        block: 1,
        blocks: 2,
        shard: 33,
        data_shards: 32,
        parity_shards: 4,
        shard_len: shard.len() as u16,
    };
    let mut bytes = Vec::new();
    header.encode(shard, &mut bytes).unwrap();
    (header, bytes)
}

fn feedback() -> Feedback {
    Feedback {
        feedback_seq: 9,
        base_seq: u32::MAX - 3,
        reference_us: 77,
        arrivals: vec![
            Some(0),
            None,
            Some(320),
            Some(-64),
            None,
            None,
            None,
            None,
            Some(ARRIVAL_TICK_US * i64::from(i16::MAX) - 64),
        ],
        nacks: vec![SeqRange {
            first: u32::MAX - 2,
            len: 1,
        }],
        streams: vec![
            StreamState {
                stream: 1,
                generation: 7,
                last_decoded: 40,
                long_term: Some((1, 30)),
                request: RecoveryRequest::Invalidate {
                    first: 41,
                    last: 42,
                },
            },
            StreamState {
                stream: 2,
                generation: 1,
                last_decoded: 0,
                long_term: None,
                request: RecoveryRequest::Keyframe,
            },
        ],
    }
}

#[test]
fn media_header_is_41_bytes_and_round_trips() {
    let shard = [0xAB; 100];
    let (header, bytes) = video(5, &shard);
    assert_eq!(bytes.len(), MEDIA_HEADER_BYTES + shard.len());
    assert_eq!(bytes[0], 0x21, "version 2, video");
    let Packet::Media(parsed, payload) = parse(&bytes).unwrap() else {
        panic!("expected media");
    };
    assert_eq!(parsed, header);
    assert_eq!(payload, shard);
    assert!(parsed.is_parity());
    assert_eq!(parse(&bytes).unwrap().seq(), Some(5));
}

#[test]
fn media_validation_rejects_inconsistent_headers() {
    let shard = [1u8; 64];
    let (base, _) = video(1, &shard);
    let cases: Vec<(&str, MediaHeader)> = vec![
        ("slice", MediaHeader { slice: 4, ..base }),
        ("block", MediaHeader { block: 2, ..base }),
        ("shard", MediaHeader { shard: 36, ..base }),
        (
            "data shards",
            MediaHeader {
                data_shards: 33,
                ..base
            },
        ),
        (
            "parity",
            MediaHeader {
                parity_shards: 33,
                ..base
            },
        ),
        (
            "zero back",
            MediaHeader {
                reference: Reference::Back(0),
                ..base
            },
        ),
        (
            "slot",
            MediaHeader {
                reference: Reference::LongTerm { slot: 16, back: 1 },
                ..base
            },
        ),
        (
            "mark",
            MediaHeader {
                mark_long_term: Some(16),
                ..base
            },
        ),
        (
            "keyframe ref",
            MediaHeader {
                flags: MediaFlags::KEYFRAME,
                ..base
            },
        ),
        (
            "slice len",
            MediaHeader {
                slice_len: 0,
                ..base
            },
        ),
        (
            "capacity",
            MediaHeader {
                slice_len: 2 * 32 * u32::from(base.shard_len) + 1,
                ..base
            },
        ),
        (
            "audio slices",
            MediaHeader {
                kind: MediaKind::Audio,
                reference: Reference::None,
                mark_long_term: None,
                ..base
            },
        ),
    ];
    for (name, header) in cases {
        assert!(
            matches!(
                header.encode(&shard, &mut Vec::new()),
                Err(WireError::Invalid(_))
            ),
            "{name} should be rejected"
        );
    }
    assert!(
        base.encode(&shard[..63], &mut Vec::new()).is_err(),
        "shard length"
    );

    let (_, bytes) = video(1, &shard);
    let mut flags = bytes.clone();
    flags[1] |= 0x80;
    assert!(parse(&flags).is_err(), "reserved flags");
    let mut long_term = bytes.clone();
    long_term[28] = 1;
    let Packet::Media(parsed, _) = parse(&long_term).unwrap() else {
        panic!("media");
    };
    assert_eq!(parsed.reference, Reference::LongTerm { slot: 1, back: 1 });
    long_term[26] = 0;
    assert!(
        parse(&long_term).is_err(),
        "long-term reference of zero frames back"
    );
    assert!(parse(&bytes[..bytes.len() - 1]).is_err(), "short shard");
    let mut long = bytes.clone();
    long.push(0);
    assert!(parse(&long).is_err(), "long shard");
    assert!(matches!(
        parse(&bytes[..10]),
        Err(WireError::Truncated { .. })
    ));
    let mut version = bytes;
    version[0] = 0x11;
    assert_eq!(parse(&version), Err(WireError::Version(1)));
    assert_eq!(parse(&[0x2F]), Err(WireError::UnknownKind(0xF)));
    assert!(parse(&[]).is_err());
}

#[test]
fn feedback_round_trips_with_reordering_and_exact_size() {
    let report = feedback();
    let mut bytes = Vec::new();
    report.encode(1200, &mut bytes).unwrap();
    assert_eq!(bytes.len(), report.encoded_len());
    assert_eq!(parse(&bytes).unwrap(), Packet::Feedback(report.clone()));
    assert_eq!(parse(&bytes).unwrap().seq(), None);
    assert!(matches!(
        report.encode(bytes.len() - 1, &mut Vec::new()),
        Err(WireError::TooLarge { .. })
    ));
}

#[test]
fn feedback_rejects_unrepresentable_reports() {
    let mut unquantized = feedback();
    unquantized.arrivals[0] = Some(1);
    assert!(unquantized.encode(usize::MAX, &mut Vec::new()).is_err());

    let mut gap = feedback();
    gap.arrivals = vec![Some(0), Some(ARRIVAL_TICK_US * (i64::from(i16::MAX) + 1))];
    assert!(
        gap.encode(usize::MAX, &mut Vec::new()).is_err(),
        "delta overflow"
    );

    let mut empty = feedback();
    empty.nacks[0].len = 0;
    assert!(empty.encode(usize::MAX, &mut Vec::new()).is_err());

    let mut bytes = Vec::new();
    feedback().encode(usize::MAX, &mut bytes).unwrap();
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(parse(&trailing).is_err(), "trailing bytes");
    let mut stray = bytes;
    stray[16 + 1] |= 0x80; // bitmap bit for sequence 15 of 9
    assert!(parse(&stray).is_err(), "bitmap bits past the count");
}

#[test]
fn probes_round_trip_with_zero_padding_only() {
    let probe = Probe {
        seq: 3,
        send_us: 4,
        cluster: 5,
        padding: 900,
    };
    let mut bytes = Vec::new();
    probe.encode(&mut bytes);
    assert_eq!(bytes.len(), probe.encoded_len());
    assert_eq!(parse(&bytes).unwrap(), Packet::Probe(probe));
    assert_eq!(parse(&bytes).unwrap().seq(), Some(3));
    *bytes.last_mut().unwrap() = 1;
    assert!(parse(&bytes).is_err());
}

/// Deterministic xorshift so the fuzz smoke test is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn assert_canonical(bytes: &[u8]) -> bool {
    match parse(bytes) {
        Ok(packet) => {
            let mut again = Vec::new();
            encode(&packet, &mut again).unwrap();
            assert_eq!(again, bytes, "parse is canonical for {packet:?}");
            true
        }
        Err(_) => false,
    }
}

#[test]
fn fuzz_random_datagrams_never_panic_and_parse_canonically() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut accepted = 0;
    for _ in 0..200_000 {
        let len = rng.below(1300) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if !bytes.is_empty() && rng.below(2) == 0 {
            bytes[0] = (VERSION << 4) | (1 + rng.below(4) as u8);
        }
        accepted += usize::from(assert_canonical(&bytes));
    }
    // Random bytes almost never form a valid packet; the property that
    // matters is "no panic, and anything accepted is canonical".
    assert!(accepted < 200_000);
}

#[test]
fn fuzz_mutated_valid_packets_never_panic_and_parse_canonically() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut seeds = Vec::new();
    seeds.push(video(1, &[7u8; 64]).1);
    let mut bytes = Vec::new();
    feedback().encode(usize::MAX, &mut bytes).unwrap();
    seeds.push(bytes);
    let mut probe = Vec::new();
    Probe {
        seq: 1,
        send_us: 2,
        cluster: 3,
        padding: 16,
    }
    .encode(&mut probe);
    seeds.push(probe);

    let mut accepted = 0;
    for i in 0..300_000 {
        let mut bytes = seeds[i % seeds.len()].clone();
        for _ in 0..=rng.below(3) {
            match rng.below(4) {
                0 if !bytes.is_empty() => {
                    let at = rng.below(bytes.len() as u64) as usize;
                    bytes[at] ^= 1 << rng.below(8);
                }
                1 if !bytes.is_empty() => {
                    let at = rng.below(bytes.len() as u64) as usize;
                    bytes[at] = rng.next() as u8;
                }
                2 => bytes.truncate(rng.below(bytes.len() as u64 + 1) as usize),
                _ => bytes.push(rng.next() as u8),
            }
        }
        accepted += usize::from(assert_canonical(&bytes));
    }
    assert!(
        accepted > 0,
        "some mutations stay valid (e.g. payload bytes)"
    );
}
