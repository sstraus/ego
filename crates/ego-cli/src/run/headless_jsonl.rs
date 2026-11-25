//! Line input for a retained headless session, independent of runtime shutdown.

use std::io::BufRead;

use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptRecord {
    prompt: String,
}

pub(super) struct Input {
    receiver: tokio::sync::mpsc::Receiver<std::io::Result<String>>,
    line: usize,
}

impl Input {
    pub(super) fn start() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        // Tokio stdin uses the blocking pool: a pending read would make runtime
        // shutdown wait for EOF after SIGTERM. This detached OS reader has no
        // session/provider authority and cannot keep process shutdown waiting.
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let failed = line.is_err();
                if sender.blocking_send(line).is_err() || failed {
                    break;
                }
            }
        });
        Self { receiver, line: 0 }
    }

    pub(super) async fn next(
        &mut self,
        cancel: &CancelToken,
        deadline: Option<Instant>,
    ) -> Result<Option<RunPrompt>, RunTermination> {
        let record = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(cancelled_termination(cancel)),
            cause = stop_signal(deadline) => {
                cancel.cancel(cause);
                return Err(cancelled_termination(cancel));
            }
            record = self.receiver.recv() => record,
        };
        let Some(record) = record else {
            return Ok(None);
        };
        self.line += 1;
        let invalid = |reason| RunTermination::UsageError {
            message: format!(
                "stdin JSONL line {}: {reason}; expected {{\"prompt\":\"non-empty text\"}}. Correct the record before resuming",
                self.line
            ),
        };
        let line =
            record.map_err(|error| invalid(format!("could not read UTF-8 input: {error}")))?;
        let record: PromptRecord = serde_json::from_str(&line)
            .map_err(|error| invalid(format!("invalid prompt record: {error}")))?;
        if record.prompt.trim().is_empty() {
            return Err(invalid("prompt is empty".to_owned()));
        }
        Ok(Some(RunPrompt::from(record.prompt)))
    }
}

#[derive(serde::Serialize)]
struct ResultRecord {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    report: Report,
}

pub(super) fn publish(ended: &Ended, output: &dyn TextOutput) -> Result<(), RunTermination> {
    let record = ResultRecord {
        kind: "result",
        report: report(ended),
    };
    let line = serde_json::to_string(&record).map_err(|error| RunTermination::Failed {
        message: format!("could not encode JSONL turn result: {error}"),
    })?;
    output.line(&line);
    Ok(())
}
