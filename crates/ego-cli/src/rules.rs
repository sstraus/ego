//! Root permission-rule management without discovery or execution.

use std::path::Path;
use std::sync::Arc;

use ego_core::config::rule_management::{
    EffectiveRule, OmittedDimension, RememberPlan, RuleExplanation, RuleManagement,
    RuleManagementError, RuleMutationComparison, RuleOutcome,
};
use ego_core::config::{
    ManagedRuleEffect, OriginSet, RuleMutation, RuleSnapshot, Ruleset, Source, UserConfigMutation,
    UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication, UserConfigStore,
    UserConfigTarget,
};
use ego_core::permit::rule::{Rule, RuleId};
use ego_core::permit::{CanonPath, PathResolver, PrepareCtx, PreparedCall, Tool, ToolError};
use ego_core::run::RunTermination;
use ego_tools::config::OsUserConfigStore;
use ego_tools::fs::OsFs;
use ego_tools::store::OsIo;

use crate::cli::{RulesAction, RulesArgs};
use crate::config::{escape_field, rule_management, user_target};
use crate::output::{Stdout, TextOutput, empty_listing};

/// One prepared call whose representable permission rules may be remembered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberRequest {
    call: PreparedCall,
    effect: ManagedRuleEffect,
    workspace: CanonPath,
}

impl RememberRequest {
    pub fn new(call: PreparedCall, effect: ManagedRuleEffect, workspace: CanonPath) -> Self {
        Self {
            call,
            effect,
            workspace,
        }
    }
}

/// The complete, side-effect-free preview of a remember operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberPreview {
    effect: ManagedRuleEffect,
    plan: RememberPlan,
}

impl RememberPreview {
    pub(crate) fn presented(effect: ManagedRuleEffect, plan: RememberPlan) -> Self {
        Self { effect, plan }
    }

    pub(crate) fn effect(&self) -> ManagedRuleEffect {
        self.effect
    }

    pub(crate) fn plan(&self) -> &RememberPlan {
        &self.plan
    }
}

/// One verified atomic rule mutation and its effective-policy comparisons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMutationReceipt {
    mutation: UserConfigMutationResult,
    affected: Vec<RuleMutation>,
    comparisons: Vec<RuleMutationComparison>,
}

impl RuleMutationReceipt {
    /// Construct a receipt only when all facts describe the same transaction.
    pub fn from_atomic_facts(
        mutation: UserConfigMutationResult,
        affected: Vec<RuleMutation>,
        comparisons: Vec<RuleMutationComparison>,
    ) -> Result<Self, &'static str> {
        let expected_affected = validate_atomic_rule_result(&mutation, None)?;
        if affected != expected_affected {
            return Err("rule mutation receipt disagrees with its atomic request facts");
        }
        if comparisons.len() != affected.len()
            || comparisons
                .iter()
                .any(|comparison| comparison.stored_changed != mutation.changed)
        {
            return Err("rule mutation comparisons disagree with its atomic facts");
        }
        Ok(Self {
            mutation,
            affected,
            comparisons,
        })
    }

    pub fn publication(&self) -> UserConfigPublication {
        self.mutation.publication.clone()
    }

    pub(crate) fn mutation(&self) -> &UserConfigMutationResult {
        &self.mutation
    }

    pub(crate) fn affected(&self) -> &[RuleMutation] {
        &self.affected
    }

    pub(crate) fn comparisons(&self) -> &[RuleMutationComparison] {
        &self.comparisons
    }

    pub(crate) fn effective_changed(&self) -> bool {
        self.comparisons
            .iter()
            .any(|comparison| comparison.effective_changed)
    }
}

fn validate_atomic_rule_result(
    result: &UserConfigMutationResult,
    expected_request: Option<&UserConfigMutationFacts>,
) -> Result<Vec<RuleMutation>, &'static str> {
    let (request, before, after) = result
        .rule_transaction()
        .ok_or("rule mutation returned no atomic before/after facts")?;
    if before.target != result.target || after.target != result.target {
        return Err("rule mutation returned facts for a different target");
    }
    validate_rule_snapshot(before)?;
    validate_rule_snapshot(after)?;
    if expected_request.is_some_and(|expected| request != expected) {
        return Err("rule mutation returned different request facts");
    }

    let affected = match request {
        UserConfigMutationFacts::Batch { changes } => {
            let expected = expected_batch_after(before, changes)?;
            if &expected != after {
                return Err("rule mutation after facts do not apply its batch request");
            }
            changes.clone()
        }
        UserConfigMutationFacts::Revoke { id } => {
            validate_revoke_after(before, after, id)?;
            removed_rule_mutations(before, after, id)?
        }
        _ => return Err("rule mutation returned non-rule request facts"),
    };

    if result.changed != (before != after) {
        return Err("rule mutation changed flag disagrees with its atomic facts");
    }
    if result.changed && result.publication == UserConfigPublication::NotPublished {
        return Err("a changed mutation was reported as not published");
    }
    if !result.changed && result.publication != UserConfigPublication::NotPublished {
        return Err("an unchanged mutation was reported as published");
    }
    Ok(affected)
}

fn validate_rule_snapshot(snapshot: &RuleSnapshot) -> Result<(), &'static str> {
    if !snapshot.permissions_present && (!snapshot.allow.is_empty() || !snapshot.deny.is_empty()) {
        return Err("rule mutation returned rules outside a permission authority");
    }
    Ok(())
}

fn expected_batch_after(
    before: &RuleSnapshot,
    changes: &[RuleMutation],
) -> Result<RuleSnapshot, &'static str> {
    let mut expected = before.clone();
    let mut added = false;
    for change in changes {
        match change.effect {
            ManagedRuleEffect::Allow => {
                if expected.deny.contains(&change.rule) {
                    return Err("rule mutation batch contains an opposite-effect conflict");
                }
                if !expected.allow.contains(&change.rule) {
                    expected.allow.push(change.rule.clone());
                    added = true;
                }
            }
            ManagedRuleEffect::Deny => {
                if expected.allow.contains(&change.rule) {
                    return Err("rule mutation batch contains an opposite-effect conflict");
                }
                if !expected.deny.contains(&change.rule) {
                    expected.deny.push(change.rule.clone());
                    added = true;
                }
            }
        }
    }
    if added {
        expected.permissions_present = true;
    }
    Ok(expected)
}

fn removed_rule_mutations(
    before: &RuleSnapshot,
    after: &RuleSnapshot,
    id: &RuleId,
) -> Result<Vec<RuleMutation>, &'static str> {
    let affected = before
        .allow
        .iter()
        .filter(|rule| rule.id() == *id && !after.allow.contains(rule))
        .cloned()
        .map(|rule| RuleMutation {
            effect: ManagedRuleEffect::Allow,
            rule,
        })
        .chain(
            before
                .deny
                .iter()
                .filter(|rule| rule.id() == *id && !after.deny.contains(rule))
                .cloned()
                .map(|rule| RuleMutation {
                    effect: ManagedRuleEffect::Deny,
                    rule,
                }),
        )
        .collect::<Vec<_>>();
    if let Some(first) = affected.first()
        && affected
            .iter()
            .any(|candidate| candidate.rule != first.rule)
    {
        return Err("rule mutation revoke facts contain an ambiguous rule ID");
    }
    Ok(affected)
}

fn validate_revoke_after(
    before: &RuleSnapshot,
    after: &RuleSnapshot,
    id: &RuleId,
) -> Result<(), &'static str> {
    let expected_allow = before
        .allow
        .iter()
        .filter(|rule| rule.id() != *id)
        .cloned()
        .collect::<Vec<_>>();
    let expected_deny = before
        .deny
        .iter()
        .filter(|rule| rule.id() != *id)
        .cloned()
        .collect::<Vec<_>>();
    if after.allow != expected_allow || after.deny != expected_deny {
        return Err("rule mutation after facts do not apply its revoke request");
    }
    let removed =
        expected_allow.len() != before.allow.len() || expected_deny.len() != before.deny.len();
    if !removed && after.permissions_present != before.permissions_present {
        return Err("an unchanged revoke altered the permission authority");
    }
    Ok(())
}

pub(crate) enum RuleReadAuthority<'a> {
    Stored { system: &'a Ruleset },
    Admitted(&'a Ruleset),
}

#[derive(Debug)]
pub(crate) enum RuleApplicationError {
    Store(String),
    Management(RuleManagementError),
    Contract(&'static str),
}

pub(crate) struct RuleChange {
    mutation: UserConfigMutationResult,
    affected: Vec<RuleMutation>,
}

impl RuleChange {
    pub(crate) fn transaction(&self) -> (&RuleSnapshot, &RuleSnapshot) {
        let (_, before, after) = self
            .mutation
            .rule_transaction()
            .expect("RuleApplication admits only atomic rule facts");
        (before, after)
    }

    pub(crate) fn affected(&self) -> &[RuleMutation] {
        &self.affected
    }

    pub(crate) fn changed(&self) -> bool {
        self.mutation.changed
    }

    pub(crate) fn comparisons(&self, system: &Ruleset) -> Vec<RuleMutationComparison> {
        let (before, after) = self.transaction();
        self.affected
            .iter()
            .map(|mutation| {
                RuleManagement::compare(system, before, after, &mutation.rule, mutation.effect)
            })
            .collect()
    }

    pub(crate) fn into_receipt(
        self,
        comparisons: Vec<RuleMutationComparison>,
    ) -> Result<RuleMutationReceipt, RuleApplicationError> {
        RuleMutationReceipt::from_atomic_facts(self.mutation, self.affected, comparisons)
            .map_err(RuleApplicationError::Contract)
    }

    pub(crate) fn into_unchanged_receipt(
        self,
    ) -> Result<RuleMutationReceipt, RuleApplicationError> {
        let comparisons = self
            .affected
            .iter()
            .map(|_| RuleMutationComparison {
                stored_changed: false,
                effective_changed: false,
                authorities: Vec::new(),
            })
            .collect();
        self.into_receipt(comparisons)
    }
}

/// Shared application service for the six rule-management operations.
pub(crate) struct RuleApplication {
    store: Arc<dyn UserConfigStore>,
    target: UserConfigTarget,
}

impl RuleApplication {
    pub(crate) fn new(store: Arc<dyn UserConfigStore>, target: UserConfigTarget) -> Self {
        Self { store, target }
    }

    pub(crate) fn list(
        &self,
        authority: RuleReadAuthority<'_>,
    ) -> Result<Vec<EffectiveRule>, RuleApplicationError> {
        match authority {
            RuleReadAuthority::Stored { system } => {
                let user = self
                    .store
                    .rule_snapshot(self.target.clone())
                    .map_err(|error| RuleApplicationError::Store(error.to_string()))?;
                Ok(RuleManagement::effective_rows(system, &user))
            }
            RuleReadAuthority::Admitted(ruleset) => Ok(RuleManagement::rows(ruleset)),
        }
    }

    pub(crate) fn explain(
        &self,
        call: &PreparedCall,
        workspace: &CanonPath,
        authority: RuleReadAuthority<'_>,
    ) -> Result<RuleExplanation, RuleApplicationError> {
        let rules = match authority {
            RuleReadAuthority::Stored { system } => {
                let user = self
                    .store
                    .rule_snapshot(self.target.clone())
                    .map_err(|error| RuleApplicationError::Store(error.to_string()))?;
                RuleManagement::effective(system, &user)
            }
            RuleReadAuthority::Admitted(ruleset) => ruleset.clone(),
        };
        Ok(RuleManagement::explain(call, &rules, workspace.as_path()))
    }

    pub(crate) fn allow(&self, rule: Rule) -> Result<RuleChange, RuleApplicationError> {
        self.add(ManagedRuleEffect::Allow, rule)
    }

    pub(crate) fn deny(&self, rule: Rule) -> Result<RuleChange, RuleApplicationError> {
        self.add(ManagedRuleEffect::Deny, rule)
    }

    pub(crate) fn revoke(&self, id: RuleId) -> Result<RuleChange, RuleApplicationError> {
        self.mutate(UserConfigMutation::RevokeRule {
            target: self.target.clone(),
            id,
        })
    }

    pub(crate) fn preview_remember(
        &self,
        request: RememberRequest,
    ) -> Result<RememberPreview, RuleApplicationError> {
        let plan =
            RuleManagement::remember(&request.call, request.effect, request.workspace.as_path())
                .map_err(RuleApplicationError::Management)?;
        Ok(RememberPreview {
            effect: request.effect,
            plan,
        })
    }

    pub(crate) fn confirm_remember(
        &self,
        preview: RememberPreview,
    ) -> Result<RuleChange, RuleApplicationError> {
        let changes = preview
            .plan
            .candidates
            .iter()
            .cloned()
            .map(|rule| RuleMutation {
                effect: preview.effect,
                rule,
            })
            .collect::<Vec<_>>();
        self.mutate(UserConfigMutation::Rules {
            target: self.target.clone(),
            changes,
        })
    }

    fn add(
        &self,
        effect: ManagedRuleEffect,
        rule: Rule,
    ) -> Result<RuleChange, RuleApplicationError> {
        let affected = vec![RuleMutation { effect, rule }];
        self.mutate(UserConfigMutation::Rules {
            target: self.target.clone(),
            changes: affected,
        })
    }

    fn mutate(&self, request: UserConfigMutation) -> Result<RuleChange, RuleApplicationError> {
        let expected_facts = request.facts();
        let result = self
            .store
            .mutate(request)
            .map_err(|error| RuleApplicationError::Store(error.to_string()))?;
        if result.target != self.target {
            return Err(RuleApplicationError::Contract(
                "rule mutation returned facts for a different target",
            ));
        }
        let affected = validate_atomic_rule_result(&result, Some(&expected_facts))
            .map_err(RuleApplicationError::Contract)?;
        Ok(RuleChange {
            mutation: result,
            affected,
        })
    }
}

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
    let store: Arc<dyn UserConfigStore> =
        Arc::new(OsUserConfigStore::new(Arc::new(OsIo), management.user_path));
    let application = RuleApplication::new(store, target.clone());

    match &args.action {
        RulesAction::List { target, json } => {
            let rows = application
                .list(RuleReadAuthority::Stored {
                    system: &management.system,
                })
                .map_err(application_failure)?;
            if *json {
                render_list_json(target, &rows, out);
            } else {
                render_list_text(&rows, out);
            }
        }
        RulesAction::Add { effect, rule, .. } => {
            let change = match effect {
                ManagedRuleEffect::Allow => application.allow(rule.clone()),
                ManagedRuleEffect::Deny => application.deny(rule.clone()),
            }
            .map_err(application_failure)?;
            let comparisons = change.comparisons(&management.system);
            let receipt = change
                .into_receipt(comparisons)
                .map_err(application_failure)?;
            render_mutation_receipt(&receipt, effect_name(*effect), out);
        }
        RulesAction::Revoke { id, .. } => {
            let change = application
                .revoke(id.clone())
                .map_err(application_failure)?;
            let comparisons = change.comparisons(&management.system);
            let receipt = change
                .into_receipt(comparisons)
                .map_err(application_failure)?;
            render_mutation_receipt(&receipt, "both", out);
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
            let preview = application
                .preview_remember(RememberRequest::new(call, *effect, workspace))
                .map_err(application_failure)?;
            if !yes {
                render_remember_plan(preview.plan(), preview.effect(), out);
                render_rows(render_remember_result_rows(target, None), out);
                return Ok(());
            }

            let rendered_preview = preview.clone();
            let change = application
                .confirm_remember(preview)
                .map_err(application_failure)?;
            let comparisons = change.comparisons(&management.system);
            let receipt = change
                .into_receipt(comparisons)
                .map_err(application_failure)?;
            render_remember_plan(rendered_preview.plan(), rendered_preview.effect(), out);
            render_rows(render_remember_result_rows(target, Some(&receipt)), out);
            publication_warning(&receipt.mutation().publication, out);
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
            let explanation = application
                .explain(
                    &call,
                    &workspace,
                    RuleReadAuthority::Stored {
                        system: &management.system,
                    },
                )
                .map_err(application_failure)?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreparationError {
    UnknownTool { name: String },
    AmbiguousTool { name: String },
    InvalidParams { diagnostic: String },
    PreparationFailed { diagnostic: String },
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool { name } => write!(formatter, "unknown admitted tool `{name}`"),
            Self::AmbiguousTool { name } => {
                write!(formatter, "admitted tool name `{name}` is ambiguous")
            }
            Self::InvalidParams { diagnostic } => {
                write!(formatter, "tool arguments are invalid: {diagnostic}")
            }
            Self::PreparationFailed { diagnostic } => {
                write!(formatter, "tool call could not be prepared: {diagnostic}")
            }
        }
    }
}

pub(crate) fn prepare_from_catalogue(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
    workspace: &CanonPath,
    resolver: &dyn PathResolver,
    admitted_tools: &[Arc<dyn Tool>],
) -> Result<PreparedCall, PreparationError> {
    let mut matches = admitted_tools
        .iter()
        .filter(|tool| tool.descriptor().name == name);
    let Some(tool) = matches.next() else {
        return Err(PreparationError::UnknownTool {
            name: name.to_owned(),
        });
    };
    if matches.next().is_some() {
        return Err(PreparationError::AmbiguousTool {
            name: name.to_owned(),
        });
    }
    let context = PrepareCtx {
        cwd: workspace.clone(),
        resolver,
    };
    tool.prepare(serde_json::Value::Object(arguments.clone()), &context)
        .map_err(|error| match error {
            ToolError::InvalidParams(diagnostic) => PreparationError::InvalidParams { diagnostic },
            other => PreparationError::PreparationFailed {
                diagnostic: other.to_string(),
            },
        })
}

fn prepare_call(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
    workspace: &CanonPath,
) -> Result<PreparedCall, RunTermination> {
    let tools = ego_tools::tools::builtins();
    prepare_from_catalogue(name, arguments, workspace, &OsFs, &tools).map_err(|error| match error {
        PreparationError::UnknownTool { .. } => RunTermination::UsageError {
            message: "unknown rule-management builtin tool".to_owned(),
        },
        PreparationError::AmbiguousTool { .. } => RunTermination::Failed {
            message: "rule-management builtin tool name is ambiguous".to_owned(),
        },
        PreparationError::InvalidParams { .. } => RunTermination::UsageError {
            message: "tool arguments do not satisfy the builtin schema".to_owned(),
        },
        PreparationError::PreparationFailed { diagnostic } => RunTermination::Failed {
            message: format!("tool call could not be prepared: {diagnostic}"),
        },
    })
}

pub(crate) fn render_list_rows(rows: &[EffectiveRule]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                row.id,
                effect_name(row.effect),
                escape_field(&row.rule.to_string()),
                row.from,
                render_also(row.also)
            )
        })
        .collect()
}

fn render_rows(rows: Vec<String>, out: &dyn TextOutput) {
    for row in rows {
        out.line(&row);
    }
}

fn render_list_text(rows: &[EffectiveRule], out: &dyn TextOutput) {
    if rows.is_empty() {
        // The rule ADR fixes the text listing as tab-separated rows, so an
        // empty one has no stdout at all and says why on stderr instead.
        out.note(&empty_listing(
            "no permission rules are in effect",
            "ego rules allow",
        ));
        return;
    }
    render_rows(render_list_rows(rows), out);
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

pub(crate) fn render_mutation_receipt_rows(
    receipt: &RuleMutationReceipt,
    effect: ManagedRuleEffect,
) -> Vec<String> {
    render_mutation_receipt_rows_named(receipt, effect_name(effect))
}

pub(crate) fn render_revoke_receipt_rows(receipt: &RuleMutationReceipt) -> Vec<String> {
    render_mutation_receipt_rows_named(receipt, "both")
}

fn render_mutation_receipt_rows_named(receipt: &RuleMutationReceipt, effect: &str) -> Vec<String> {
    let row = |affected: Option<&RuleMutation>| {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            stored_state(receipt.mutation().changed),
            effective_state(receipt.effective_changed()),
            effect,
            affected
                .map(|mutation| mutation.rule.id().to_string())
                .unwrap_or_else(|| "-".to_owned()),
            affected
                .map(|mutation| escape_field(&mutation.rule.to_string()))
                .unwrap_or_else(|| "-".to_owned()),
            escape_field(&user_target(&receipt.mutation().target)),
            render_authorities(receipt.comparisons()),
            publication(&receipt.mutation().publication)
        )
    };
    vec![row(receipt.affected().first())]
}

fn render_mutation_receipt(receipt: &RuleMutationReceipt, effect: &str, out: &dyn TextOutput) {
    render_rows(render_mutation_receipt_rows_named(receipt, effect), out);
    publication_warning(&receipt.mutation().publication, out);
}

pub(crate) fn render_remember_preview_rows(preview: &RememberPreview) -> Vec<String> {
    render_remember_plan_rows(preview.plan(), preview.effect())
}

fn render_remember_plan_rows(plan: &RememberPlan, effect: ManagedRuleEffect) -> Vec<String> {
    let mut rows = Vec::new();
    for (operation, rule) in plan.candidate_rows() {
        rows.push(format!(
            "DERIVED\t{}\t{}\t{}\t{}",
            operation,
            effect_name(effect),
            rule.id(),
            escape_field(&rule.to_string())
        ));
    }
    for (operation, dimension) in plan.omission_rows() {
        rows.push(format!(
            "OMITTED\t{}\t{}",
            operation,
            omitted_name(dimension)
        ));
    }
    rows
}

fn render_remember_plan(plan: &RememberPlan, effect: ManagedRuleEffect, out: &dyn TextOutput) {
    render_rows(render_remember_plan_rows(plan, effect), out);
}

pub(crate) fn render_explanation_rows(
    call: &PreparedCall,
    explanation: &RuleExplanation,
) -> Vec<String> {
    let mut rows = vec![format!("TOOL\t{}", escape_field(call.tool()))];
    for operation in &explanation.operations {
        rows.push(format!(
            "OP\t{}\t{}\t{}",
            operation.index,
            operation.kind,
            escape_field(&operation.subject)
        ));
    }
    for authority in &explanation.authorities {
        rows.push(format!(
            "MATCH\t{}\t{}\t{}\t{}\t{}",
            authority.operation,
            effect_name(authority.effect),
            authority.id,
            authority.from,
            render_also(authority.also)
        ));
    }
    rows.push(format!("OUTCOME\t{}", outcome_name(explanation.outcome)));
    rows.push(format!(
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
    rows.push("SETTLEMENT\tnot_run".to_owned());
    rows.push("WOULD_EXECUTE\tfalse".to_owned());
    rows
}

fn render_explanation_text(
    call: &PreparedCall,
    explanation: &RuleExplanation,
    out: &dyn TextOutput,
) {
    render_rows(render_explanation_rows(call, explanation), out);
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
        json_string(call.tool()),
        operations,
        json_string(outcome_name(explanation.outcome)),
        authorities,
        uncovered
    ));
}

fn render_authorities(comparisons: &[RuleMutationComparison]) -> String {
    let mut sources = Vec::new();
    for source in Source::ALL {
        if comparisons
            .iter()
            .flat_map(|comparison| &comparison.authorities)
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

pub(crate) fn render_remember_result_rows(
    target: &UserConfigTarget,
    receipt: Option<&RuleMutationReceipt>,
) -> Vec<String> {
    let (stored, effective, publication) = receipt.map_or(
        ("stored_unchanged", "effective_unchanged", "not_published"),
        |receipt| {
            (
                stored_state(receipt.mutation().changed),
                effective_state(receipt.effective_changed()),
                publication(&receipt.mutation().publication),
            )
        },
    );
    vec![format!(
        "RESULT\t{stored}\t{effective}\t{}\t{publication}",
        escape_field(&user_target(target))
    )]
}

pub(crate) fn publication_warning_text(publication: &UserConfigPublication) -> Option<String> {
    match publication {
        UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => Some(format!(
            "rules changed, but saving them to disk could not be confirmed: {diagnostic}"
        )),
        _ => None,
    }
}

fn publication_warning(publication: &UserConfigPublication, out: &dyn TextOutput) {
    if let Some(warning) = publication_warning_text(publication) {
        out.note(&warning);
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

fn application_failure(error: RuleApplicationError) -> RunTermination {
    match error {
        RuleApplicationError::Management(error) => RunTermination::UsageError {
            message: error.to_string(),
        },
        RuleApplicationError::Store(diagnostic) => store_failure(diagnostic),
        RuleApplicationError::Contract(message) => RunTermination::Failed {
            message: message.to_owned(),
        },
    }
}

#[cfg(test)]
mod permission_red {
    use super::*;
    use crate::output::{Diagnostics, TextOutput};
    use crate::repl::policy_controls::RememberPreview;
    use ego_core::generator::attempt::DeliveryCertainty;
    use ego_core::mcp::ToolExecution;
    use ego_core::permit::{GuardedIo, Op, Tool, ToolDescriptor, ToolResult};
    use futures::future::BoxFuture;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AdmittedToolFake {
        name: String,
        result: Result<PreparedCall, ToolError>,
        prepares: Arc<AtomicUsize>,
    }

    impl Tool for AdmittedToolFake {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.name.clone(),
                description: "permission fixture".into(),
                schema: serde_json::from_value(serde_json::json!({"type":"object"}))
                    .expect("fixture schema"),
            }
        }

        fn prepare(
            &self,
            _params: Value,
            _ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }

        fn execute<'a, 'io>(
            &'a self,
            _call: PreparedCall,
            _io: &'a dyn GuardedIo<'io>,
        ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
        where
            'io: 'a,
        {
            Box::pin(async { Ok(ToolExecution::Complete(ToolResult::builtin("unused"))) })
        }
    }

    #[test]
    fn catalogue_preparation_has_closed_errors_and_no_discovery_or_mutation() {
        let workspace = CanonPath::new("/workspace");
        let arguments = serde_json::json!({"path": "src/main.rs"})
            .as_object()
            .unwrap()
            .clone();
        let resolver = OsFs;
        let prepares = Arc::new(AtomicUsize::new(0));
        let expected = PreparedCall::new(
            "foreign_read",
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
        );
        let foreign: Arc<dyn Tool> = Arc::new(AdmittedToolFake {
            name: "foreign_read".into(),
            result: Ok(expected.clone()),
            prepares: Arc::clone(&prepares),
        });
        let admitted = [Arc::clone(&foreign)];
        let prepared =
            prepare_from_catalogue("foreign_read", &arguments, &workspace, &resolver, &admitted)
                .expect("admitted foreign tool prepares");
        assert_eq!(prepared.into_parts(), expected.into_parts());
        assert_eq!(prepares.load(Ordering::SeqCst), 1);

        assert!(matches!(
            prepare_from_catalogue("missing", &arguments, &workspace, &resolver, &admitted),
            Err(PreparationError::UnknownTool { .. })
        ));
        assert_eq!(prepares.load(Ordering::SeqCst), 1);

        let duplicate_calls = Arc::new(AtomicUsize::new(0));
        let duplicate_a: Arc<dyn Tool> = Arc::new(AdmittedToolFake {
            name: "foreign_read".into(),
            result: Ok(PreparedCall::new(
                "foreign_read",
                Op::Read {
                    path: CanonPath::new("/workspace/a"),
                },
            )),
            prepares: Arc::clone(&duplicate_calls),
        });
        let duplicate_b: Arc<dyn Tool> = Arc::new(AdmittedToolFake {
            name: "foreign_read".into(),
            result: Ok(PreparedCall::new(
                "foreign_read",
                Op::Read {
                    path: CanonPath::new("/workspace/b"),
                },
            )),
            prepares: Arc::clone(&duplicate_calls),
        });
        let duplicates = [duplicate_a, duplicate_b];
        assert!(matches!(
            prepare_from_catalogue(
                "foreign_read",
                &arguments,
                &workspace,
                &resolver,
                &duplicates,
            ),
            Err(PreparationError::AmbiguousTool { .. })
        ));
        assert_eq!(duplicate_calls.load(Ordering::SeqCst), 0);

        let invalid: Arc<dyn Tool> = Arc::new(AdmittedToolFake {
            name: "foreign_read".into(),
            result: Err(ToolError::InvalidParams("bad params".into())),
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let invalid_result = prepare_from_catalogue(
            "foreign_read",
            &arguments,
            &workspace,
            &resolver,
            &[invalid],
        )
        .expect_err("invalid params are usage errors");
        assert!(matches!(
            invalid_result,
            PreparationError::InvalidParams { .. }
        ));

        let failed: Arc<dyn Tool> = Arc::new(AdmittedToolFake {
            name: "foreign_read".into(),
            result: Err(ToolError::foreign(
                "transport failed",
                DeliveryCertainty::DefinitelyUnsent,
            )),
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        assert!(matches!(
            prepare_from_catalogue("foreign_read", &arguments, &workspace, &resolver, &[failed]),
            Err(PreparationError::PreparationFailed { .. })
        ));
    }

    #[test]
    fn permission_rows_are_shared_with_root_and_interactive_renderers() {
        let rule = Rule::load("read(src/**)").unwrap();
        let rules = Ruleset::from_layer(
            ego_core::config::PermCfg {
                judge: None,
                allow: vec![rule],
                deny: vec![],
            },
            Source::Project,
        );
        let expected_list = format!(
            "{}\tallow\tread(src/**)\tproject\t-",
            rules.allow[0].rule().id()
        );
        assert_eq!(
            render_list_rows(&RuleManagement::rows(&rules)),
            vec![expected_list]
        );
        let mutation = RuleMutation {
            effect: ManagedRuleEffect::Allow,
            rule: Rule::load("read(src/**)").unwrap(),
        };
        let before = RuleSnapshot {
            target: UserConfigTarget::Base,
            permissions_present: true,
            allow: vec![],
            deny: vec![],
        };
        let after = RuleSnapshot {
            target: UserConfigTarget::Base,
            permissions_present: true,
            allow: vec![mutation.rule.clone()],
            deny: vec![],
        };
        let receipt = crate::repl::policy_controls::RuleMutationReceipt::from_atomic_facts(
            UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(UserConfigMutationFacts::Batch {
                        changes: vec![mutation.clone()],
                    }),
                    before,
                    after,
                },
                changed: true,
                publication: UserConfigPublication::PublishedDurably,
            },
            vec![mutation.clone()],
            vec![RuleMutationComparison {
                stored_changed: true,
                effective_changed: true,
                authorities: vec![],
            }],
        )
        .unwrap();
        let receipt_rows = render_mutation_receipt_rows(&receipt, ManagedRuleEffect::Allow);
        assert_eq!(receipt_rows.len(), 1);
        assert_eq!(
            receipt_rows[0],
            format!(
                "stored_changed\teffective_changed\tallow\t{}\tread(src/**)\tuser\t-\tpublished_durably",
                mutation.rule.id()
            )
        );

        let call = PreparedCall::new(
            "remember",
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
        )
        .with_op(Op::Write {
            path: CanonPath::new("/workspace/src/main.rs"),
            change: ego_core::permit::Change::Whole(b"next".to_vec()),
        });
        let plan =
            RuleManagement::remember(&call, ManagedRuleEffect::Allow, Path::new("/workspace"))
                .unwrap();
        let preview = RememberPreview {
            effect: ManagedRuleEffect::Allow,
            plan,
        };
        let preview_rows = render_remember_preview_rows(&preview);
        assert!(
            preview_rows
                .iter()
                .any(|row| row.starts_with("DERIVED\t0\tallow\t"))
        );
        assert!(preview_rows.iter().any(|row| row == "OMITTED\t1\tChange"));
        let call = PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
        )
        .with_op(Op::Read {
            path: CanonPath::new("/workspace/other.txt"),
        });
        let explanation = RuleManagement::explain(&call, &rules, Path::new("/workspace"));
        let rows = render_explanation_rows(&call, &explanation);
        assert_eq!(rows[0], "TOOL\tread");
        assert_eq!(rows[1], "OP\t0\tread\tsrc/main.rs");
        assert_eq!(rows[2], "OP\t1\tread\tother.txt");
        assert_eq!(
            rows[3],
            format!(
                "MATCH\t0\tallow\t{}\tproject\t-",
                rules.allow[0].rule().id()
            )
        );
        assert_eq!(rows[4], "OUTCOME\tuncovered");
        assert_eq!(rows[5], "UNCOVERED\t1");
        assert_eq!(rows[6], "SETTLEMENT\tnot_run");
        assert_eq!(rows[7], "WOULD_EXECUTE\tfalse");
    }

    #[test]
    fn revoke_renderer_projects_one_row_for_dual_effect_atomic_removal() {
        let rule = Rule::load("read(src/**)").unwrap();
        let id = rule.id();
        let receipt = RuleMutationReceipt::from_atomic_facts(
            UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(UserConfigMutationFacts::Revoke { id }),
                    before: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![rule.clone()],
                        deny: vec![rule.clone()],
                    },
                    after: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![],
                        deny: vec![],
                    },
                },
                changed: true,
                publication: UserConfigPublication::PublishedDurably,
            },
            vec![
                RuleMutation {
                    effect: ManagedRuleEffect::Allow,
                    rule: rule.clone(),
                },
                RuleMutation {
                    effect: ManagedRuleEffect::Deny,
                    rule: rule.clone(),
                },
            ],
            vec![
                RuleMutationComparison {
                    stored_changed: true,
                    effective_changed: true,
                    authorities: vec![],
                },
                RuleMutationComparison {
                    stored_changed: true,
                    effective_changed: true,
                    authorities: vec![],
                },
            ],
        )
        .unwrap();
        assert_eq!(
            render_revoke_receipt_rows(&receipt),
            vec![format!(
                "stored_changed\teffective_changed\tboth\t{}\tread(src/**)\tuser\t-\tpublished_durably",
                rule.id()
            )]
        );
    }

    #[derive(Default)]
    struct RecordedText(std::sync::Mutex<Vec<String>>);

    impl Diagnostics for RecordedText {
        fn note(&self, _message: &str) {}
    }

    impl TextOutput for RecordedText {
        fn write(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }

        fn line(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    #[test]
    fn root_text_wrappers_accept_text_output_and_match_pure_rows_byte_for_byte() {
        let rules = Ruleset::from_layer(
            ego_core::config::PermCfg {
                judge: None,
                allow: vec![Rule::load("read(src/**)").unwrap()],
                deny: vec![],
            },
            Source::Project,
        );
        let rows = RuleManagement::rows(&rules);
        let expected = render_list_rows(&rows);
        let output = RecordedText::default();
        render_list_text(&rows, &output);
        assert_eq!(*output.0.lock().unwrap(), expected);

        let call = PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
        );
        let explanation = RuleManagement::explain(&call, &rules, Path::new("/workspace"));
        let expected = render_explanation_rows(&call, &explanation);
        let output = RecordedText::default();
        render_explanation_text(&call, &explanation, &output);
        assert_eq!(*output.0.lock().unwrap(), expected);

        let mutation = RuleMutation {
            effect: ManagedRuleEffect::Allow,
            rule: Rule::load("read(src/**)").unwrap(),
        };
        let receipt = RuleMutationReceipt::from_atomic_facts(
            UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(UserConfigMutationFacts::Batch {
                        changes: vec![mutation.clone()],
                    }),
                    before: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![],
                        deny: vec![],
                    },
                    after: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![mutation.rule.clone()],
                        deny: vec![],
                    },
                },
                changed: true,
                publication: UserConfigPublication::PublishedDurably,
            },
            vec![mutation],
            vec![RuleMutationComparison {
                stored_changed: true,
                effective_changed: true,
                authorities: vec![],
            }],
        )
        .unwrap();
        let expected = render_mutation_receipt_rows(&receipt, ManagedRuleEffect::Allow);
        let output = RecordedText::default();
        render_mutation_receipt(&receipt, "allow", &output);
        assert_eq!(*output.0.lock().unwrap(), expected);

        let preview = RememberPreview {
            effect: ManagedRuleEffect::Allow,
            plan: RuleManagement::remember(
                &call,
                ManagedRuleEffect::Allow,
                Path::new("/workspace"),
            )
            .unwrap(),
        };
        let expected = render_remember_preview_rows(&preview);
        let output = RecordedText::default();
        render_remember_plan(&preview.plan, preview.effect, &output);
        assert_eq!(*output.0.lock().unwrap(), expected);
    }

    #[test]
    fn publication_warning_and_remember_result_rows_are_pure_and_typed() {
        let uncertain = UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: "sync pending".into(),
        };
        assert_eq!(
            publication_warning_text(&uncertain),
            Some(
                "rules changed, but saving them to disk could not be confirmed: sync pending"
                    .to_owned()
            )
        );
        assert_eq!(
            render_remember_result_rows(&UserConfigTarget::Base, None),
            vec!["RESULT\tstored_unchanged\teffective_unchanged\tuser\tnot_published"]
        );
        let receipt = RuleMutationReceipt::from_atomic_facts(
            UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(UserConfigMutationFacts::Batch {
                        changes: vec![RuleMutation {
                            effect: ManagedRuleEffect::Allow,
                            rule: Rule::load("read(src/**)").unwrap(),
                        }],
                    }),
                    before: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![],
                        deny: vec![],
                    },
                    after: RuleSnapshot {
                        target: UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![Rule::load("read(src/**)").unwrap()],
                        deny: vec![],
                    },
                },
                changed: true,
                publication: UserConfigPublication::PublishedDurably,
            },
            vec![RuleMutation {
                effect: ManagedRuleEffect::Allow,
                rule: Rule::load("read(src/**)").unwrap(),
            }],
            vec![RuleMutationComparison {
                stored_changed: true,
                effective_changed: true,
                authorities: vec![],
            }],
        )
        .unwrap();
        assert_eq!(
            render_remember_result_rows(&UserConfigTarget::Base, Some(&receipt)),
            vec!["RESULT\tstored_changed\teffective_changed\tuser\tpublished_durably"]
        );
    }
}
