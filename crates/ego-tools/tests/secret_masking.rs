//! Catches disclosure of exact known values, including split and overlapping matches.
use ego_core::secrets::SecretValues;

#[test]
fn split_secret_and_longer_overlap_cannot_leak_into_persisted_chunks() {
    let values = SecretValues::new([b"token".to_vec(), b"token-long".to_vec(), vec![]]);
    for split in 0..=21 {
        let input = b"prefix token-long end";
        let split = split.min(input.len());
        let mut stream = values.stream();
        let mut out = stream.push(&input[..split], false);
        out.extend(stream.push(&input[split..], true));
        assert_eq!(out, b"prefix [REDACTED] end", "split {split}");
    }
    let mut stream = values.stream();
    assert_eq!(stream.push(b"\xffnot-a-tok", false), b"\xffnot-a-");
    assert_eq!(stream.push(b"", true), b"tok");
}

#[test]
fn export_masks_decoded_values_without_matching_regex_like_text() {
    let values = SecretValues::new([b"a.*\n\"token".to_vec()]);
    let mut json = serde_json::json!({"text": "a.*\n\"token", "public": "abcdef token"});
    values.mask_json(&mut json);
    assert_eq!(json["text"], "[REDACTED]");
    assert_eq!(json["public"], "abcdef token");
}
