//! Epoch-owned, policy-guarded status producers. Rendering never executes code.
use std::collections::BTreeMap;
use std::path::Path;

use ego_core::config::StatusProvider;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, PolicyEngine, PrepareCtx, ProcessExecutor, Resolution,
    StreamProvenance, ToolOutputEncoding, ToolOutputProvenance, ToolResult, TypedTool,
};
use ego_core::turn::ToolEffectPorts;
use ego_tools::fs::OsFs;
use ego_tools::tools::{ExecParams, ExecTool};

/// One bounded refresh per interactive session epoch, using the run's actual effects and policy.
/// Requests requiring more authority are omitted; status collection never asks a model or human.
pub async fn collect(
    providers: &BTreeMap<String, StatusProvider>,
    cwd: &Path,
    environment: EnvPolicy,
    policy: &PolicyEngine,
    effects: ToolEffectPorts<'_>,
    process: &dyn ProcessExecutor,
) -> (Vec<(String, String)>, Vec<String>) {
    let tool = ExecTool::with_environment(environment);
    let context = PrepareCtx {
        cwd: CanonPath::new(cwd),
        resolver: &OsFs,
    };
    let mut values = Vec::new();
    let mut diagnostics = Vec::new();
    for (name, provider) in providers {
        let collected = async {
            let call = tool
                .prepare_typed(
                    ExecParams {
                        argv: provider.argv.clone(),
                        cwd: None,
                        timeout_ms: Some(provider.timeout_ms),
                        background: false,
                    },
                    &context,
                )
                .map_err(|_| "invalid command")?;
            let call = call
                .observe_processes(process)
                .map_err(|_| "executable unavailable")?;
            let authorization = match policy.classify(call) {
                Classification::Settled(Ok(grant)) => grant,
                Classification::Ask(pending) => match policy.settle(pending).await {
                    Resolution::Settled(Ok(grant)) => grant,
                    _ => return Err("permission required or denied"),
                },
                _ => return Err("permission denied"),
            };
            let (call, guarded) = effects.clone().bind(authorization);
            let mut result = tool
                .execute(call, &guarded)
                .await
                .map_err(|_| "execution failed or timed out")?;
            result.mask_secrets(&ego_tools::secrets::from_env());
            output(&result)
        }
        .await;
        match collected {
            Ok(value) => values.push((name.clone(), value)),
            Err(reason) => diagnostics.push(format!("status provider {name}: {reason}")),
        }
    }
    (values, diagnostics)
}

fn output(result: &ToolResult) -> Result<String, &'static str> {
    let mut stdout = None;
    for block in &result.content {
        match &block.provenance {
            ToolOutputProvenance::Process {
                stream,
                encoding: ToolOutputEncoding::Utf8,
                truncated: false,
                omitted_bytes: 0,
            } if block.text.len() <= 1024 => match stream {
                StreamProvenance::Stdout if stdout.is_none() => stdout = Some(block.text.as_str()),
                StreamProvenance::Stderr if block.text.is_empty() => {}
                _ => return Err("invalid output"),
            },
            _ => return Err("invalid or oversized output"),
        }
    }
    let stdout = stdout.ok_or("missing stdout")?;
    let text = stdout.strip_suffix('\n').unwrap_or(stdout);
    if text.contains(['\r', '\n']) {
        return Err("output must be one line");
    }
    Ok(ego_core::permit::terminal_safe(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::permit::ToolContentBlock;

    fn result(stdout: &str, stderr: &str) -> ToolResult {
        ToolResult::new(
            [
                (StreamProvenance::Stdout, stdout),
                (StreamProvenance::Stderr, stderr),
            ]
            .into_iter()
            .map(|(stream, text)| ToolContentBlock {
                text: text.into(),
                provenance: ToolOutputProvenance::Process {
                    stream,
                    encoding: ToolOutputEncoding::Utf8,
                    truncated: false,
                    omitted_bytes: 0,
                },
                full_output: None,
            })
            .collect(),
        )
    }

    #[test]
    fn provider_text_cannot_inject_terminal_controls_or_multiline_rows() {
        // Catches: external status text injecting escape sequences or unbounded terminal rows.
        let safe = output(&result("branch\u{1b}[2J\u{7}end\n", "")).unwrap();
        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains('\u{7}'));
        for (out, err) in [
            ("one\ntwo", ""),
            ("one\rtwo", ""),
            ("value", "warning"),
            ("value\n\n", ""),
        ] {
            assert!(output(&result(out, err)).is_err());
        }
        assert_eq!(output(&result("value\n", "")).unwrap(), "value");
        assert!(output(&result(&"x".repeat(1025), "")).is_err());
        assert_eq!(output(&result(&"x".repeat(1024), "")).unwrap().len(), 1024);
    }

    #[test]
    fn truncated_or_binary_process_evidence_never_becomes_a_status_value() {
        // Catches: admitting an incomplete preview or base64 encoding as a complete status value.
        for provenance in [
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                encoding: ToolOutputEncoding::Base64,
                truncated: false,
                omitted_bytes: 0,
            },
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                encoding: ToolOutputEncoding::Utf8,
                truncated: true,
                omitted_bytes: 1,
            },
            ToolOutputProvenance::Builtin,
        ] {
            let result = ToolResult::new(vec![ToolContentBlock {
                text: "value".into(),
                provenance,
                full_output: None,
            }]);
            assert!(output(&result).is_err());
        }
    }
}
