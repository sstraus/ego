//! Trusted Jev configuration and pure canonical-root policy selection.
use serde::Deserialize;
use std::path::{Path, PathBuf};
use url::Url;

/// Approved OpenRouter Decisions endpoint for every root.
pub const JEV_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
/// Separate calibrated uses of the shared client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevUse {
    Wake,
    Destructive,
    Router,
}
/// Fail behavior for a use; destructive never accepts Open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JevFail {
    Open,
    Closed,
    CurrentJudge,
    FixedModel,
}

mod value;
pub use value::{JevAuth, Probability};

/// An explicit per-use root override; omitted fields inherit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevUseOverride {
    pub on: Option<bool>,
    #[serde(default, deserialize_with = "low")]
    pub low: Option<Probability>,
    #[serde(default, deserialize_with = "high")]
    pub high: Option<Probability>,
    pub fail: Option<JevFail>,
}
/// An explicit root override, not an eligibility list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevRoot {
    pub path: String,
    pub endpoint: Option<String>,
    #[serde(default, deserialize_with = "wake")]
    pub wake: Option<JevUseOverride>,
    #[serde(default, deserialize_with = "destructive")]
    pub destructive: Option<JevUseOverride>,
    #[serde(default, deserialize_with = "router")]
    pub router: Option<JevUseOverride>,
}
/// One complete system/user Jev section. Higher layers replace this section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    pub endpoint: String,
    pub auth: JevAuth,
    pub model: String,
    pub roots: Vec<JevRoot>,
}
impl Default for JevConfig {
    fn default() -> Self {
        Self {
            endpoint: JEV_ENDPOINT.into(),
            auth: JevAuth::default(),
            model: "~typesafe/jev-latest".into(),
            roots: vec![],
        }
    }
}
/// The effective per-use policy before the independent calibration gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevPolicy {
    pub on: bool,
    pub low: Probability,
    pub high: Probability,
    pub fail: JevFail,
}
/// Root-specific settings, without credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevSettings {
    pub endpoint: String,
    pub auth: JevAuth,
    pub model: String,
    policies: [JevPolicy; 3],
}
impl JevSettings {
    pub fn policy(&self, usage: JevUse) -> JevPolicy {
        self.policies[index(usage)]
    }
    pub fn verified_zdr(&self) -> bool {
        self.endpoint == JEV_ENDPOINT
    }
    /// No auth values or state enter doctor output.
    pub fn summary(&self) -> String {
        format!(
            "endpoint={} ZDR={} wake={} destructive={} router={} (each use requires calibration)",
            self.endpoint,
            if self.verified_zdr() {
                "verified"
            } else {
                "unverified-ZDR"
            },
            self.policy(JevUse::Wake).on,
            self.policy(JevUse::Destructive).on,
            self.policy(JevUse::Router).on
        )
    }
}
fn index(usage: JevUse) -> usize {
    match usage {
        JevUse::Wake => 0,
        JevUse::Destructive => 1,
        JevUse::Router => 2,
    }
}
impl JevConfig {
    pub fn default_policy(&self, usage: JevUse) -> JevPolicy {
        let (low, high, fail) = match usage {
            JevUse::Wake => (0.3, 0.7, JevFail::Open),
            JevUse::Destructive => (0.2, 0.8, JevFail::Closed),
            JevUse::Router => (0.2, 0.8, JevFail::FixedModel),
        };
        JevPolicy {
            on: true,
            low: Probability(low),
            high: Probability(high),
            fail,
        }
    }
    /// Validate without filesystem or credential reads.
    pub fn validate(&self) -> Result<(), String> {
        validate_endpoint(&self.endpoint)?;
        if self.model.trim().is_empty() {
            return Err("jev.model must not be empty".into());
        }
        match &self.auth {
            JevAuth::ApiKey { env } => {
                if env.parse::<super::McpEnvironmentName>().is_err() {
                    return Err("jev.auth.env is invalid".into());
                }
            }
            JevAuth::Header { name, value } => {
                if super::is_forbidden_auth_header(name)
                    || name.eq_ignore_ascii_case("authorization")
                    || name.is_empty()
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
                {
                    return Err("jev.auth.header is invalid".into());
                }
            }
        }
        for root in &self.roots {
            if root.path.trim().is_empty() {
                return Err("jev.roots.path must not be empty".into());
            }
            if let Some(endpoint) = &root.endpoint {
                validate_endpoint(endpoint)?;
            }
            for (usage, overrides) in [
                (JevUse::Wake, &root.wake),
                (JevUse::Destructive, &root.destructive),
                (JevUse::Router, &root.router),
            ] {
                if let Some(overrides) = overrides {
                    apply(self.default_policy(usage), overrides, usage)?;
                }
            }
        }
        Ok(())
    }
    /// Inputs are actual canonical OS paths in the same order as roots.
    pub fn select(
        &self,
        root: &Path,
        canonical_entries: &[PathBuf],
    ) -> Result<JevSettings, String> {
        self.validate()?;
        if canonical_entries.len() != self.roots.len()
            || !root.is_absolute()
            || canonical_entries.iter().any(|p| !p.is_absolute())
        {
            return Err("jev.roots requires canonical absolute paths".into());
        }
        let mut matched: Vec<_> = canonical_entries
            .iter()
            .enumerate()
            .filter(|(_, p)| root.starts_with(p))
            .map(|(i, p)| (p.components().count(), i))
            .collect();
        matched.sort_by_key(|(depth, _)| *depth);
        let mut selected = JevSettings {
            endpoint: self.endpoint.clone(),
            auth: self.auth.clone(),
            model: self.model.clone(),
            policies: [
                self.default_policy(JevUse::Wake),
                self.default_policy(JevUse::Destructive),
                self.default_policy(JevUse::Router),
            ],
        };
        for (_, i) in matched {
            let entry = &self.roots[i];
            if let Some(endpoint) = &entry.endpoint {
                selected.endpoint = endpoint.clone();
            }
            for (usage, overrides) in [
                (JevUse::Wake, &entry.wake),
                (JevUse::Destructive, &entry.destructive),
                (JevUse::Router, &entry.router),
            ] {
                if let Some(overrides) = overrides {
                    selected.policies[index(usage)] =
                        apply(selected.policy(usage), overrides, usage)?;
                }
            }
        }
        Ok(selected)
    }
}
fn apply(
    mut policy: JevPolicy,
    overrides: &JevUseOverride,
    usage: JevUse,
) -> Result<JevPolicy, String> {
    if let Some(v) = overrides.on {
        policy.on = v;
    }
    if let Some(v) = overrides.low {
        policy.low = v;
    }
    if let Some(v) = overrides.high {
        policy.high = v;
    }
    if let Some(v) = overrides.fail {
        policy.fail = v;
    }
    let name = match usage {
        JevUse::Wake => "wake",
        JevUse::Destructive => "destructive",
        JevUse::Router => "router",
    };
    if policy.low.0 >= policy.high.0 {
        return Err(format!("jev.roots.{name}.low must be less than high"));
    }
    let valid = match usage {
        JevUse::Wake => policy.fail == JevFail::Open,
        JevUse::Destructive => matches!(policy.fail, JevFail::Closed | JevFail::CurrentJudge),
        JevUse::Router => policy.fail == JevFail::FixedModel,
    };
    if !valid {
        return Err(format!("jev.roots.{name}.fail is invalid"));
    }
    Ok(policy)
}
fn validate_endpoint(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|_| "jev.endpoint is invalid")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("jev.endpoint must be an HTTP URL without credentials or fragment".into());
    }
    Ok(())
}

fn low<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Probability>, D::Error> {
    Option::<Probability>::deserialize(d).map_err(|e| serde::de::Error::custom(format!("low: {e}")))
}
fn high<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Probability>, D::Error> {
    Option::<Probability>::deserialize(d)
        .map_err(|e| serde::de::Error::custom(format!("high: {e}")))
}
fn wake<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<JevUseOverride>, D::Error> {
    Option::<JevUseOverride>::deserialize(d)
        .map_err(|e| serde::de::Error::custom(format!("jev.roots.wake: {e}")))
}
fn destructive<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<JevUseOverride>, D::Error> {
    Option::<JevUseOverride>::deserialize(d)
        .map_err(|e| serde::de::Error::custom(format!("jev.roots.destructive: {e}")))
}
fn router<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<JevUseOverride>, D::Error> {
    Option::<JevUseOverride>::deserialize(d)
        .map_err(|e| serde::de::Error::custom(format!("jev.roots.router: {e}")))
}
