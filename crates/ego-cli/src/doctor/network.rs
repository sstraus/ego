//! Provider network reachability, the one part of `ego doctor` that touches the network.
//!
//! Each configured provider gets one unauthenticated probe of its endpoint:
//! DNS, TCP, TLS, then a bare HTTP `GET`. The probe sends no credential, so it
//! never logs in, refreshes a token or makes a paid call. Probes are opt-in
//! `ego doctor --probe-network` or
//! `doctor_network = true` in the user configuration turns them on. The offline
//! posture (`--offline` or `network = "off"`) skips every probe.

use std::path::PathBuf;
use std::time::Duration;

use ego_core::health::{
    HealthDetail, HealthItem, HealthItemId, HealthProbe, HealthSection, HealthStatus,
    RedactionRefusal, VisibleRoots,
};
use ego_tools::http::{ProbeLayer, probe_reachability};
use url::Url;

use super::fact;
use crate::auth::AuthStore;

/// The most one provider's probe may take, all four layers together.
pub(super) const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// Who decides whether this snapshot probes the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Probe {
    /// `--offline`: skip every probe.
    Offline,
    /// `--probe-network`: probe unless the offline posture forbids it.
    Requested,
    /// No flag: probe only when `doctor_network = true`.
    Configured,
}

pub(super) struct Network {
    pub(super) workspace: PathBuf,
    pub(super) home: Option<PathBuf>,
    pub(super) perimeter: Option<crate::perimeter::Perimeter>,
    pub(super) probe: Probe,
}

impl HealthProbe for Network {
    fn section(&self) -> HealthSection {
        HealthSection::Catalogue
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        let enabled = match self.probe {
            Probe::Offline => false,
            Probe::Requested => true,
            Probe::Configured => doctor_network_configured(&self.workspace, self.home.as_deref()),
        };
        if self.probe != Probe::Offline && !enabled {
            return Ok(vec![skipped(
                "provider reachability probes are off: use --probe-network or set doctor_network = true",
            )]);
        }
        let probes = if self.probe == Probe::Offline {
            false
        } else {
            let perimeter = self.perimeter.clone().or_else(|| {
                crate::perimeter::observe(&self.workspace, self.home.as_deref(), None).ok()
            });
            match perimeter {
                Some(perimeter) => !perimeter.offline(),
                None => {
                    return Ok(vec![skipped(
                        "the offline posture could not be resolved, so no provider was probed",
                    )]);
                }
            }
        };
        if !probes {
            return Ok(vec![skipped(
                "offline posture: provider reachability probes are skipped",
            )]);
        }
        let targets =
            match configured_targets(self.home.as_deref(), &|name| std::env::var(name).ok()) {
                Ok(targets) => targets,
                Err(reason) => {
                    return Ok(vec![fact(
                        HealthItemId::probe("network.providers"),
                        HealthStatus::NotChecked,
                        "network.providers",
                        reason,
                        "the provider table",
                        roots,
                    )?]);
                }
            };
        if targets.is_empty() {
            return Ok(vec![skipped(
                "no provider is configured, so there is nothing to probe",
            )]);
        }
        let workers: Vec<_> = targets
            .into_iter()
            .map(|(id, url)| {
                let worker_url = url.clone();
                let handle =
                    std::thread::spawn(move || probe_reachability(&worker_url, PROBE_BUDGET));
                (id, url, handle)
            })
            .collect();
        workers
            .into_iter()
            .map(|(id, url, handle)| {
                let outcome = handle.join().unwrap_or_else(|_| {
                    Err(ego_tools::http::Unreached {
                        layer: ProbeLayer::Http,
                        detail: "the probe stopped without a result".to_owned(),
                    })
                });
                provider_item(&id, &url, outcome, roots)
            })
            .collect()
    }
}

/// `doctor_network` from the user configuration. A repository cannot set it.
fn doctor_network_configured(workspace: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    use ego_core::config::{Trust, resolve};
    crate::config::base_layers_in(workspace, Trust::Untrusted, home)
        .ok()
        .and_then(|layers| resolve(&layers, Trust::Untrusted, None).ok())
        .is_some_and(|effective| effective.doctor_network)
}

fn skipped(reason: &'static str) -> HealthItem {
    HealthItem::new(
        HealthItemId::probe("network.providers"),
        HealthStatus::NotChecked,
        HealthDetail::fixed(reason),
        HealthDetail::fixed("the offline posture and the provider table"),
    )
}

/// `host` or `host:port`, with no user, password, path or query.
fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or("?");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn provider_item(
    id: &str,
    url: &Url,
    outcome: Result<ego_tools::http::Reached, ego_tools::http::Unreached>,
    roots: &VisibleRoots,
) -> Result<HealthItem, RedactionRefusal> {
    let item_id = HealthItemId::about("network.provider", id);
    let source = "an unauthenticated probe of the provider endpoint";
    let host = authority(url);
    match outcome {
        Ok(reached) if reached.status < 500 => fact(
            item_id,
            HealthStatus::Ok,
            "network.provider",
            format!(
                "{host} reachable: DNS, TCP, TLS and HTTP ok (HTTP {})",
                reached.status
            ),
            source,
            roots,
        ),
        Ok(reached) => Ok(fact(
            item_id,
            HealthStatus::Warning,
            "network.provider",
            format!(
                "{host} HTTP failed: the endpoint answered {}",
                reached.status
            ),
            source,
            roots,
        )?
        .remediation(HealthDetail::fixed(
            "retry later; the provider reported a server error",
        ))),
        Err(failure) => Ok(fact(
            item_id,
            HealthStatus::Warning,
            "network.provider",
            format!(
                "{host} {} failed: {}",
                failure.layer.label(),
                failure.detail
            ),
            source,
            roots,
        )?
        .remediation(HealthDetail::fixed(match failure.layer {
            ProbeLayer::Dns => "check DNS and the endpoint host name",
            ProbeLayer::Tcp => "check the network, firewall and endpoint address",
            ProbeLayer::Tls => "check the system trust store and any TLS inspection",
            ProbeLayer::Http => "check the endpoint and retry",
        }))),
    }
}

/// The provider endpoints doctor should probe.
///
/// A provider is configured when it has a stored credential, when the
/// environment variable its row reads is set, or, for the local `ollama` row,
/// when `OLLAMA_HOST` is set. Endpoints come from the same provider table a run
/// uses, so a base-URL override is the endpoint that gets probed.
pub(super) fn configured_targets(
    home: Option<&std::path::Path>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, Url)>, String> {
    let rows = crate::model::rows(&Default::default()).map_err(|error| error.to_string())?;
    let stored = home
        .and_then(|home| AuthStore::new(home).read_observed().ok())
        .unwrap_or_default();
    let set = |name: &str| environment(name).is_some_and(|value| !value.is_empty());
    Ok(rows
        .into_iter()
        .filter(|row| {
            stored.contains_key(row.id())
                || row.credential.environment_var().is_some_and(set)
                || (row.id() == "ollama" && set(crate::model::OLLAMA_HOST))
        })
        .map(|row| (row.id().to_owned(), row.base().clone()))
        .collect())
}

#[cfg(test)]
mod tests;
