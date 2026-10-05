//! Deterministic coordinator command policy. Unrecognized effects remain Unknown.

use std::path::Path;

use serde_json::Value;

use super::analysis::{RiskAssessment, RiskClass, RiskReason};
use super::{Op, PreparedCall};

/// Credit-funded advisor roles in the operator's orchestration policy.
pub const PAID_ROLES: &[&str] = &["astra", "fable", "grok"];

/// Named wrappers perform High operations regardless of their interpreter.
const HIGH_WRAPPERS: &[&str] = &["tuic-land.sh", "tuic-land-cleanup.sh"];

pub(super) fn assess(call: &PreparedCall) -> RiskAssessment {
    let foreign = call.mcp_envelope().and_then(|envelope| {
        if envelope.subject().server() != "tuicommander" {
            return None;
        }
        let params: Value = serde_json::from_slice(envelope.parameters()).ok()?;
        tuic(envelope.subject().operation_name(), &params)
    });
    let mut risk = RiskAssessment {
        class: RiskClass::Low,
        reasons: Vec::new(),
    };
    for op in call.ops() {
        let mut part = super::analysis::assess(&PreparedCall::new(call.tool(), op.clone()));
        let known = match op {
            Op::Spawn { spec } => {
                let argv = spec
                    .argv()
                    .iter()
                    .map(|arg| arg.to_str())
                    .collect::<Option<Vec<_>>>();
                argv.as_deref().and_then(command).or(foreign)
            }
            Op::Opaque { .. } | Op::ProtocolConnect { .. } => foreign,
            _ => None,
        };
        if let Some(class) = known {
            part.class = class;
            part.reasons = vec![RiskReason::OrchestrationPolicy];
        }
        risk.class = risk.class.max(part.class);
        risk.reasons.extend(part.reasons);
    }
    risk.reasons.sort();
    risk.reasons.dedup();
    risk
}

fn basename(text: &str) -> &str {
    Path::new(text)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(text)
}

fn command(argv: &[&str]) -> Option<RiskClass> {
    let (program, args) = argv.split_first()?;
    if HIGH_WRAPPERS
        .iter()
        .any(|name| argv.iter().any(|arg| basename(arg) == *name))
    {
        return Some(RiskClass::High);
    }
    match basename(program) {
        "rtk" => command(args),
        "bash" | "sh" | "zsh" if args.first().is_some_and(|arg| !arg.starts_with('-')) => {
            command(args)
        }
        "git" => {
            let mut args = args;
            while args.first().is_some_and(|arg| matches!(*arg, "-C" | "-c")) {
                args = args.get(2..)?;
            }
            match args.first().copied()? {
                "merge" | "push" => Some(RiskClass::High),
                "branch"
                    if args
                        .iter()
                        .any(|arg| matches!(*arg, "-d" | "-D" | "--delete")) =>
                {
                    Some(RiskClass::High)
                }
                "status" | "diff" | "log" | "show" => Some(RiskClass::Low),
                _ => None,
            }
        }
        "gh" if args.starts_with(&["pr", "create"]) => Some(RiskClass::High),
        "kill" | "killall" | "pkill" => Some(RiskClass::High),
        "tuic-spawn.sh" | "tuic-consult.sh" => args.first().map(|role| {
            if PAID_ROLES.contains(role) {
                RiskClass::High
            } else {
                RiskClass::Bounded
            }
        }),
        "tuic-triumvirate.sh" => Some(RiskClass::High),
        "tuic" if args.first() == Some(&"ls") => Some(RiskClass::Low),
        "find"
            if args.len() == 7
                && !args[0].starts_with('-')
                && args[1..6] == ["-maxdepth", "1", "-name", "*.md", "-newer"] =>
        {
            Some(RiskClass::Low)
        }
        "tuic" if args.first() == Some(&"mcp") => {
            let params = serde_json::from_str::<Value>(args.get(2)?).ok()?;
            tuic(args.get(1)?, &params)
        }
        "mdkb" | "wiz-run" => Some(RiskClass::Bounded),
        "curl"
            if args.iter().any(|arg| {
                arg.contains("openrouter.ai/api/")
                    || arg.contains("api.openai.com/")
                    || arg.contains("api.anthropic.com/")
            }) =>
        {
            Some(RiskClass::High)
        }
        _ => None,
    }
}

fn tuic(tool: &str, params: &Value) -> Option<RiskClass> {
    let action = params.get("action")?.as_str()?;
    match (tool, action) {
        ("repo", "worktree_remove" | "branch_delete" | "land") | ("session", "kill" | "close") => {
            Some(RiskClass::High)
        }
        ("agent", "spawn") => {
            let role = params
                .get("model")
                .or_else(|| params.get("agent_type"))
                .and_then(Value::as_str)?;
            Some(
                if PAID_ROLES
                    .iter()
                    .any(|paid| role == *paid || role.contains(paid))
                {
                    RiskClass::High
                } else {
                    RiskClass::Bounded
                },
            )
        }
        ("session", "list" | "status" | "output")
        | ("agent", "list_peers" | "inbox" | "wait")
        | ("progress", "list") => Some(RiskClass::Low),
        ("agent", "send" | "register")
        | ("session", "submit" | "input")
        | ("repo", "worktree_create") => Some(RiskClass::Bounded),
        _ => None,
    }
}
