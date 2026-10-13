use super::*;
use crate::generator::port::AiEvent;
use crate::generator::types::StopReason;

const CANDIDATE: &str = "/Users/u/Exports/bench-2026/report.json";

fn finished(text: &str) -> JudgeDecoder {
    let mut decoder = JudgeDecoder::new();
    decoder.observe(&AiEvent::TextDelta { text: text.into() });
    decoder.observe(&AiEvent::Stop(StopReason::Stop));
    decoder
}

fn decode(text: &str) -> Result<ResidualVerdict, &'static str> {
    decode_residual(&finished(text), CANDIDATE)
}

// Catches: a verdict that names a parent or another path is applied instead of
// the exact candidate, widening the grant beyond the refused path.
#[test]
fn a_write_verdict_for_any_path_but_the_presented_candidate_is_invalid() {
    for path in [
        "/Users/u/Exports",
        "/Users/u/.ssh",
        "/",
        "/Users/u/Exports/bench-2026/report.json/",
    ] {
        let text =
            format!(r#"{{"grant":"write","path":"{path}","scope":"session","reason":"needed"}}"#);
        assert!(decode(&text).is_err(), "accepted {path}");
    }
    assert_eq!(
        decode(&format!(
            r#"{{"grant":"write","path":"{CANDIDATE}","scope":"session","reason":"needed"}}"#
        )),
        Ok(ResidualVerdict::Write {
            scope: ResidualScope::Session,
            reason: "needed".into()
        })
    );
}

// Catches: a lenient decoder takes the last of duplicate keys, a read/exec
// verdict, an extra key or a second object as a write grant.
#[test]
fn only_one_closed_self_consistent_object_is_a_verdict() {
    for text in [
        format!(
            r#"{{"grant":"none","grant":"write","path":"{CANDIDATE}","scope":"session","reason":"r"}}"#
        ),
        format!(r#"{{"grant":"read","path":"{CANDIDATE}","scope":"session","reason":"r"}}"#),
        format!(r#"{{"grant":"exec","path":"{CANDIDATE}","scope":"session","reason":"r"}}"#),
        format!(
            r#"{{"grant":"write","path":"{CANDIDATE}","scope":"session","reason":"r","approved":true}}"#
        ),
        format!(r#"{{"grant":"write","path":"{CANDIDATE}","scope":"session","reason":"r"}} {{}}"#),
        format!(r#"{{"grant":"write","path":"{CANDIDATE}","scope":"none","reason":"r"}}"#),
        format!(r#"{{"grant":"write","path":"{CANDIDATE}","scope":"session","reason":""}}"#),
        r#"{"grant":"none","path":"/x","scope":"none","reason":"r"}"#.to_owned(),
        r#"{"grant":"none","scope":"none","reason":"r"}"#.to_owned(),
        r#"{"grant":"none","path":null,"scope":"session","reason":"r"}"#.to_owned(),
        format!(
            r#"{{"grant":"write","path":"{CANDIDATE}","scope":"session","reason":"{}"}}"#,
            "x".repeat(1025)
        ),
    ] {
        assert!(decode(&text).is_err(), "accepted {text}");
    }
    assert_eq!(
        decode(r#"{"grant":"none","path":null,"scope":"none","reason":"unrelated"}"#),
        Ok(ResidualVerdict::None {
            reason: "unrelated".into()
        })
    );
}

// Catches: a generation cut off before its terminal event, or one that tried
// a tool, still yields a grant from its partial text.
#[test]
fn an_incomplete_or_tool_using_generation_is_no_verdict() {
    let text = format!(r#"{{"grant":"write","path":"{CANDIDATE}","scope":"always","reason":"r"}}"#);
    let mut open = JudgeDecoder::new();
    open.observe(&AiEvent::TextDelta { text: text.clone() });
    assert!(decode_residual(&open, CANDIDATE).is_err());
    let mut tool = finished(&text);
    tool.observe(&AiEvent::ToolCall {
        call_id: "call".into(),
        name: "exec".into(),
        args: "{}".into(),
    });
    assert!(decode_residual(&tool, CANDIDATE).is_err());
    assert_eq!(
        decode(&text),
        Ok(ResidualVerdict::Write {
            scope: ResidualScope::Always,
            reason: "r".into()
        })
    );
}
