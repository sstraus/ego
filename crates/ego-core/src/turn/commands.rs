//! Command preprocessing through the ordinary tool preparation and guarded effects.

use super::*;
use crate::command::{MAX_PROMPT_BYTES, MAX_SHELL_BYTES, Part};
use crate::permit::{GuardedIo, Op, StreamProvenance, ToolOutputEncoding, ToolOutputProvenance};

impl<'session: 'turn, 'turn> TurnLoop<'session, 'turn> {
    pub(crate) async fn expand_command(
        &mut self,
        template: &str,
        arguments: &str,
        turn: TurnId,
    ) -> Result<String, String> {
        let parts = crate::command::prepare(template, arguments).map_err(|e| e.to_string())?;
        let mut imports = 0;
        let mut import_bytes = 0;
        let mut prepared = Vec::new();
        // Resolve every import before any command starts. Imported bytes never
        // pass through the template parser, even if they contain shell markers.
        for part in parts {
            match part {
                Part::File(path) => {
                    imports += 1;
                    if imports > 8
                        || std::path::Path::new(&path).is_absolute()
                        || std::path::Path::new(&path)
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                    {
                        return Err(
                            "command file reference leaves its scope or exceeds eight files".into(),
                        );
                    }
                    let (_, call) = self
                        .tools
                        .prepare("read", &serde_json::json!({"path":path}).to_string())?;
                    let file = call
                        .ops()
                        .iter()
                        .find_map(|op| match op {
                            Op::Read { path } => Some(path),
                            _ => None,
                        })
                        .ok_or("command import did not prepare a read")?;
                    if !file
                        .as_path()
                        .starts_with(self.tools.command_cwd().as_path())
                        || file.as_path() != self.tools.command_cwd().as_path().join(&path)
                    {
                        return Err("command file reference resolves outside the workspace".into());
                    }
                    let policy = self.policy_source.current()?;
                    let authorization = match self.tools.authorize(&policy, call) {
                        Classification::Settled(Ok(authorized)) => authorized,
                        Classification::Settled(Err(error)) => return Err(error.to_string()),
                        Classification::Ask(pending) => {
                            match self.tools.settle(&policy, pending).await {
                                Resolution::Settled(Ok(authorized)) => authorized,
                                Resolution::Settled(Err(error)) => return Err(error.to_string()),
                                _ => {
                                    return Err(
                                        "command import requires an explicit read grant".into()
                                    );
                                }
                            }
                        }
                    };
                    let (call, effects) = self.tools.effect_ports().bind(authorization);
                    let Some(Op::Read { path }) = call.ops().first() else {
                        return Err("command import lost its read operation".into());
                    };
                    let bytes = effects
                        .read_bounded(path, 64 * 1024)
                        .map_err(|e| e.to_string())?;
                    import_bytes += bytes.len();
                    if bytes.len() > 64 * 1024 || import_bytes > MAX_PROMPT_BYTES {
                        return Err("command import exceeds instruction byte limits".into());
                    }
                    prepared.push(Part::Text(
                        String::from_utf8(bytes).map_err(|_| "command import is not UTF-8")?,
                    ));
                }
                part => prepared.push(part),
            }
        }
        let values = shell_words::split(arguments).map_err(|e| e.to_string())?;
        let mut output = String::new();
        for part in prepared {
            let text = match part {
                Part::Text(text) => text,
                Part::File(_) => unreachable!("imports resolved before shell execution"),
                Part::Shell(command) => {
                    if self.cancel.is_cancelled() {
                        return Err("command preprocessing cancelled".into());
                    }
                    let mut argv = vec![
                        "/bin/sh".to_owned(),
                        "-c".into(),
                        command,
                        "ego-command".into(),
                    ];
                    argv.extend(values.iter().cloned());
                    let args =
                        serde_json::json!({"argv":argv,"timeout_ms":30000,"background":false})
                            .to_string();
                    let catalogue = self.tools.capture().map_err(|e| e.to_string())?;
                    let selected = catalogue.select("exec");
                    let item = ItemId::new();
                    let started = self
                        .journal
                        .append(
                            turn,
                            Some(item),
                            None,
                            EventKind::ToolCallStarted {
                                name: "exec".into(),
                                args: args.clone(),
                                call_id: None,
                            },
                        )
                        .map_err(|e| e.to_string())?;
                    let mut call = PendingCall {
                        timing: Default::default(),
                        user_effect: None,
                        item,
                        started,
                        selected: Some(selected),
                        args,
                        _catalogue_generation: catalogue.generation(),
                        capture_authority: catalogue.authority(),
                    };
                    let capture = self.policy_source.capture()?;
                    let outcome = self
                        .run_tool(&capture.policy(), &capture.snapshot(), turn, &mut call)
                        .await
                        .map_err(|e| e.to_string())?;
                    let (ok, mut result) = match outcome {
                        ToolOutcome::Answered { ok, result }
                        | ToolOutcome::Executed { ok, result, .. } => (ok, result),
                        ToolOutcome::Refused { denial } => return Err(denial.to_string()),
                        ToolOutcome::Unanswerable { .. } => {
                            return Err("command shell preprocessing requires permission".into());
                        }
                        _ => return Err("command shell preprocessing did not complete".into()),
                    };
                    self.tools.finish_result(&mut result, self.invocation, item);
                    self.journal
                        .append(
                            turn,
                            Some(item),
                            Some(started),
                            EventKind::ToolCallCompleted {
                                ok,
                                result: result.clone(),
                            },
                        )
                        .map_err(|e| e.to_string())?;
                    if !ok {
                        return Err(result
                            .content
                            .iter()
                            .map(|block| block.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"));
                    }
                    let mut stdout = String::new();
                    for block in result.content {
                        if let ToolOutputProvenance::Process {
                            stream: StreamProvenance::Stdout,
                            encoding,
                            truncated,
                            ..
                        } = block.provenance
                        {
                            if truncated || encoding != ToolOutputEncoding::Utf8 {
                                return Err(
                                    "command shell stdout is truncated or is not UTF-8".into()
                                );
                            }
                            stdout.push_str(&block.text);
                            if stdout.len() > MAX_SHELL_BYTES {
                                return Err("command shell stdout exceeds 64 KiB".into());
                            }
                        }
                    }
                    stdout
                }
            };
            if output.len().saturating_add(text.len()) > MAX_PROMPT_BYTES {
                return Err("command expanded prompt exceeds 256 KiB".into());
            }
            output.push_str(&text);
        }
        Ok(output)
    }
}
