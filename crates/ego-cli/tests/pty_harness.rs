//! Terminal bytes recorded during an early Ctrl-D timeout.

#![cfg(not(windows))]

mod support;

#[test]
fn typed_line_repaint_does_not_signal_an_empty_prompt_for_eof() {
    // Catches: counting the echoed /fork prompt as the idle editor after
    // submission, sending Ctrl-D while Reedline changes its input mode.
    let recorded = include_bytes!("fixtures/pty/typed-fork-before-eof.ansi");
    let typed = recorded
        .windows(b"/fork".len())
        .position(|window| window == b"/fork")
        .expect("the recorded child echoed the command");
    let before = support::ready_prompt_count(&recorded[..typed]);
    assert_eq!(before, 2);
    for end in typed..=recorded.len() {
        assert_eq!(support::ready_prompt_count(&recorded[..end]), before);
    }
}

#[test]
fn incomplete_empty_prompt_frame_does_not_signal_readiness() {
    // Catches: a PTY chunk ending inside a redraw is mistaken for an editor
    // that has completed its cursor handshake and can consume input.
    let recorded = include_bytes!("fixtures/pty/typed-fork-before-eof.ansi");
    let first_end = recorded
        .windows(b"\x1b[?25h".len())
        .position(|window| window == b"\x1b[?25h")
        .unwrap()
        + b"\x1b[?25h".len();
    let first = &recorded[..first_end];
    assert_eq!(support::ready_prompt_count(first), 1);
    let mut frames = first.to_vec();
    frames.extend_from_slice(first);
    assert_eq!(support::ready_prompt_count(&frames), 2);
    frames.extend_from_slice(&first[..first.len() - 1]);
    assert_eq!(support::ready_prompt_count(&frames), 2);
    frames.push(first[first.len() - 1]);
    assert_eq!(support::ready_prompt_count(&frames), 3);
}
