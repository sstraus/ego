//! Read-only destination declarations; authorization remains in the permit engine.

use crate::config::{Effective, ManagedRuleEffect, OriginSet, RuleManagement};
use crate::permit::rule::Rule;
use serde::Serialize;

/// The network declarations captured when the session perimeter was admitted.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DestinationPolicy {
    snapshot: &'static str,
    profile: Option<String>,
    rules: Vec<DestinationRule>,
    uncovered: &'static str,
    guarded_http: &'static str,
    dns: &'static str,
    redirects: &'static str,
    private_addresses: &'static str,
    child_processes: &'static str,
    platforms: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct DestinationRule {
    effect: &'static str,
    rule: String,
    source: String,
    also: OriginSet,
}

impl DestinationPolicy {
    pub(super) fn capture(effective: &Effective) -> Self {
        Self {
            snapshot: "startup declarations; use ego rules ls/explain for current rule authority",
            profile: effective
                .active_profile
                .as_ref()
                .map(|profile| profile.name.clone()),
            rules: RuleManagement::rows(&effective.permissions)
                .into_iter()
                .filter(|row| matches!(row.rule, Rule::Net(_)))
                .map(|row| DestinationRule {
                    effect: match row.effect {
                        ManagedRuleEffect::Allow => "allow",
                        ManagedRuleEffect::Deny => "deny",
                    },
                    rule: row.rule.to_string(),
                    source: row.from.to_string(),
                    also: row.also,
                })
                .collect(),
            uncovered: "mode and approval policy decide uncovered destinations; allow rules are not a default-deny firewall; explicit denies take precedence",
            guarded_http: "net rules authorize guarded tool HTTP effects, not all ego HTTP clients or remote server activity; offline remains a separate boundary",
            dns: "each guarded effect binds one fresh DNS result to the logical host and selected socket address; literal IPs skip DNS",
            redirects: "one hop per authorization; a redirect destination needs fresh policy authorization and DNS validation",
            private_addresses: "native public web rejects special-use literal IPs and DNS answers on every hop; authorized foreign HTTP can reach private addresses; net grants do not bypass public web checks",
            child_processes: "net rules do not filter child sockets; offline child isolation requires the backend's measured offline capability; unmeasured capabilities are not a guarantee",
            platforms: "macOS uses Seatbelt; Linux offline needs a supported namespace backend and successful probes; unsupported enforcement fails closed; sandbox off alone provides no network isolation",
        }
    }

    pub(super) fn text(&self) -> Vec<String> {
        let mut lines = vec![format!("  destination_policy\t{}", self.snapshot)];
        if let Some(profile) = &self.profile {
            lines.push(format!("  destination_profile\t{}", escape(profile)));
        }
        if self.rules.is_empty() {
            lines.push("  destination_rules\tnone".into());
        }
        for row in &self.rules {
            let also = row
                .also
                .iter()
                .map(|source| source.to_string())
                .collect::<Vec<_>>();
            lines.push(format!(
                "  destination_{}\t{}\t{}{}",
                row.effect,
                escape(&row.rule),
                row.source,
                if also.is_empty() {
                    String::new()
                } else {
                    format!("; also {}", also.join(", "))
                }
            ));
        }
        for (key, value) in [
            ("uncovered", self.uncovered),
            ("guarded_http", self.guarded_http),
            ("dns", self.dns),
            ("redirects", self.redirects),
            ("private_addresses", self.private_addresses),
            ("child_processes", self.child_processes),
            ("platforms", self.platforms),
        ] {
            lines.push(format!("  {key}\t{value}"));
        }
        lines
    }
}

fn escape(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}
