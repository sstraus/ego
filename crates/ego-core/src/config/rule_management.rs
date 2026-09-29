//! Pure policy facts used by persistent rule management.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::permit::rule::{ArgvPattern, Endpoint, PathPattern, Rule, RuleId};
use crate::permit::{Op, OpName, PreparedCall, RuleAuthority};

use super::{ManagedRuleEffect, OriginSet, RuleSnapshot, Ruleset, Source, SourcedRule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRule {
    pub id: RuleId,
    pub effect: ManagedRuleEffect,
    pub rule: Rule,
    pub from: Source,
    pub also: OriginSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOutcome {
    Allowed,
    Denied,
    Uncovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainedOperation {
    pub index: usize,
    pub kind: OpName,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainedAuthority {
    pub operation: usize,
    pub id: RuleId,
    pub effect: ManagedRuleEffect,
    pub from: Source,
    pub also: OriginSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleExplanation {
    pub operations: Vec<ExplainedOperation>,
    pub outcome: RuleOutcome,
    pub authorities: Vec<ExplainedAuthority>,
    pub uncovered: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmittedDimension {
    Change,
    Pattern,
    Query,
    Cwd,
    Timeout,
    Environment,
    RequestDigest,
    ParametersDigest,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberPlan {
    pub candidates: Vec<Rule>,
    pub omitted: Vec<OmittedDimension>,
    candidate_operations: Vec<usize>,
    omitted_operations: Vec<usize>,
}

impl RememberPlan {
    pub fn candidate_rows(&self) -> impl Iterator<Item = (usize, &Rule)> {
        self.candidate_operations
            .iter()
            .copied()
            .zip(self.candidates.iter())
    }

    pub fn omission_rows(&self) -> impl Iterator<Item = (usize, OmittedDimension)> + '_ {
        self.omitted_operations
            .iter()
            .copied()
            .zip(self.omitted.iter().copied())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMutationComparison {
    pub stored_changed: bool,
    pub effective_changed: bool,
    pub authorities: Vec<RuleAuthority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleManagementError {
    OutsideWorkspace { path: PathBuf, workspace: PathBuf },
    NonPersistible(OpName),
}

impl std::fmt::Display for RuleManagementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideWorkspace { path, workspace } => write!(
                formatter,
                "operation path {} is outside workspace {}",
                path.display(),
                workspace.display()
            ),
            Self::NonPersistible(kind) => {
                write!(
                    formatter,
                    "{kind} cannot be represented by a persistent rule"
                )
            }
        }
    }
}

impl std::error::Error for RuleManagementError {}

pub struct RuleManagement;

impl RuleManagement {
    pub fn effective(system: &Ruleset, user: &RuleSnapshot) -> Ruleset {
        effective_rules(system, user)
    }

    pub fn effective_rows(system: &Ruleset, user: &RuleSnapshot) -> Vec<EffectiveRule> {
        let rules = effective_rules(system, user);
        Self::rows(&rules)
    }

    /// Project one already-admitted ruleset without reading another authority.
    pub fn rows(rules: &Ruleset) -> Vec<EffectiveRule> {
        let mut rows = Vec::with_capacity(rules.deny.len() + rules.allow.len());
        rows.extend(
            rules
                .deny
                .iter()
                .map(|entry| effective_row(entry, ManagedRuleEffect::Deny)),
        );
        rows.extend(
            rules
                .allow
                .iter()
                .map(|entry| effective_row(entry, ManagedRuleEffect::Allow)),
        );
        rows.sort_by(|left, right| {
            effect_rank(left.effect)
                .cmp(&effect_rank(right.effect))
                .then_with(|| left.rule.to_string().cmp(&right.rule.to_string()))
                .then_with(|| source_rank(left.from).cmp(&source_rank(right.from)))
        });
        rows
    }

    pub fn compare(
        system: &Ruleset,
        before: &RuleSnapshot,
        after: &RuleSnapshot,
        rule: &Rule,
        effect: ManagedRuleEffect,
    ) -> RuleMutationComparison {
        let stored_changed = before != after;
        let before = Self::effective(system, before);
        let after = Self::effective(system, after);
        Self::compare_resolved(&before, &after, stored_changed, rule, effect)
    }

    pub fn compare_resolved(
        before: &Ruleset,
        after: &Ruleset,
        stored_changed: bool,
        rule: &Rule,
        _effect: ManagedRuleEffect,
    ) -> RuleMutationComparison {
        let before_binding = binding(before, rule);
        let after_binding = binding(after, rule);
        let effective_changed =
            binding_identity(before_binding.as_ref()) != binding_identity(after_binding.as_ref());
        let authorities = if effective_changed {
            Vec::new()
        } else {
            after_binding
                .as_ref()
                .map(rule_authority)
                .into_iter()
                .collect()
        };
        RuleMutationComparison {
            stored_changed,
            effective_changed,
            authorities,
        }
    }

    pub fn explain(call: &PreparedCall, rules: &Ruleset, workspace: &Path) -> RuleExplanation {
        let operations = call
            .ops()
            .iter()
            .enumerate()
            .map(|(index, op)| ExplainedOperation {
                index,
                kind: op.name(),
                subject: operation_subject(op, workspace),
            })
            .collect();

        for (operation, op) in call.ops().iter().enumerate() {
            if let Some(entry) = rules
                .deny
                .iter()
                .find(|entry| entry.rule().matches(op, workspace))
            {
                return RuleExplanation {
                    operations,
                    outcome: RuleOutcome::Denied,
                    authorities: vec![explained_authority(
                        operation,
                        ManagedRuleEffect::Deny,
                        entry,
                    )],
                    uncovered: Vec::new(),
                };
            }
        }

        let mut authorities = Vec::new();
        let mut uncovered = Vec::new();
        for (operation, op) in call.ops().iter().enumerate() {
            match rules
                .allow
                .iter()
                .find(|entry| entry.rule().matches(op, workspace))
            {
                Some(entry) => authorities.push(explained_authority(
                    operation,
                    ManagedRuleEffect::Allow,
                    entry,
                )),
                None => uncovered.push(operation),
            }
        }
        RuleExplanation {
            operations,
            outcome: if uncovered.is_empty() {
                RuleOutcome::Allowed
            } else {
                RuleOutcome::Uncovered
            },
            authorities,
            uncovered,
        }
    }

    pub fn remember(
        call: &PreparedCall,
        effect: ManagedRuleEffect,
        workspace: &Path,
    ) -> Result<RememberPlan, RuleManagementError> {
        let mut candidates = Vec::new();
        let mut candidate_operations = Vec::new();
        let mut omitted = Vec::new();
        let mut omitted_operations = Vec::new();
        for (operation, op) in call.ops().iter().enumerate() {
            let (candidate, dimensions) = Self::remember_op(op, effect, workspace)?;
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
                candidate_operations.push(operation);
            }
            for dimension in dimensions {
                omitted.push(dimension);
                omitted_operations.push(operation);
            }
        }
        Ok(RememberPlan {
            candidates,
            omitted,
            candidate_operations,
            omitted_operations,
        })
    }

    pub fn remember_op(
        op: &Op,
        _effect: ManagedRuleEffect,
        workspace: &Path,
    ) -> Result<(Rule, Vec<OmittedDimension>), RuleManagementError> {
        let path_rule = |path: &Path, make: fn(PathPattern) -> Rule| {
            ensure_workspace(path, workspace)?;
            let text = path
                .to_str()
                .ok_or_else(|| RuleManagementError::NonPersistible(op.name()))?;
            let pattern = PathPattern::parse(text)
                .map_err(|_| RuleManagementError::NonPersistible(op.name()))?;
            if text.contains('*') {
                return Err(RuleManagementError::NonPersistible(op.name()));
            }
            Ok(make(pattern))
        };

        match op {
            Op::Read { path } => Ok((path_rule(path.as_path(), Rule::Read)?, Vec::new())),
            Op::Write { path, .. } => Ok((
                path_rule(path.as_path(), Rule::Write)?,
                vec![OmittedDimension::Change],
            )),
            Op::List { root, .. } => Ok((
                path_rule(root.as_path(), Rule::List)?,
                vec![OmittedDimension::Pattern],
            )),
            Op::Search { root, .. } => Ok((
                path_rule(root.as_path(), Rule::Search)?,
                vec![OmittedDimension::Query],
            )),
            Op::Spawn { spec } => {
                let argv = render_argv(spec.argv())
                    .ok_or_else(|| RuleManagementError::NonPersistible(op.name()))?;
                let pattern = ArgvPattern::parse(&argv)
                    .map_err(|_| RuleManagementError::NonPersistible(op.name()))?;
                Ok((
                    Rule::Exec(pattern),
                    vec![
                        OmittedDimension::Cwd,
                        OmittedDimension::Timeout,
                        OmittedDimension::Environment,
                    ],
                ))
            }
            Op::Connect { target, .. } => Ok((
                Rule::Net(endpoint(target.host(), target.port(), op.name())?),
                vec![OmittedDimension::RequestDigest],
            )),
            Op::ProtocolConnect { target } => Ok((
                Rule::Net(endpoint(target.host(), target.port(), op.name())?),
                Vec::new(),
            )),
            Op::Opaque { tool, .. } => Ok((
                Rule::Tool {
                    server: tool.server().to_owned(),
                    name: tool.name().to_owned(),
                },
                vec![
                    OmittedDimension::ParametersDigest,
                    OmittedDimension::Transport,
                ],
            )),
            Op::ReadEvidence { .. } | Op::SessionProcess { .. } | Op::SecretEgress { .. } => {
                Err(RuleManagementError::NonPersistible(op.name()))
            }
        }
    }
}

fn effective_rules(system: &Ruleset, user: &RuleSnapshot) -> Ruleset {
    if !user.permissions_present {
        return system.clone();
    }

    let mut rules = Ruleset::default();
    for rule in &user.deny {
        Ruleset::merge(&mut rules.deny, rule.clone(), Source::User);
    }
    for rule in &user.allow {
        Ruleset::merge(&mut rules.allow, rule.clone(), Source::User);
    }
    for entry in &system.deny {
        for origin in entry.origins().iter() {
            Ruleset::merge(&mut rules.deny, entry.rule().clone(), origin);
        }
    }
    rules
}

fn effective_row(entry: &SourcedRule, effect: ManagedRuleEffect) -> EffectiveRule {
    let (from, also) = entry.attribution();
    EffectiveRule {
        id: entry.rule().id(),
        effect,
        rule: entry.rule().clone(),
        from,
        also,
    }
}

fn binding(rules: &Ruleset, rule: &Rule) -> Option<EffectiveRule> {
    RuleManagement::rows(rules)
        .into_iter()
        .find(|row| row.rule == *rule)
}

fn binding_identity(row: Option<&EffectiveRule>) -> Option<(ManagedRuleEffect, RuleId, Source)> {
    row.map(|row| (row.effect, row.id.clone(), row.from))
}

fn rule_authority(row: &EffectiveRule) -> RuleAuthority {
    RuleAuthority {
        id: row.id.clone(),
        from: row.from,
        also: row.also,
    }
}

fn explained_authority(
    operation: usize,
    effect: ManagedRuleEffect,
    entry: &SourcedRule,
) -> ExplainedAuthority {
    let authority = RuleAuthority::of(entry);
    ExplainedAuthority {
        operation,
        id: authority.id,
        effect,
        from: authority.from,
        also: authority.also,
    }
}

fn operation_subject(op: &Op, workspace: &Path) -> String {
    let path = match op {
        Op::Read { path } | Op::Write { path, .. } => Some(path.as_path()),
        Op::List { root, .. } | Op::Search { root, .. } => Some(root.as_path()),
        _ => None,
    };
    if let Some(path) = path {
        return match path.strip_prefix(workspace) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => "<outside-workspace>".to_owned(),
        };
    }
    match op {
        Op::Spawn { spec } => spec
            .argv()
            .first()
            .and_then(|value| value.to_str())
            .and_then(|value| Path::new(value).file_name())
            .and_then(OsStr::to_str)
            .unwrap_or("<process>")
            .to_owned(),
        Op::ReadEvidence { .. } => "<session-evidence>".to_owned(),
        Op::SessionProcess { id, .. } => format!("<session-process:{id}>"),
        Op::Opaque { tool, .. } => tool.to_string(),
        Op::Connect { target, .. } | Op::ProtocolConnect { target } => target.to_string(),
        Op::SecretEgress { .. } => "<secret-egress>".to_owned(),
        Op::Read { .. } | Op::Write { .. } | Op::List { .. } | Op::Search { .. } => {
            unreachable!("filesystem subjects returned above")
        }
    }
}

fn render_argv(argv: &[std::ffi::OsString]) -> Option<String> {
    argv.iter()
        .map(|argument| {
            let argument = argument.to_str()?;
            let mut rendered = String::new();
            for character in argument.chars() {
                if character.is_whitespace() || matches!(character, '*' | '\\' | '\'' | '"') {
                    rendered.push('\\');
                }
                rendered.push(character);
            }
            if rendered.is_empty() {
                Some("''".to_owned())
            } else {
                Some(rendered)
            }
        })
        .collect::<Option<Vec<_>>>()
        .map(|arguments| arguments.join(" "))
}

fn endpoint(host: &str, port: u16, kind: OpName) -> Result<Endpoint, RuleManagementError> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Endpoint::parse(&authority).map_err(|_| RuleManagementError::NonPersistible(kind))
}

fn ensure_workspace(path: &Path, workspace: &Path) -> Result<(), RuleManagementError> {
    if path.starts_with(workspace) {
        Ok(())
    } else {
        Err(RuleManagementError::OutsideWorkspace {
            path: path.to_path_buf(),
            workspace: workspace.to_path_buf(),
        })
    }
}

fn effect_rank(effect: ManagedRuleEffect) -> u8 {
    match effect {
        ManagedRuleEffect::Deny => 0,
        ManagedRuleEffect::Allow => 1,
    }
}

fn source_rank(source: Source) -> usize {
    Source::ALL
        .iter()
        .position(|candidate| *candidate == source)
        .expect("every source is in Source::ALL")
}
