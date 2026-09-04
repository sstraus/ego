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
use ego_core::permit::{CanonPath, PathResolver, PrepareCtx, PreparedCall, ToolError};
use ego_core::run::RunTermination;
use ego_tools::config::OsUserConfigStore;
use ego_tools::fs::OsFs;
use ego_tools::store::OsIo;

use crate::cli::{RulesAction, RulesArgs};
use crate::config::{escape_field, rule_management, user_target};
use crate::output::{self, Stdout};

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
                out.line(&format!(
                    "RESULT\tstored_unchanged\teffective_unchanged\t{}\tnot_published",
                    escape_field(&user_target(target))
                ));
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
            let publication = publication(&receipt.mutation().publication);
            out.line(&format!(
                "RESULT\t{}\t{}\t{}\t{}",
                stored_state(receipt.mutation().changed),
                effective_state(receipt.effective_changed()),
                escape_field(&user_target(target)),
                publication
            ));
            publication_warning(&receipt.mutation().publication);
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

fn render_mutation_receipt(receipt: &RuleMutationReceipt, effect: &str, out: &Stdout) {
    let affected = receipt.affected().first();
    out.line(&format!(
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
    ));
    publication_warning(&receipt.mutation().publication);
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
    out.line(&format!("TOOL\t{}", escape_field(call.tool())));
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
