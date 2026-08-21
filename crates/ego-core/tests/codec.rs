//! `StoreRecord` codec v1: header/record roundtrip, checksum integrity,
//! forward compatibility for unrecognised lines.
//!
//! See `ARCHITECTURE.md` §1/§5 and `plans/m0a-contracts.md` Step 3.

use ego_core::cost::{Amount, CostAttribution, CostDelta, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{Producer, Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::types::{ErrorKind, ReasoningState, StopReason};
use ego_core::state::Usage;
use ego_core::store::codec::{
    CODEC_VERSION, CodecError, Header, IndexedCodecError, StoreLine, decode_line, decode_log,
    encode_header, encode_record,
};
use serde::{Deserialize, Serialize};

mod common;

// Wrapped in a module named `codec` so `cargo test -p ego-core codec` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches on the full test path, not the source file name, and
// `tests/codec.rs`'s bare functions wouldn't otherwise contain "codec".
mod codec {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Payload {
        kind: String,
        value: u64,
    }

    fn payload() -> Payload {
        Payload {
            kind: "tool_call_started".to_string(),
            value: 42,
        }
    }

    fn header() -> Header {
        Header::new(
            SessionId::new(),
            Timestamp::from_millis(1_787_206_085_352),
            "/ws".into(),
        )
    }

    fn seq_of(n: u64) -> Seq {
        let mut seq = Seq::default();
        for _ in 0..n {
            seq.next();
        }
        seq
    }

    /// Flip a single bit at `offset` and return the mutated copy.
    fn flip_bit(bytes: &[u8], offset: usize, mask: u8) -> Vec<u8> {
        let mut out = bytes.to_vec();
        out[offset] ^= mask;
        out
    }

    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in {haystack:?}"))
    }

    // --- roundtrip ---------------------------------------------------

    #[test]
    fn codec_header_roundtrip_encode_decode() {
        let h = header();
        let bytes = encode_header(&h);
        let line = decode_line(&bytes).unwrap();
        assert_eq!(line, StoreLine::Header(h));
    }

    /// The version is named in this test's own name on purpose: a bump has
    /// to be a deliberate edit here, not a constant quietly following the
    /// code that changed it.
    #[test]
    fn codec_header_carries_codec_version_5() {
        let h = header();
        assert_eq!(h.v, 5);
        assert_eq!(CODEC_VERSION, 5);

        let bytes = encode_header(&h);
        match decode_line(&bytes).unwrap() {
            StoreLine::Header(decoded) => assert_eq!(decoded.v, 5),
            other => panic!("expected Header, got {other:?}"),
        }
    }

    /// The field the bump to 5 exists for. A header that round-trips its
    /// version and loses its workspace would satisfy every other assertion
    /// in this file.
    #[test]
    fn codec_header_carries_the_workspace_the_session_belongs_to() {
        let bytes = encode_header(&header());
        match decode_line(&bytes).unwrap() {
            StoreLine::Header(decoded) => {
                assert_eq!(decoded.workspace_root, std::path::PathBuf::from("/ws"));
            }
            other => panic!("expected Header, got {other:?}"),
        }
    }

    #[test]
    fn codec_record_roundtrip_encode_decode() {
        let seq = seq_of(3);
        let p = payload();
        let bytes = encode_record(seq, &p).unwrap();

        match decode_line(&bytes).unwrap() {
            StoreLine::Record(record) => {
                assert_eq!(record.seq, seq);
                let decoded_payload: Payload = serde_json::from_str(record.payload.get()).unwrap();
                assert_eq!(decoded_payload, p);
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn codec_record_checksum_differs_when_seq_differs_for_the_same_payload() {
        // The checksum covers `seq` as well as `payload` (not just
        // `payload`), so two records with the same payload but different
        // seq numbers must not collide.
        let p = payload();
        let a = encode_record(seq_of(1), &p).unwrap();
        let b = encode_record(seq_of(2), &p).unwrap();
        let StoreLine::Record(record_a) = decode_line(&a).unwrap() else {
            panic!("expected Record");
        };
        let StoreLine::Record(record_b) = decode_line(&b).unwrap() else {
            panic!("expected Record");
        };
        assert_ne!(record_a.checksum, record_b.checksum);
    }

    // --- forward compatibility ----------------------------------------

    #[test]
    fn codec_unknown_record_kind_is_preserved_not_dropped() {
        let bytes = br#"{"kind":"future_blob","hash":"abc123"}"#.to_vec();
        match decode_line(&bytes).unwrap() {
            StoreLine::Unknown(raw) => assert_eq!(raw.as_bytes(), bytes.as_slice()),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn codec_decode_log_preserves_unknown_lines_in_order() {
        let h = header();
        let seq = seq_of(1);
        let p = payload();
        let mut log = encode_header(&h);
        log.push(b'\n');
        log.extend(br#"{"kind":"future_blob","hash":"abc123"}"#);
        log.push(b'\n');
        log.extend(encode_record(seq, &p).unwrap());
        log.push(b'\n');

        let lines = decode_log(&log).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], StoreLine::Header(h));
        match &lines[1] {
            StoreLine::Unknown(raw) => {
                assert_eq!(raw.as_bytes(), br#"{"kind":"future_blob","hash":"abc123"}"#)
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        match &lines[2] {
            StoreLine::Record(record) => assert_eq!(record.seq, seq),
            other => panic!("expected Record, got {other:?}"),
        }
    }

    // --- bit-flip corruption: detected, with the record index ----------

    #[test]
    fn codec_bit_flip_in_payload_is_checksum_mismatch_with_index() {
        let h = header();
        let seq = seq_of(1);
        let p = payload();
        let mut log = encode_header(&h);
        log.push(b'\n');
        let record_line = encode_record(seq, &p).unwrap();
        log.extend(&record_line);

        // Flip a bit inside the payload's "value":42 numeral — well within
        // the checksummed region, and staying valid JSON (digit -> digit)
        // is not required here since any change to these bytes changes the
        // checksummed payload text either way.
        let offset = find(&record_line, b"42");
        let corrupted = flip_bit(&record_line, offset, 0x01);
        let mut corrupted_log = encode_header(&h);
        corrupted_log.push(b'\n');
        corrupted_log.extend(corrupted);

        let err = decode_log(&corrupted_log).unwrap_err();
        assert_eq!(err.index, 1);
        assert!(
            matches!(err.error, CodecError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {:?}",
            err.error
        );
    }

    #[test]
    fn codec_bit_flip_in_checksum_field_is_checksum_mismatch_with_index() {
        let seq = seq_of(1);
        let p = payload();
        let record_line = encode_record(seq, &p).unwrap();

        // `"checksum":<digits>,"payload":...` — corrupt the checksum's last
        // digit (the byte right before the `,"payload":` separator) to a
        // different digit. Targeting the last digit rather than the first
        // keeps the corrupted value's magnitude close to the original, so
        // it cannot accidentally overflow u64 (see the dedicated overflow
        // test below for that case) and always stays a plain digit.
        let payload_key_offset = find(&record_line, b",\"payload\":");
        let last_digit_offset = payload_key_offset - 1;
        assert!(record_line[last_digit_offset].is_ascii_digit());
        let mut corrupted = record_line.clone();
        corrupted[last_digit_offset] = match record_line[last_digit_offset] {
            b'9' => b'0',
            d => d + 1,
        };

        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&corrupted);

        let err = decode_log(&log).unwrap_err();
        assert_eq!(err.index, 1);
        assert!(
            matches!(err.error, CodecError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {:?}",
            err.error
        );
    }

    #[test]
    fn codec_checksum_value_overflowing_u64_is_malformed_not_silently_unknown() {
        // Regression: some bit flips on the leading digit of `checksum`
        // produce a numeral that is syntactically valid JSON but too large
        // for u64 (as this test found while exercising the codec). Because
        // the line still carries a `checksum` key, it must be reported as a
        // corrupted *record* (a typed error), never silently reclassified
        // as `StoreLine::Unknown` just because the optimistic record parse
        // failed.
        let record_line = encode_record(seq_of(1), &payload()).unwrap();
        let key_offset = find(&record_line, b"\"checksum\":");
        let digits_start = key_offset + b"\"checksum\":".len();
        let digits_len = record_line[digits_start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .unwrap();

        let mut corrupted = record_line.clone();
        corrupted.splice(
            digits_start..digits_start + digits_len,
            b"99999999999999999999".iter().copied(), // > u64::MAX
        );

        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&corrupted);

        let err = decode_log(&log).unwrap_err();
        assert_eq!(err.index, 1);
        assert!(
            matches!(err.error, CodecError::Malformed(_)),
            "expected Malformed (not a silent Unknown), got {:?}",
            err.error
        );
    }

    #[test]
    fn codec_bit_flip_in_seq_value_breaking_json_syntax_is_detected_with_index() {
        // A bit flip that turns a `seq` digit into a non-digit character
        // breaks JSON syntax outright and is caught as a parse failure,
        // independent of the checksum.
        let seq = seq_of(12);
        let p = payload();
        let record_line = encode_record(seq, &p).unwrap();

        let key_offset = find(&record_line, b"\"seq\":");
        let digit_offset = key_offset + b"\"seq\":".len();
        assert_eq!(
            record_line[digit_offset], b'1',
            "expected the leading digit of seq=12"
        );
        // '1' (0x31) ^ 0x10 = '!' (0x21): not a JSON digit, breaks syntax.
        let corrupted = flip_bit(&record_line, digit_offset, 0x10);
        assert_eq!(corrupted[digit_offset], b'!');

        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&corrupted);

        let err = decode_log(&log).unwrap_err();
        assert_eq!(err.index, 1);
        assert!(
            matches!(err.error, CodecError::Malformed(_)),
            "expected Malformed, got {:?}",
            err.error
        );
    }

    #[test]
    fn codec_bit_flip_in_seq_digit_staying_valid_json_is_checksum_mismatch_with_index() {
        // The checksum covers `seq` as well as `payload` (see
        // `StoreRecord`'s doc comment): a flip that turns one valid digit
        // into another (seq=12 -> seq=13) stays syntactically valid JSON,
        // so this is only caught because `seq`'s bytes are hashed too — a
        // payload-only checksum would silently accept this.
        let seq = seq_of(12);
        let p = payload();
        let record_line = encode_record(seq, &p).unwrap();

        let key_offset = find(&record_line, b"\"seq\":");
        let digit_offset = key_offset + b"\"seq\":".len() + 1; // second digit of "12"
        assert_eq!(record_line[digit_offset], b'2');
        // '2' (0x32) ^ 0x01 = '3' (0x33): still a digit, still valid JSON.
        let corrupted = flip_bit(&record_line, digit_offset, 0x01);
        assert_eq!(corrupted[digit_offset], b'3');

        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&corrupted);

        let err = decode_log(&log).unwrap_err();
        assert_eq!(err.index, 1);
        assert!(
            matches!(err.error, CodecError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {:?}",
            err.error
        );
    }

    // --- malformed / truncated -----------------------------------------

    #[test]
    fn codec_truncated_line_is_typed_error_not_panic() {
        let record_line = encode_record(seq_of(1), &payload()).unwrap();
        let truncated = &record_line[..record_line.len() / 2];
        let err = decode_line(truncated).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn codec_empty_line_is_typed_error_not_panic() {
        let err = decode_line(b"").unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn codec_non_json_line_is_typed_error_not_panic() {
        let err = decode_line(b"not json at all").unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn codec_decode_log_reports_index_of_first_bad_line() {
        let good = encode_record(seq_of(1), &payload()).unwrap();
        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&good);
        log.push(b'\n');
        log.extend(b"not json at all");

        let err: IndexedCodecError = decode_log(&log).unwrap_err();
        assert_eq!(err.index, 2);
        assert!(matches!(err.error, CodecError::Malformed(_)));
    }

    #[test]
    fn codec_decode_log_tolerates_single_trailing_newline() {
        let good = encode_record(seq_of(1), &payload()).unwrap();
        let mut log = encode_header(&header());
        log.push(b'\n');
        log.extend(&good);
        log.push(b'\n');

        let lines = decode_log(&log).unwrap();
        assert_eq!(lines.len(), 2);
    }

    // --- golden: the durable wire format --------------------------------
    //
    // Every test above this line round-trips through *this* build, so all of
    // them still pass if a field is renamed, the checksum framing changes or
    // a `DomainEvent` variant's serde shape moves — encoder and decoder move
    // together and agree with each other. That is precisely the failure a
    // durable format cannot afford: `events.jsonl` is read back by future
    // builds, so the bytes are the contract, not the round-trip.
    //
    // The fixture is committed data. It pins the record envelope *and* the
    // `DomainEvent` payload shape, which no other golden covers — the public
    // goldens pin the `PublicEvent` projection, which is deliberately a
    // different schema with its own compatibility policy (ARCH §1).

    /// A log with a fixed session, fixed ids and fixed timestamps, so its
    /// encoding is byte-for-byte reproducible.
    fn golden_log() -> Vec<u8> {
        let header = Header::new(
            common::id(GOLDEN_SESSION),
            Timestamp::from_millis(1_787_206_085_352),
            "/ws".into(),
        );
        let mut log = encode_header(&header);
        for (n, event) in golden_events().iter().enumerate() {
            log.push(b'\n');
            log.extend(encode_record(seq_of(n as u64 + 1), event).unwrap());
        }
        log
    }

    const GOLDEN_SESSION: &str = "00000000-0000-0000-0000-000000000001";
    const GOLDEN_TURN: &str = "00000000-0000-0000-0000-000000000002";
    const GOLDEN_ITEM: &str = "00000000-0000-0000-0000-000000000003";
    const GOLDEN_EVENT_1: &str = "00000000-0000-0000-0000-00000000000a";
    const GOLDEN_EVENT_2: &str = "00000000-0000-0000-0000-00000000000b";
    const GOLDEN_EVENT_3: &str = "00000000-0000-0000-0000-00000000000c";
    const GOLDEN_EVENT_4: &str = "00000000-0000-0000-0000-00000000000d";
    const GOLDEN_EVENT_5: &str = "00000000-0000-0000-0000-00000000000e";
    const GOLDEN_GENERATION: &str = "00000000-0000-0000-0000-000000000010";
    const GOLDEN_ATTEMPT: &str = "00000000-0000-0000-0000-000000000011";

    /// An `AttemptRef` over fixed ids.
    ///
    /// Built through serde because there is deliberately no constructor that
    /// takes both ids — `AttemptRef::first` mints a fresh `AttemptId`, which
    /// a byte-for-byte golden cannot use. Decoding one here is also the
    /// durable shape being exercised in the direction that matters.
    fn golden_attempt() -> AttemptRef {
        serde_json::from_value(serde_json::json!({
            "generation": GOLDEN_GENERATION,
            "attempt": GOLDEN_ATTEMPT,
        }))
        .expect("an AttemptRef is two ids")
    }

    fn golden_events() -> Vec<DomainEvent> {
        let envelope = |id: &str, parent: Option<&str>| Envelope {
            seq: Seq::default(),
            id: common::id(id),
            turn: common::id(GOLDEN_TURN),
            item: Some(common::id(GOLDEN_ITEM)),
            parent: parent.map(common::id),
            ts: Timestamp::from_millis(1_787_206_085_400),
            producer: Producer::Main,
        };
        vec![
            DomainEvent {
                env: envelope(GOLDEN_EVENT_1, None),
                kind: EventKind::UserPrompt {
                    text: "list the files".to_string(),
                },
            },
            // Chosen deliberately: this is the money event. Its durable shape
            // carries every priced token category and an exact decimal amount
            // as a *string*, and a silent change to either would misprice
            // every session already on disk. Its reasoning split is unknown,
            // so this fixture also pins how *unknown* is spelled on disk:
            // absent, exactly as in a log written before the field existed.
            DomainEvent {
                env: envelope(GOLDEN_EVENT_2, Some(GOLDEN_EVENT_1)),
                kind: EventKind::UsageUpdated {
                    usage: Usage {
                        uncached_input_tokens: 120,
                        cache_read_tokens: 900,
                        cache_write_tokens: 60,
                        cache_write_extended_tokens: 0,
                        output_tokens: 45,
                        reasoning_tokens: None,
                    },
                    cost_delta: CostDelta::Known {
                        amount: Money::new(
                            "0.0027".parse::<Amount>().expect("a valid test amount"),
                            CurrencyCode::new("USD").expect("USD is a valid code"),
                        ),
                        attribution: CostAttribution::Table {
                            revision: PricingRevision("test-2026-08".to_string()),
                        },
                        fx: None,
                    },
                    // Deliberately absent, and the record above is committed
                    // without it. This is the backward-read evidence: a
                    // `UsageUpdated` written before attempts were attributed
                    // still decodes, which is the whole reason the field is
                    // optional rather than required
                    // (`docs/decisions/2026-08-21-attempt-accounting-ledger.md`).
                    attempt: None,
                },
            },
            // The attempt lifecycle, appended rather than woven in, so every
            // record above keeps the exact bytes an earlier build wrote.
            DomainEvent {
                env: envelope(GOLDEN_EVENT_3, Some(GOLDEN_EVENT_2)),
                kind: EventKind::GenerationStarted {
                    attempt: golden_attempt(),
                },
            },
            // The attributed twin of the record above: same money shape, now
            // carrying the attempt a correction can key to.
            DomainEvent {
                env: envelope(GOLDEN_EVENT_4, Some(GOLDEN_EVENT_3)),
                kind: EventKind::UsageUpdated {
                    usage: Usage {
                        uncached_input_tokens: 120,
                        cache_read_tokens: 900,
                        cache_write_tokens: 60,
                        cache_write_extended_tokens: 0,
                        output_tokens: 45,
                        reasoning_tokens: None,
                    },
                    cost_delta: CostDelta::Known {
                        amount: Money::new(
                            "0.0027".parse::<Amount>().expect("a valid test amount"),
                            CurrencyCode::new("USD").expect("USD is a valid code"),
                        ),
                        attribution: CostAttribution::Table {
                            revision: PricingRevision("test-2026-08".to_string()),
                        },
                        fx: None,
                    },
                    attempt: Some(golden_attempt()),
                },
            },
            DomainEvent {
                env: envelope(GOLDEN_EVENT_5, Some(GOLDEN_EVENT_4)),
                kind: EventKind::AttemptSettled {
                    attempt: golden_attempt(),
                    outcome: Outcome::new(
                        StopReason::Stop,
                        DeliveryCertainty::PossiblySent,
                        UsageReport::Reported(Usage {
                            uncached_input_tokens: 120,
                            cache_read_tokens: 900,
                            cache_write_tokens: 60,
                            cache_write_extended_tokens: 0,
                            output_tokens: 45,
                            reasoning_tokens: None,
                        }),
                    ),
                },
            },
        ]
    }

    #[test]
    fn codec_golden_log_encodes_to_the_committed_bytes() {
        let actual = String::from_utf8(golden_log()).expect("the codec emits UTF-8");
        assert_eq!(
            actual,
            include_str!("golden/store_log.jsonl").trim_end(),
            "the durable wire format changed. Every log already written to \
             disk is in the old shape, so this is a codec version bump and a \
             migration, not a fixture update."
        );
    }

    /// The committed log decodes under today's codec, with every checksum
    /// intact and every payload still parsing as a `DomainEvent`.
    ///
    /// This used to be called "written by a past build", and that name
    /// overclaimed: the fixture is regenerated whenever the durable format
    /// moves, so it has only ever proved that *this* build reads *this*
    /// fixture. `docs/decisions/2026-08-21-two-levels-of-ending.md` §3 makes
    /// the gap explicit rather than leaving it in a test name — v1 logs are
    /// not readable by a v2 build, deliberately, because none exist outside
    /// this repository. If that ever stops being true, this is the test that
    /// has to grow a real v1 fixture and a migration beside it.
    #[test]
    fn codec_golden_log_decodes_and_verifies_under_todays_codec() {
        let lines = decode_log(include_str!("golden/store_log.jsonl").as_bytes())
            .expect("the committed log decodes with checksums intact");

        let expected = golden_events();
        assert_eq!(lines.len(), expected.len() + 1, "header plus one per event");
        match &lines[0] {
            StoreLine::Header(h) => assert_eq!(h.v, CODEC_VERSION),
            other => panic!("first line is not a header: {other:?}"),
        }
        for (line, event) in lines[1..].iter().zip(&expected) {
            match line {
                // `decode_line` verifies the checksum before it yields a
                // `Record` at all, so reaching this arm is the integrity
                // assertion; deserialising the payload then proves the
                // committed bytes still parse as today's `DomainEvent`.
                StoreLine::Record(record) => {
                    let decoded: DomainEvent = serde_json::from_str(record.payload.get())
                        .expect("the committed payload still parses as a DomainEvent");
                    assert_eq!(&decoded, event);
                }
                other => panic!("expected a record, got {other:?}"),
            }
        }
    }

    /// `Usage` gained two fields in
    /// `docs/decisions/2026-08-21-a-vocabulary-settled-before-the-second-mapping.md`
    /// — one for cache writes at an extended retention, one for the share of
    /// the output the provider attributed to reasoning — and that ADR's
    /// argument for leaving `CODEC_VERSION` alone is that both decode when
    /// absent. This is the test that argument rests on, and it is written
    /// against the bytes the *previous* build committed rather than against
    /// anything this build can produce.
    ///
    /// The two fields decode to different things on purpose. Nothing ego has
    /// ever spoken to offers an extended cache retention, so a record without
    /// that field was written by a run that wrote nothing at one: **zero is
    /// that run's measurement.** A reasoning split is the opposite — three of
    /// the six wires publish one and this repository used to drop it — so an
    /// absent field means nobody said, and `None` is the only honest reading.
    /// A zero there would claim a turn did no reasoning on the strength of a
    /// field that did not exist when it ran.
    #[test]
    fn codec_a_usage_written_before_the_two_new_fields_decodes_without_inventing_them() {
        let old = r#"{"env":{"seq":0,"id":"00000000-0000-0000-0000-00000000000b","turn":"00000000-0000-0000-0000-000000000002","item":"00000000-0000-0000-0000-000000000003","parent":"00000000-0000-0000-0000-00000000000a","ts":1787206085400,"producer":"Main"},"kind":{"kind":"UsageUpdated","usage":{"uncached_input_tokens":120,"cache_read_tokens":900,"cache_write_tokens":60,"output_tokens":45},"cost_delta":{"cost":"known","amount":{"amount":"0.0027","currency":"USD"},"attribution":{"source":"table","revision":"test-2026-08"}}}}"#;

        let decoded: DomainEvent = serde_json::from_str(old)
            .expect("a usage payload without the new fields still decodes");
        let EventKind::UsageUpdated { usage, .. } = &decoded.kind else {
            panic!("the fixture is a UsageUpdated: {:?}", decoded.kind);
        };
        assert_eq!(usage.cache_write_extended_tokens, 0);
        assert_eq!(
            usage.reasoning_tokens, None,
            "an absent breakdown is unknown, and a zero would state a \
             measurement this record never carried"
        );

        // And it does not grow one on the way back out. Same rule as
        // `call_id` below: an unknown has one spelling on disk, and a log
        // round-tripped by this build stays byte-identical to the one the
        // older build wrote — except for the field whose zero is a real
        // measurement, which is why that one is asserted separately.
        let round_tripped = serde_json::to_string(&decoded).expect("a DomainEvent serialises");
        assert!(
            !round_tripped.contains("reasoning_tokens"),
            "an unknown reasoning split must not be written back at all: {round_tripped}"
        );
        assert!(
            round_tripped.contains(r#""cache_write_extended_tokens":0"#),
            "a measured zero is written: {round_tripped}"
        );

        // The converse, so both spellings are pinned and not just the one the
        // committed fixture happens to carry: a row that *did* publish a
        // breakdown reads back as the figure it published.
        let stated = old.replace(
            r#""output_tokens":45"#,
            r#""output_tokens":45,"reasoning_tokens":40"#,
        );
        let decoded: DomainEvent =
            serde_json::from_str(&stated).expect("a stated breakdown decodes");
        let EventKind::UsageUpdated { usage, .. } = &decoded.kind else {
            panic!("still a UsageUpdated: {:?}", decoded.kind);
        };
        assert_eq!(usage.reasoning_tokens, Some(40));
        assert_eq!(
            usage.total_tokens(),
            1125,
            "the breakdown is inside the output count, not beside it"
        );
    }

    /// An `Error` written before it could be classified decodes as
    /// `Unclassified`, which is what it proved: nobody read a provider code
    /// for it.
    ///
    /// This is the additivity proof for the field, and it is the reason the
    /// change carries no `CODEC_VERSION` bump — a new field that decodes when
    /// absent is additive by this module's own rule, and a required one would
    /// not be.
    #[test]
    fn codec_an_error_written_before_it_could_be_classified_decodes_as_unclassified() {
        let old = r#"{"env":{"seq":0,"id":"00000000-0000-0000-0000-00000000000b","turn":"00000000-0000-0000-0000-000000000002","item":null,"parent":null,"ts":1787206085400,"producer":"Main"},"kind":{"kind":"Error","message":"rate limited, retrying"}}"#;

        let decoded: DomainEvent =
            serde_json::from_str(old).expect("an error payload without the new field decodes");
        let EventKind::Error { message, kind } = &decoded.kind else {
            panic!("the fixture is an Error: {:?}", decoded.kind);
        };
        assert_eq!(message, "rate limited, retrying");
        assert_eq!(
            *kind,
            ErrorKind::Unclassified,
            "an absent classification must fail closed, not decode as retryable"
        );

        // The wire name is `error_kind`, because the enum is internally
        // tagged on `kind` and a field of that name would collide with the
        // discriminant. Pinned here so a later rename is caught as the codec
        // break it would be.
        let stated = old.replace(
            r#""message":"rate limited, retrying""#,
            r#""message":"rate limited, retrying","error_kind":"transient""#,
        );
        let decoded: DomainEvent =
            serde_json::from_str(&stated).expect("a stated classification decodes");
        let EventKind::Error { kind, .. } = &decoded.kind else {
            panic!("still an Error: {:?}", decoded.kind);
        };
        assert_eq!(*kind, ErrorKind::Transient);

        let round_tripped = serde_json::to_string(&decoded).expect("a DomainEvent serialises");
        assert!(
            round_tripped.contains(r#""error_kind":"transient""#),
            "the classification survives a round trip under its wire name: {round_tripped}"
        );
    }

    /// A reasoning state journalled as its own variant, round-tripped whole.
    ///
    /// The blob is opaque and the dialect beside it is what makes it
    /// replayable into the right model, so losing either half in the codec
    /// would leave a resumed session holding state it cannot use.
    #[test]
    fn codec_a_reasoning_state_round_trips_with_its_dialect() {
        let event = DomainEvent {
            env: golden_events()[0].env.clone(),
            kind: EventKind::ReasoningState {
                state: ReasoningState {
                    blob: "opaque-continuation-token".to_string(),
                    dialect: "anthropic-messages".to_string(),
                },
            },
        };

        let encoded = serde_json::to_string(&event).expect("a DomainEvent serialises");
        let decoded: DomainEvent = serde_json::from_str(&encoded).expect("and decodes");
        assert_eq!(decoded, event);
        assert_eq!(decoded.kind.name(), "reasoning_state");
    }

    /// `ToolCallStarted` gained `call_id` in
    /// `docs/decisions/2026-08-21-tool-use-history.md`, as an `Option` with a
    /// serde default rather than a required field plus a `CODEC_VERSION`
    /// bump. This is the claim that decision rests on: a payload written
    /// before the field existed still decodes, and decodes to the only honest
    /// value — not to a fabricated id, and not to an error.
    ///
    /// The other half of the same claim is that a `None` does not re-appear
    /// in the bytes, so a log round-tripped by this build is byte-identical
    /// to the one an older build wrote.
    #[test]
    fn codec_a_tool_call_written_before_call_id_existed_decodes_as_unknown() {
        let old = r#"{"env":{"seq":7,"id":"00000000-0000-0000-0000-00000000000a","turn":"00000000-0000-0000-0000-000000000002","item":"00000000-0000-0000-0000-000000000003","parent":null,"ts":1787206085400,"producer":"Main"},"kind":{"kind":"ToolCallStarted","name":"glob","args":"{\"pattern\":\"*\"}"}}"#;

        let decoded: DomainEvent =
            serde_json::from_str(old).expect("a payload without call_id still decodes");
        assert_eq!(
            decoded.kind,
            EventKind::ToolCallStarted {
                name: "glob".to_string(),
                args: "{\"pattern\":\"*\"}".to_string(),
                call_id: None,
            }
        );

        let round_tripped = serde_json::to_string(&decoded).expect("a DomainEvent serialises");
        assert_eq!(
            round_tripped, old,
            "an unknown call_id must not be written back as a null"
        );
    }
}
