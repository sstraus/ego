use super::Parser;
use crate::event::{Event, InternalEvent, KeyCode};

// Catches: coalesced cancellation Escape consumes the CPR's Escape and
// publishes its [row;columnR suffix as composer keys.
#[test]
fn escape_prefix_preserves_coalesced_cursor_reply() {
    let mut parser = Parser::default();
    parser.advance(b"\x1b\x1b[21;3R", false);
    assert_eq!(
        parser.collect::<Vec<_>>(),
        vec![
            InternalEvent::Event(Event::Key(KeyCode::Esc.into())),
            InternalEvent::CursorPosition(2, 20),
        ]
    );
}

// Catches: a tty read boundary changes ownership of the CPR prefix.
#[test]
fn escape_prefix_preserves_cursor_reply_at_every_read_boundary() {
    let bytes = b"\x1b\x1b[21;3R";
    for split in 1..bytes.len() {
        let mut parser = Parser::default();
        parser.advance(&bytes[..split], true);
        parser.advance(&bytes[split..], false);
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![
                InternalEvent::Event(Event::Key(KeyCode::Esc.into())),
                InternalEvent::CursorPosition(2, 20),
            ],
            "read boundary {split}",
        );
    }
}

// Catches: retaining an Escape for CPR corrupts adjacent non-CPR keys.
#[test]
fn escape_prefix_preserves_arrow_and_repeated_escape_keys() {
    let mut parser = Parser::default();
    parser.advance(b"\x1b\x1b[A\x1b\x1b", false);
    assert_eq!(
        parser.collect::<Vec<_>>(),
        vec![KeyCode::Esc, KeyCode::Up, KeyCode::Esc, KeyCode::Esc]
            .into_iter()
            .map(|code| InternalEvent::Event(Event::Key(code.into())))
            .collect::<Vec<_>>()
    );
}
