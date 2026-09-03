//! Root permission-rule management without discovery or execution.

use std::path::Path;
use std::sync::Arc;

use ego_core::config::rule_management::{
    EffectiveRule, OmittedDimension, RememberPlan, RuleExplanation, RuleManagement,
    RuleMutationComparison, RuleOutcome,
};
use ego_core::config::{
    ManagedRuleEffect, OriginSet, RuleMutation, RuleSnapshot, Ruleset, Source, UserConfigMutation,
    UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication, UserConfigStore,
    UserConfigTarget,
};
use ego_core::permit::rule::{Rule, RuleId};
use ego_core::permit::{CanonPath, PathResolver, PrepareCtx, PreparedCall, ToolError};
use ego_core::run::RunTermination;
use ego_tools::config::OsUserConfigStore;
use ego_tools::fs::OsFs;
use ego_tools::store::OsIo;

use crate::cli::{RulesAction, RulesArgs};
use crate::config::{escape_field, rule_management, user_target};
use crate::output::{self, Stdout};

pub fn execute(args: &RulesArgs, out: &Stdout) -> RunTermination {
    match execute_inner(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

fn execute_inner(args: &RulesArgs, out: &Stdout) -> Result<(), RunTermination> {
    let target = action_target(&args.action);
    let management = rule_management(target).map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let store = OsUserConfigStore::new(Arc::new(OsIo), management.user_path);

    match &args.action {
        RulesAction::List { target, json } => {
            let user = store.rule_snapshot(target.clone()).map_err(store_failure)?;
            let rows = RuleManagement::effective_rows(&management.system, &user);
            if *json {
                render_list_json(target, &rows, out);
            } else {
                render_list_text(&rows, out);
            }
        }
        RulesAction::Add {
            target,
            effect,
            rule,
        } => {
            let result = store
                .mutate(UserConfigMutation::Rules {
                    target: target.clone(),
                    changes: vec![RuleMutation {
                        effect: *effect,
                        rule: rule.clone(),
                    }],
                })
                .map_err(store_failure)?;
            let comparison = comparison(&management.system, &result, rule, *effect)?;
            render_mutation_receipt(
                &result,
                comparison,
                effect_name(*effect),
                Some(rule.id()),
                Some(rule),
                out,
            );
        }
        RulesAction::Revoke { target, id } => {
            let result = store
                .mutate(UserConfigMutation::RevokeRule {
                    target: target.clone(),
                    id: id.clone(),
                })
                .map_err(store_failure)?;
            let (_, before, after) = transaction(&result)?;
            let removed = rule_by_id(before, id);
            let comparison = match removed {
                Some((effect, rule)) => {
                    RuleManagement::compare(&management.system, before, after, rule, effect)
                }
                None => RuleMutationComparison {
                    stored_changed: false,
                    effective_changed: false,
                    authorities: Vec::new(),
                },
            };
            render_mutation_receipt(
                &result,
                comparison,
                "both",
                removed.map(|(_, rule)| rule.id()),
                removed.map(|(_, rule)| rule),
                out,
            );
        }
        RulesAction::Remember {
            target,
            directory,
            yes,
            effect,
            tool,
            arguments,
        } => {
            let workspace = workspace(directory.as_deref())?;
            let call = prepare_call(tool, arguments, &workspace)?;
            let plan =
                RuleManagement::remember(&call, *effect, workspace.as_path()).map_err(|error| {
                    RunTermination::UsageError {
                        message: error.to_string(),
                    }
                })?;
            if !yes {
                render_remember_plan(&plan, *effect, out);
                out.line(&format!(
                    "RESULT\tstored_unchanged\teffective_unchanged\t{}\tnot_published",
                    escape_field(&user_target(target))
                ));
                return Ok(());
            }

            let result = store
                .mutate(UserConfigMutation::Rules {
                    target: target.clone(),
                    changes: plan
                        .candidates
                        .iter()
                        .cloned()
                        .map(|rule| RuleMutation {
                            effect: *effect,
                            rule,
                        })
                        .collect(),
                })
                .map_err(store_failure)?;
            let (_, before, after) = transaction(&result)?;
            let effective_changed = plan.candidates.iter().any(|rule| {
                RuleManagement::compare(&management.system, before, after, rule, *effect)
                    .effective_changed
            });
            render_remember_plan(&plan, *effect, out);
            let publication = publication(&result.publication);
            out.line(&format!(
                "RESULT\t{}\t{}\t{}\t{}",
                stored_state(result.changed),
                effective_state(effective_changed),
                escape_field(&user_target(target)),
                publication
            ));
            publication_warning(&result.publication);
        }
        RulesAction::Explain {
            target,
            directory,
            json,
            tool,
            arguments,
        } => {
            let workspace = workspace(directory.as_deref())?;
            let call = prepare_call(tool, arguments, &workspace)?;
            let user = store.rule_snapshot(target.clone()).map_err(store_failure)?;
            let rules = RuleManagement::effective(&management.system, &user);
            let explanation = RuleManagement::explain(&call, &rules, workspace.as_path());
            if *json {
                render_explanation_json(target, &call, &explanation, out);
            } else {
                render_explanation_text(&call, &explanation, out);
            }
        }
    }
    Ok(())
}

fn action_target(action: &RulesAction) -> &UserConfigTarget {
    match action {
        RulesAction::List { target, .. }
        | RulesAction::Add { target, .. }
        | RulesAction::Revoke { target, .. }
        | RulesAction::Remember { target, .. }
        | RulesAction::Explain { target, .. } => target,
    }
}

fn workspace(directory: Option<&Path>) -> Result<CanonPath, RunTermination> {
    let current = std::env::current_dir().map_err(|error| RunTermination::Failed {
        message: format!("current directory could not be read: {error}"),
    })?;
    let named = directory
        .map(|directory| {
            if directory.is_absolute() {
                directory.to_path_buf()
            } else {
                current.join(directory)
            }
        })
        .unwrap_or(current);
    let invalid_directory = || RunTermination::UsageError {
        message: "-C must name an existing directory".to_owned(),
    };
    if directory.is_some() && !std::fs::metadata(&named).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(invalid_directory());
    }
    OsFs.canonicalize(&named).map_err(|error| {
        if directory.is_some() {
            invalid_directory()
        } else {
            RunTermination::Failed {
                message: format!("workspace could not be resolved: {error}"),
            }
        }
    })
}

fn prepare_call(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
    workspace: &CanonPath,
) -> Result<PreparedCall, RunTermination> {
    let tools = ego_tools::tools::builtins();
    let tool = tools
        .iter()
        .find(|tool| tool.descriptor().name == name)
        .ok_or_else(|| RunTermination::UsageError {
            message: "unknown rule-management builtin tool".to_owned(),
        })?;
    let resolver = OsFs;
    let context = PrepareCtx {
        cwd: workspace.clone(),
        resolver: &resolver,
    };
    tool.prepare(serde_json::Value::Object(arguments.clone()), &context)
        .map_err(|error| match error {
            ToolError::InvalidParams(_) => RunTermination::UsageError {
                message: "tool arguments do not satisfy the builtin schema".to_owned(),
            },
            other => RunTermination::Failed {
                message: format!("tool call could not be prepared: {other}"),
            },
        })
}

fn comparison(
    system: &Ruleset,
    result: &UserConfigMutationResult,
    rule: &Rule,
    effect: ManagedRuleEffect,
) -> Result<RuleMutationComparison, RunTermination> {
    let (_, before, after) = transaction(result)?;
    Ok(RuleManagement::compare(system, before, after, rule, effect))
}

fn transaction(
    result: &UserConfigMutationResult,
) -> Result<(&UserConfigMutationFacts, &RuleSnapshot, &RuleSnapshot), RunTermination> {
    result
        .rule_transaction()
        .ok_or_else(|| RunTermination::Failed {
            message: "rule mutation returned no atomic before/after facts".to_owned(),
        })
}

fn rule_by_id<'a>(
    snapshot: &'a RuleSnapshot,
    id: &RuleId,
) -> Option<(ManagedRuleEffect, &'a Rule)> {
    snapshot
        .allow
        .iter()
        .find(|rule| rule.id() == *id)
        .map(|rule| (ManagedRuleEffect::Allow, rule))
        .or_else(|| {
            snapshot
                .deny
                .iter()
                .find(|rule| rule.id() == *id)
                .map(|rule| (ManagedRuleEffect::Deny, rule))
        })
}

fn render_list_text(rows: &[EffectiveRule], out: &Stdout) {
    for row in rows {
        out.line(&format!(
            "{}\t{}\t{}\t{}\t{}",
            row.id,
            effect_name(row.effect),
            escape_field(&row.rule.to_string()),
            row.from,
            render_also(row.also)
        ));
    }
}

fn render_list_json(target: &UserConfigTarget, rows: &[EffectiveRule], out: &Stdout) {
    let rows = rows
        .iter()
        .map(|row| {
            format!(
                "{{\"id\":{},\"effect\":{},\"rule\":{},\"from\":{},\"also\":{}}}",
                json_string(&row.id.to_string()),
                json_string(effect_name(row.effect)),
                json_string(&row.rule.to_string()),
                json_string(&row.from.to_string()),
                json_sources(row.also)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    out.line(&format!(
        "{{\"v\":1,\"scope\":\"effective\",\"profile\":{},\"rules\":[{}]}}",
        profile_json(target),
        rows
    ));
}

fn render_mutation_receipt(
    result: &UserConfigMutationResult,
    comparison: RuleMutationComparison,
    effect: &str,
    id: Option<RuleId>,
    rule: Option<&Rule>,
    out: &Stdout,
) {
    out.line(&format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        stored_state(comparison.stored_changed),
        effective_state(comparison.effective_changed),
        effect,
        id.map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        rule.map(|rule| escape_field(&rule.to_string()))
            .unwrap_or_else(|| "-".to_owned()),
        escape_field(&user_target(&result.target)),
        render_authorities(&comparison),
        publication(&result.publication)
    ));
    publication_warning(&result.publication);
}

fn render_remember_plan(plan: &RememberPlan, effect: ManagedRuleEffect, out: &Stdout) {
    for (operation, rule) in plan.candidate_rows() {
        out.line(&format!(
            "DERIVED\t{}\t{}\t{}\t{}",
            operation,
            effect_name(effect),
            rule.id(),
            escape_field(&rule.to_string())
        ));
    }
    for (operation, dimension) in plan.omission_rows() {
        out.line(&format!(
            "OMITTED\t{}\t{}",
            operation,
            omitted_name(dimension)
        ));
    }
}

fn render_explanation_text(call: &PreparedCall, explanation: &RuleExplanation, out: &Stdout) {
    out.line(&format!("TOOL\t{}", escape_field(&call.tool)));
    for operation in &explanation.operations {
        out.line(&format!(
            "OP\t{}\t{}\t{}",
            operation.index,
            operation.kind,
            escape_field(&operation.subject)
        ));
    }
    for authority in &explanation.authorities {
        out.line(&format!(
            "MATCH\t{}\t{}\t{}\t{}\t{}",
            authority.operation,
            effect_name(authority.effect),
            authority.id,
            authority.from,
            render_also(authority.also)
        ));
    }
    out.line(&format!("OUTCOME\t{}", outcome_name(explanation.outcome)));
    out.line(&format!(
        "UNCOVERED\t{}",
        if explanation.uncovered.is_empty() {
            "-".to_owned()
        } else {
            explanation
                .uncovered
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        }
    ));
    out.line("SETTLEMENT\tnot_run");
    out.line("WOULD_EXECUTE\tfalse");
}

fn render_explanation_json(
    target: &UserConfigTarget,
    call: &PreparedCall,
    explanation: &RuleExplanation,
    out: &Stdout,
) {
    let operations = explanation
        .operations
        .iter()
        .map(|operation| {
            format!(
                "{{\"index\":{},\"kind\":{},\"subject\":{}}}",
                operation.index,
                json_string(&operation.kind.to_string()),
                json_string(&operation.subject)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let authorities = explanation
        .authorities
        .iter()
        .map(|authority| {
            format!(
                "{{\"operation\":{},\"id\":{},\"effect\":{},\"from\":{},\"also\":{}}}",
                authority.operation,
                json_string(&authority.id.to_string()),
                json_string(effect_name(authority.effect)),
                json_string(&authority.from.to_string()),
                json_sources(authority.also)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let uncovered = explanation
        .uncovered
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    out.line(&format!(
        "{{\"v\":1,\"scope\":\"effective\",\"profile\":{},\"tool\":{},\"operations\":[{}],\"outcome\":{},\"authorities\":[{}],\"uncovered\":[{}],\"settlement\":\"not_run\",\"would_execute\":false}}",
        profile_json(target),
        json_string(&call.tool),
        operations,
        json_string(outcome_name(explanation.outcome)),
        authorities,
        uncovered
    ));
}

fn render_authorities(comparison: &RuleMutationComparison) -> String {
    let mut sources = Vec::new();
    for source in Source::ALL {
        if comparison
            .authorities
            .iter()
            .any(|authority| authority.from == source || authority.also.contains(source))
        {
            sources.push(source.to_string());
        }
    }
    if sources.is_empty() {
        "-".to_owned()
    } else {
        sources.join(",")
    }
}

fn render_also(origins: OriginSet) -> String {
    let sources = origins
        .iter()
        .map(|source| source.to_string())
        .collect::<Vec<_>>();
    if sources.is_empty() {
        "-".to_owned()
    } else {
        sources.join(",")
    }
}

fn json_sources(origins: OriginSet) -> String {
    format!(
        "[{}]",
        origins
            .iter()
            .map(|source| json_string(&source.to_string()))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string is always valid JSON")
}

fn profile_json(target: &UserConfigTarget) -> String {
    target
        .profile()
        .map(|profile| json_string(profile.as_str()))
        .unwrap_or_else(|| "null".to_owned())
}

fn publication(publication: &UserConfigPublication) -> &'static str {
    match publication {
        UserConfigPublication::NotPublished => "not_published",
        UserConfigPublication::PublishedDurably => "published_durably",
        UserConfigPublication::PublishedDurabilityUncertain { .. } => {
            "published_durability_uncertain"
        }
    }
}

fn publication_warning(publication: &UserConfigPublication) {
    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } = publication {
        output::note(&format!(
            "rules were published but durability is uncertain: {diagnostic}"
        ));
    }
}

fn effect_name(effect: ManagedRuleEffect) -> &'static str {
    match effect {
        ManagedRuleEffect::Allow => "allow",
        ManagedRuleEffect::Deny => "deny",
    }
}

fn stored_state(changed: bool) -> &'static str {
    if changed {
        "stored_changed"
    } else {
        "stored_unchanged"
    }
}

fn effective_state(changed: bool) -> &'static str {
    if changed {
        "effective_changed"
    } else {
        "effective_unchanged"
    }
}

fn outcome_name(outcome: RuleOutcome) -> &'static str {
    match outcome {
        RuleOutcome::Allowed => "allowed",
        RuleOutcome::Denied => "denied",
        RuleOutcome::Uncovered => "uncovered",
    }
}

fn omitted_name(dimension: OmittedDimension) -> &'static str {
    match dimension {
        OmittedDimension::Change => "Change",
        OmittedDimension::Pattern => "Pattern",
        OmittedDimension::Query => "Query",
        OmittedDimension::Cwd => "Cwd",
        OmittedDimension::Timeout => "Timeout",
        OmittedDimension::Environment => "Environment",
        OmittedDimension::RequestDigest => "RequestDigest",
        OmittedDimension::ParametersDigest => "ParametersDigest",
        OmittedDimension::Transport => "Transport",
    }
}

fn store_failure(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: format!("user config rule operation failed before publication: {error}"),
    }
}
