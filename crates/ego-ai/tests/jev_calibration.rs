//! Calibration protects real dataset boundaries and the wake safety fallback.
use ego_ai::jev::calibration::{self, Split};

// Catches: a held-out ACT at the inclusive low boundary silently leaving wake on.
#[test]
fn held_out_act_suppression_disables_wake_after_retuning() {
    let (_, _, trained) = calibration::tune(&[(true, 0.8), (false, 0.1)]).unwrap();
    assert!(trained.passed());
    let regression = calibration::score(&[(true, 0.1), (false, 0.05)], 0.1, 0.8).unwrap();
    assert_eq!(regression.false_negatives, 1);
    assert!(!regression.passed());
    let grey = calibration::score(&[(true, 0.2), (false, 0.05)], 0.1, 0.8).unwrap();
    assert_eq!(grey.grey, 1);
    assert_eq!(grey.false_negatives, 0);
    assert!(grey.passed());
}
// Catches: leakage or edited content invalidating supposedly frozen held-out evidence.
#[test]
fn labelled_sets_reject_content_tampering_and_split_leakage() {
    for data in [
        include_str!("../../../data/jev/pump.jsonl"),
        include_str!("../../../data/jev/commands.jsonl"),
        include_str!("../../../data/jev/turns.jsonl"),
    ] {
        let mut rows = calibration::load(data).unwrap();
        assert!(rows.iter().any(|r| r.split == Split::Calibration));
        assert!(rows.iter().any(|r| r.split == Split::HeldOut));
        let frozen = calibration::fingerprint(&rows, Split::HeldOut).unwrap();
        let mut edited = rows.clone();
        let row = edited
            .iter_mut()
            .find(|r| r.split == Split::HeldOut)
            .unwrap();
        row.text.push_str(" edited");
        row.text_sha256 = calibration::hash(row.text.as_bytes());
        let edited = edited
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(calibration::load(&edited).is_ok());
        assert!(calibration::load_frozen(&edited, &frozen).is_err());
        let mut duplicate = rows[0].clone();
        duplicate.id = "leaked-row".into();
        duplicate.split = match duplicate.split {
            Split::Calibration => Split::HeldOut,
            Split::HeldOut => Split::Calibration,
        };
        rows.push(duplicate);
        let text = rows
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(calibration::load(&text).is_err());
        let tampered = data.replacen("\"text\": \"", "\"text\": \"changed ", 1);
        assert!(calibration::load(&tampered).is_err());
    }
}
// Catches: absent negative examples, unsafe high-band positives, or NaN counted as a pass.
#[test]
fn uninformative_or_invalid_scores_never_enable_a_use() {
    assert!(
        !calibration::score(&[(true, 0.9)], 0.1, 0.8)
            .unwrap()
            .passed()
    );
    assert!(
        !calibration::score(&[(true, 0.9), (false, 0.9)], 0.1, 0.8)
            .unwrap()
            .passed()
    );
    assert!(calibration::score(&[(true, f64::NAN)], 0.1, 0.8).is_err());
    assert!(calibration::score(&[(true, 0.9)], 0.8, 0.1).is_err());
    assert!(calibration::score(&[(true, 1.1)], 0.1, 0.8).is_err());
}
