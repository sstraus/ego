//! Read-only policy diagnostics: no credential read and no provider call.
use super::*;
use ego_ai::jev::ConfiguredDecisions;
use std::time::Duration;

pub(super) struct Jev {
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
}
impl HealthProbe for Jev {
    fn section(&self) -> HealthSection {
        HealthSection::Configuration
    }
    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        let config = match config::jev(self.home.as_deref()) {
            Ok(config) => config,
            Err(_) => {
                return Ok(vec![HealthItem::new(
                    HealthItemId::probe("config.jev"),
                    HealthStatus::Error,
                    HealthDetail::fixed("Jev configuration could not be loaded"),
                    HealthDetail::fixed("system and user config.toml"),
                )]);
            }
        };
        let os_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from));
        let Some(os_home) = os_home else {
            return Ok(vec![]);
        };
        let configured =
            ConfiguredDecisions::new(config.clone(), os_home.clone(), Duration::from_secs(1));
        let mut paths = vec![("current".to_owned(), self.workspace.clone())];
        for (i, entry) in config.roots.iter().enumerate() {
            let raw = entry.path.strip_suffix("/**").unwrap_or(&entry.path);
            let path = if raw == "~" {
                os_home.clone()
            } else if let Some(tail) = raw.strip_prefix("~/") {
                os_home.join(tail)
            } else {
                PathBuf::from(raw)
            };
            paths.push((format!("root[{i}]"), path));
        }
        paths
            .into_iter()
            .map(|(label, path)| match configured.settings(&path) {
                Ok(settings) => fact(
                    HealthItemId::probe("config.jev"),
                    if settings.verified_zdr() {
                        HealthStatus::Ok
                    } else {
                        HealthStatus::Warning
                    },
                    "config.jev",
                    format!("{label}: {}", settings.summary()),
                    "system and user config.toml",
                    roots,
                ),
                Err(_) => Ok(HealthItem::new(
                    HealthItemId::probe("config.jev"),
                    HealthStatus::Error,
                    HealthDetail::fixed("Jev root configuration could not be canonicalized"),
                    HealthDetail::fixed("system and user config.toml"),
                )),
            })
            .collect()
    }
}
