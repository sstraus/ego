//! Display-only replay from the existing durable session, through live renderers.
use std::collections::{HashMap, HashSet};

use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::id::{ItemId, SessionId};
use ego_core::session::ReplaySource;
use ego_core::store::session_store::SessionEntry;

use super::{Answer, Capabilities, tool_row};
use crate::output::TextOutput;

/// Ten recent user turns keep startup output bounded without a new setting.
const RECENT_TURNS: usize = 10;
const UNAVAILABLE: &str = "[transcript item unavailable]";

pub(crate) fn replay(
    source: &dyn ReplaySource,
    session: SessionId,
    output: &dyn TextOutput,
    capabilities: Capabilities,
    workspace: Option<&std::path::Path>,
) -> Result<(), String> {
    let entries = source
        .entries(&session)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("session {session} is missing"))?;
    let events: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            SessionEntry::RunTransaction(_)
            | SessionEntry::Evidence(_)
            | SessionEntry::NoticeDelivered(_)
            | SessionEntry::ModelSelected(_) => None,
        })
        .collect();
    render(&events, output, capabilities, workspace);
    output.finish();
    Ok(())
}

fn message(
    output: &dyn TextOutput,
    text: &str,
    capabilities: Capabilities,
    workspace: Option<&std::path::Path>,
) {
    let mut answer = Answer::new(capabilities.with_current_width(), workspace);
    let mut rendered = answer.push(text);
    rendered.push_str(&answer.finish());
    output.begin_block();
    output.answer(&rendered);
    if !rendered.ends_with('\n') {
        output.line("");
    }
}

fn render(
    events: &[DomainEvent],
    output: &dyn TextOutput,
    capabilities: Capabilities,
    workspace: Option<&std::path::Path>,
) {
    let mut seen = HashSet::new();
    let turns: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            (matches!(event.kind, EventKind::UserPrompt { .. }) && seen.insert(event.env.turn))
                .then_some(index)
        })
        .collect();
    let omitted = turns.len().saturating_sub(RECENT_TURNS);
    if omitted > 0 {
        output.line(&format!("{omitted} earlier turns omitted"));
    }
    let events = &events[turns.get(omitted).copied().unwrap_or(0)..];
    let finals: HashMap<_, _> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event.kind {
            EventKind::AssistantMessage { .. } => event.env.item.map(|item| (item, index)),
            _ => None,
        })
        .collect();
    // TODO(2027-03-01): remove legacy AssistantDelta replay support.
    // Only logs written by earlier versions carry chunks; a partial without a
    // final message is printed from them.
    let mut last_delta = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        if matches!(event.kind, EventKind::AssistantDelta { .. })
            && let Some(item) = event.env.item
        {
            last_delta.insert(item, index);
        }
    }
    let mut partials: HashMap<ItemId, String> = HashMap::new();
    let mut calls: HashMap<ItemId, (&str, &str)> = HashMap::new();
    let mut timings = HashMap::new();
    let mut tools = tool_row::ToolRows::default();
    for (index, event) in events.iter().enumerate() {
        let transcript = matches!(
            event.kind,
            EventKind::UserPrompt { .. }
                | EventKind::AssistantMessage { .. }
                | EventKind::AssistantDelta { .. }
                | EventKind::ToolCallStarted { .. }
                | EventKind::ToolCallCompleted { .. }
        );
        if transcript && event.env.item.is_none() {
            tools.separate();
            output.line(UNAVAILABLE);
            continue;
        }
        match &event.kind {
            EventKind::UserPrompt { text, images, .. } => {
                tools.separate();
                output.line(&format!("> {text}"));
                if let Some(label) = super::attachments::label(images.len()) {
                    output.line(&label);
                }
            }
            EventKind::AssistantMessage { text } => {
                if event.env.item.and_then(|item| finals.get(&item)) == Some(&index) {
                    tools.separate();
                    message(output, text, capabilities, workspace);
                }
            }
            EventKind::CompactionSeeded { checkpoint, .. } => {
                tools.separate();
                message(output, checkpoint, capabilities, workspace)
            }
            // TODO(2027-03-01): remove legacy AssistantDelta replay support.
            EventKind::AssistantDelta { text } => {
                let item = event.env.item.expect("transcript item checked above");
                if !finals.contains_key(&item) {
                    let partial = partials.entry(item).or_default();
                    partial.push_str(text);
                    if last_delta.get(&item) == Some(&index) {
                        tools.separate();
                        message(output, partial, capabilities, workspace);
                    }
                }
            }
            EventKind::ToolCallStarted { name, args, .. } => {
                calls.insert(
                    event.env.item.expect("transcript item checked above"),
                    (name, args),
                );
            }
            EventKind::ToolCallMeasured { timing } => {
                if let Some(item) = event.env.item {
                    timings.insert(item, *timing);
                }
            }
            EventKind::ToolCallCompleted { ok, result } => {
                let item = event.env.item.expect("transcript item checked above");
                if let Some((name, args)) = calls.remove(&item) {
                    if name == ego_core::question::ASK_QUESTION_TOOL {
                        continue;
                    }
                    let text = result.model_text();
                    if matches!(name.rsplit(" › ").next(), Some("write" | "edit")) {
                        let call = tool_row::Finished {
                            name,
                            args,
                            ok: *ok,
                            result: &text,
                            result_blocks: Some(&result.content),
                            timing: timings.remove(&item),
                            before_image: None,
                        };
                        for record in tools.record_writes(
                            &call,
                            result.file_writes(),
                            capabilities.with_current_width(),
                        ) {
                            output.tool_row(&record.rows, record.detail);
                        }
                        continue;
                    }
                    let record = tools.record(
                        &tool_row::Finished {
                            name,
                            args,
                            ok: *ok,
                            result: &text,
                            result_blocks: Some(&result.content),
                            timing: timings.remove(&item),
                            before_image: result.before_image(),
                        },
                        capabilities.with_current_width(),
                    );
                    output.tool_row(&record.rows, record.detail);
                } else {
                    tools.separate();
                    output.line(UNAVAILABLE);
                }
            }
            _ => {}
        }
    }
    for _ in calls {
        output.line(UNAVAILABLE);
    }
}

#[cfg(test)]
mod tests {
    mod links;
    mod measured;
    use super::*;
    use crate::output::Diagnostics;
    use ego_core::event::domain::Envelope;
    use ego_core::event::id::{EventId, Producer, Seq, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_core::store::session_store::SessionStore;
    use ego_tools::store::{JsonlStore, OsIo};
    use std::sync::Mutex;

    // Catches: wall-clock replay fabricating execution duration including approval.
    #[test]
    fn replay_without_measurement_does_not_report_approval_as_execution() {
        let item = ItemId::new();
        let turn = TurnId::new();
        let event = |seq, ts, kind| DomainEvent {
            env: Envelope {
                seq: serde_json::from_value(serde_json::json!(seq)).unwrap(),
                id: EventId::new(),
                turn,
                item: Some(item),
                parent: None,
                ts: Timestamp::from_millis(ts),
                producer: Producer::Main,
            },
            kind,
        };
        let events = [
            event(
                1,
                100,
                EventKind::ToolCallStarted {
                    name: "exec".into(),
                    args: r#"{"command":"pwd"}"#.into(),
                    call_id: None,
                },
            ),
            event(
                2,
                16_000,
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ego_core::permit::ToolResult::builtin("/ws"),
                },
            ),
        ];
        let output = Recorded::default();
        render(&events, &output, Capabilities::plain(), None);
        let text = output.0.lock().unwrap();
        assert!(!text.contains("duration unavailable"), "{text}");
        assert!(
            !text.contains("15.9s"),
            "approval pause is not execution: {text}"
        );
    }

    // Catches: replay merges persisted marker lines or hard-wraps their tails.
    #[test]
    fn rendered_host_markers_survive_replay() {
        let text = format!(
            "TUICommander v1.8.0 is connected.\nintent: {} (Replay check)\nsuggest: [ A | B | C ]",
            "long replay marker ".repeat(30)
        );
        for width in [80, 120, 200] {
            let output = Recorded::default();
            let mut seq = Seq::ZERO;
            let events = [DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: Some(ItemId::new()),
                    parent: None,
                    ts: Timestamp::from_millis(1),
                    producer: Producer::Main,
                },
                kind: EventKind::AssistantMessage { text: text.clone() },
            }];
            render(
                &events,
                &output,
                Capabilities {
                    width,
                    depth: super::super::Depth::TrueColor,
                    osc: super::super::OscProfile::Tuic,
                    ..Capabilities::plain()
                },
                None,
            );
            assert_eq!(*output.0.lock().unwrap(), format!("{text}\n"));
        }
    }

    // Catches: live and resumed semantic blocks touch instead of sharing one blank row.
    #[cfg(unix)]
    #[test]
    fn rendered_turn_block_boundaries() {
        use crate::output::InteractiveOutput;
        use crate::repl::SurfaceOutput;
        use ego_core::turn::CancelToken;
        use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
        use std::io::Read;
        fn capture(text: &str, width: usize) -> String {
            let pair = NativePtySystem::default()
                .openpty(PtySize {
                    rows: 40,
                    cols: width as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", "printf '%s' \"$1\"", "rhythm-capture", text]);
            let mut child = pair.slave.spawn_command(command).unwrap();
            drop(pair.slave);
            let mut bytes = Vec::new();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut buffer = [0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(error) => panic!("PTY read: {error}"),
                }
            }
            assert!(child.wait().unwrap().success());
            let mut terminal = vt100::Parser::new(40, width as u16, 0);
            terminal.process(&bytes);
            terminal.screen().contents()
        }
        for width in [80, 120, 200] {
            let (sender, receiver) = crossbeam::channel::unbounded();
            let output = InteractiveOutput::new(SurfaceOutput::new(sender, CancelToken::new()));
            output.line("> first");
            output.answer("Prose\n");
            output.tool_row("✓ read first\n  tail one\n  tail two", String::new());
            output.tool_row("✗ read second", String::new());
            output.answer("Done\n");
            output.line("> second");
            output.answer("• item one\n• item two\n\n  code one\n  code two\n");
            output.finish();
            let live = receiver.try_iter().collect::<String>();
            let expected = "> first\n\nProse\n\n✓ read first\n  tail one\n  tail two\n\n✗ read second\n\nDone\n\n> second\n\n• item one\n• item two\n\n  code one\n  code two";
            let grid = capture(&live, width);
            assert_eq!(grid, expected, "width={width}");
            println!("rhythm live width={width} grid={grid:?}");

            let (sender, receiver) = crossbeam::channel::unbounded();
            let output = InteractiveOutput::new(SurfaceOutput::new(sender, CancelToken::new()));
            let mut seq = Seq::ZERO;
            let turn = TurnId::new();
            let tool_item = ItemId::new();
            let events = [
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "first".into(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                EventKind::AssistantMessage {
                    text: "Prose".into(),
                },
                EventKind::ToolCallStarted {
                    name: "read".into(),
                    args: r#"{"files":[{"path":"first"}]}"#.into(),
                    call_id: None,
                },
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ego_core::permit::ToolResult::builtin("tail"),
                },
                EventKind::AssistantMessage {
                    text: "Done".into(),
                },
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "second".into(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                EventKind::AssistantMessage {
                    text: "Final".into(),
                },
            ]
            .into_iter()
            .map(|kind| DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn,
                    item: Some(
                        if matches!(
                            kind,
                            EventKind::ToolCallStarted { .. } | EventKind::ToolCallCompleted { .. }
                        ) {
                            tool_item
                        } else {
                            ItemId::new()
                        },
                    ),
                    parent: None,
                    ts: Timestamp::from_millis(1),
                    producer: Producer::Main,
                },
                kind,
            })
            .collect::<Vec<_>>();
            render(
                &events,
                &output,
                Capabilities {
                    width,
                    osc: super::super::OscProfile::Standard,
                    ..Capabilities::plain()
                },
                None,
            );
            output.finish();
            let replayed = receiver.try_iter().collect::<String>();
            let grid = capture(&replayed, width);
            assert_eq!(
                grid,
                "> first\n\nProse\n\n✓ read first · 1 line\n  … 1 more line · Option+O or /expand\n\nDone\n\n> second\n\nFinal"
            );
            println!("rhythm replay width={width} grid={grid:?}");

            for (prose, expected) in [(true, "\nOnly prose"), (false, "\n✓ read only\n  tail")] {
                let (sender, receiver) = crossbeam::channel::unbounded();
                let output = InteractiveOutput::new(SurfaceOutput::new(sender, CancelToken::new()));
                if prose {
                    for delta in ["Only ", "pro", "se", "\n"] {
                        output.answer(delta);
                    }
                } else {
                    output.tool_row("✓ read only\n  tail", String::new());
                }
                output.finish();
                let bytes = receiver.try_iter().collect::<String>();
                assert_eq!(capture(&bytes, width), expected);
            }
        }
    }

    #[derive(Default)]
    struct Recorded(Mutex<String>);
    impl Diagnostics for Recorded {
        fn note(&self, text: &str) {
            self.0.lock().unwrap().push_str(text);
        }
    }
    impl TextOutput for Recorded {
        fn write(&self, text: &str) {
            self.0.lock().unwrap().push_str(text);
        }
        fn line(&self, text: &str) {
            self.write(&format!("{text}\n"));
        }
    }

    // Catches: displaying a durable transcript rewrites the log or adds usage.
    #[test]
    fn replay_preserves_durable_bytes_and_usage_without_a_generation() {
        let home = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(home.path()).unwrap();
        let store = JsonlStore::new(home.path().join("sessions"), root.clone(), Box::new(OsIo));
        let id = SessionId::new();
        store
            .publish_empty_session(id, &root, Timestamp::from_millis(1))
            .unwrap();
        let mut seq = Seq::ZERO;
        store
            .append(
                &id,
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: Some(ItemId::new()),
                        parent: None,
                        ts: Timestamp::from_millis(2),
                        producer: Producer::Main,
                    },
                    kind: EventKind::AssistantMessage {
                        text: "DISPLAY-ONLY-KOO".to_owned(),
                    },
                },
            )
            .unwrap();
        let log = home
            .path()
            .join("sessions")
            .join(id.to_string())
            .join("events.jsonl");
        let before = std::fs::read(&log).unwrap();
        let entries = source_entries(&store, id);
        let state = ego_core::state::SessionState::replay(entries.clone());
        let output = Recorded::default();
        replay(&store, id, &output, Capabilities::plain(), Some(&root)).unwrap();
        assert_eq!(*output.0.lock().unwrap(), "DISPLAY-ONLY-KOO\n");
        assert_eq!(std::fs::read(log).unwrap(), before);
        assert_eq!(
            ego_core::state::SessionState::replay(source_entries(&store, id)),
            state
        );
    }

    fn source_entries(store: &JsonlStore, id: SessionId) -> Vec<DomainEvent> {
        store
            .entries(&id)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Event(event) => Some(event),
                _ => None,
            })
            .collect()
    }
}
