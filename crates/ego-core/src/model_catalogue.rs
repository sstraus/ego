//! Read-only model knowledge at the host's model-resolution boundary.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::ModelRef;
use crate::generator::capabilities::ReasoningEffortSupport;
use crate::generator::types::{ThinkingLevel, ToolDescriptor};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub model: ModelRef,
    pub effort: ThinkingLevel,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogueProvider {
    pub id: String,
    pub configured: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogueModel {
    pub id: String,
    pub efforts: ReasoningEffortSupport,
    pub selection_allowed: bool,
    /// Known model facts do not prove current authentication or reachability.
    pub availability: String,
    pub freshness: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelCatalogue {
    pub snapshot: String,
    pub providers: Vec<CatalogueProvider>,
    pub models: Vec<CatalogueModel>,
    pub effort_ceiling: Option<ThinkingLevel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogueRequest {
    provider: Option<String>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveTaskProfile {
    pub id: String,
    pub profile: Option<ExecutionProfile>,
}

pub fn active_foreground_tasks(state: &crate::state::SessionState) -> Vec<ActiveTaskProfile> {
    state
        .items
        .iter()
        .filter(|(_, item)| item.tool_name.as_deref() == Some("task") && item.ok.is_none())
        .filter_map(|(id, item)| {
            let args: serde_json::Value = serde_json::from_str(&item.args).ok()?;
            if args.get("background").and_then(serde_json::Value::as_bool) == Some(true) {
                return None;
            }
            Some(ActiveTaskProfile {
                id: id.to_string(),
                profile: state.task_profiles.get(id).cloned(),
            })
        })
        .collect()
}

impl CatalogueRequest {
    pub(crate) fn render(
        &self,
        catalogue: &ModelCatalogue,
        current: Option<&ExecutionProfile>,
        active: &[ActiveTaskProfile],
    ) -> Result<String, String> {
        let limit = self.limit.unwrap_or(16);
        if !(1..=32).contains(&limit) {
            return Err("model_catalogue limit must be between 1 and 32".into());
        }
        if let Some(provider) = &self.provider
            && !catalogue.providers.iter().any(|row| &row.id == provider)
        {
            return Err(format!("unknown catalogue provider `{provider}`"));
        }
        let models: Vec<_> = catalogue
            .models
            .iter()
            .filter(|row| {
                self.provider.as_ref().is_none_or(|provider| {
                    row.id.split_once('/').is_some_and(|(id, _)| id == provider)
                })
            })
            .collect();
        let end = self.offset.saturating_add(limit).min(models.len());
        let page = models.get(self.offset..end).unwrap_or_default();
        Ok(json!({
            "snapshot": catalogue.snapshot,
            "current": current,
            "active_tasks": active.iter().take(32).collect::<Vec<_>>(),
            "active_tasks_total": active.len(),
            "effort_ceiling": catalogue.effort_ceiling,
            "providers": catalogue.providers.iter().filter(|row| self.provider.as_ref().is_none_or(|id| id == &row.id)).map(|row| {
                let active = current.is_some_and(|profile| profile.model.0.split_once('/').is_some_and(|(id, _)| id == row.id));
                json!({"id":row.id,"configured":if active { Some(true) } else { row.configured }})
            }).collect::<Vec<_>>(),
            "models": page,
            "next_offset": (end < models.len()).then_some(end),
            "discovery": "Known facts only. Use ego models --refresh for explicit live discovery. Availability does not prove reachability."
        }).to_string())
    }
}

pub(crate) fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "model_catalogue".into(),
        description: "List configured provider identities and page known canonical model IDs, supported reasoning efforts and availability. Read-only: no network, authentication, refresh or cache writes. Unknown facts remain unknown; current is the actual resolved execution profile.".into(),
        parameters: json!({"type":"object","additionalProperties":false,"properties":{
            "provider":{"type":"string","description":"Optional canonical provider ID."},
            "offset":{"type":"integer","minimum":0,"default":0},
            "limit":{"type":"integer","minimum":1,"maximum":32,"default":16}
        }}),
    }
}

/// Existing host model/effort authority; catalogue knowledge never widens it.
#[derive(Clone, Debug)]
pub struct ModelSelectionCeiling {
    pub models: std::collections::BTreeSet<String>,
    pub effort: ThinkingLevel,
}

impl ModelSelectionCeiling {
    pub fn validate(
        &self,
        model: Option<&ModelRef>,
        effort: Option<ThinkingLevel>,
    ) -> Result<(), String> {
        if model.is_some_and(|model| !self.models.contains(&model.0)) {
            return Err("requested model is outside this run's admitted model set".into());
        }
        if effort.is_some_and(|effort| effort > self.effort) {
            return Err("requested effort exceeds this run's admitted ceiling".into());
        }
        Ok(())
    }
}

/// Bind host metadata and selection authority to the ordinary model source.
/// This wrapper has no independent provider or credential resolver.
pub fn admit_source(
    owner: std::sync::Arc<dyn crate::generation_model_source::GenerationModelSource>,
    default_model: ModelRef,
    metadata: Option<std::sync::Arc<ModelCatalogue>>,
    ceiling: Option<ModelSelectionCeiling>,
) -> std::sync::Arc<dyn crate::generation_model_source::GenerationModelSource> {
    if metadata.is_none() && ceiling.is_none() {
        return owner;
    }
    let mut catalogue =
        metadata.map_or_else(|| owner.catalogue(), |metadata| Ok((*metadata).clone()));
    if let (Ok(catalogue), Some(ceiling)) = (&mut catalogue, &ceiling) {
        for model in &mut catalogue.models {
            model.selection_allowed &= ceiling.models.contains(&model.id);
        }
        for id in &ceiling.models {
            if let Some((provider, _)) = id.split_once('/')
                && !catalogue.providers.iter().any(|row| row.id == provider)
            {
                catalogue.providers.push(CatalogueProvider {
                    id: provider.into(),
                    configured: None,
                });
            }
            if !catalogue.models.iter().any(|model| &model.id == id) {
                catalogue.models.push(CatalogueModel {
                    id: id.clone(),
                    efforts: ReasoningEffortSupport::Unknown,
                    selection_allowed: true,
                    availability: "unknown".into(),
                    freshness: "unknown".into(),
                    provenance: "host_admission".into(),
                });
            }
        }
        catalogue.models.sort_by(|a, b| a.id.cmp(&b.id));
        catalogue.providers.sort_by(|a, b| a.id.cmp(&b.id));
        catalogue.effort_ceiling = Some(ceiling.effort);
        catalogue.snapshot = crate::digest::sha256_hex(
            &serde_json::to_vec(catalogue).expect("catalogue serializes"),
        );
    }
    std::sync::Arc::new(AdmittedSource {
        owner,
        default_model,
        catalogue,
        ceiling,
    })
}

struct AdmittedSource {
    owner: std::sync::Arc<dyn crate::generation_model_source::GenerationModelSource>,
    default_model: ModelRef,
    catalogue: Result<ModelCatalogue, String>,
    ceiling: Option<ModelSelectionCeiling>,
}
impl crate::generation_model_source::GenerationModelSource for AdmittedSource {
    fn catalogue(&self) -> Result<ModelCatalogue, String> {
        self.catalogue.clone()
    }
    fn resolve(
        &self,
        request: crate::generation_model_source::GenerationModelRequest,
    ) -> futures::future::BoxFuture<
        '_,
        Result<
            crate::generation_model_source::GenerationProfile,
            crate::generation_model_source::GenerationModelError,
        >,
    > {
        use crate::generation_model_source::{
            GenerationModelError, GenerationModelErrorClass, GenerationRole,
        };
        Box::pin(async move {
            let primary = !matches!(
                request.role(),
                GenerationRole::Judge | GenerationRole::GoalCheck
            );
            if primary && let Some(ceiling) = &self.ceiling {
                ceiling
                    .validate(
                        Some(request.model().unwrap_or(&self.default_model)),
                        request.thinking(),
                    )
                    .map_err(|message| {
                        GenerationModelError::new(GenerationModelErrorClass::Usage, message)
                    })?;
            }
            let profile = self.owner.resolve(request).await?;
            if primary && let Some(ceiling) = &self.ceiling {
                ceiling
                    .validate(Some(&profile.identity), Some(profile.thinking))
                    .map_err(|message| {
                        GenerationModelError::new(GenerationModelErrorClass::Usage, message)
                    })?;
            }
            Ok(profile)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation_model_source::{
        GenerationModelError, GenerationModelErrorClass, GenerationModelRequest,
        GenerationModelSource, GenerationProfile, GenerationRole,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct InventorySource {
        reads: AtomicUsize,
        resolutions: AtomicUsize,
    }
    impl GenerationModelSource for InventorySource {
        fn catalogue(&self) -> Result<ModelCatalogue, String> {
            let revision = self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(ModelCatalogue {
                snapshot: format!("revision-{revision}"),
                providers: vec![CatalogueProvider {
                    id: "host".into(),
                    configured: Some(true),
                }],
                models: vec![CatalogueModel {
                    id: "host/known".into(),
                    efforts: ReasoningEffortSupport::Unknown,
                    selection_allowed: true,
                    availability: "unknown".into(),
                    freshness: "unknown".into(),
                    provenance: "test".into(),
                }],
                effort_ceiling: None,
            })
        }
        fn resolve(
            &self,
            _: GenerationModelRequest,
        ) -> futures::future::BoxFuture<'_, Result<GenerationProfile, GenerationModelError>>
        {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(GenerationModelError::new(
                    GenerationModelErrorClass::Provider,
                    "owner reached",
                ))
            })
        }
    }

    // Catches: lazy catalogue capture, metadata widening selection, and omitted
    // default selections bypassing the same ceiling as explicit task overrides.
    #[test]
    fn admitted_model_catalogue_freezes_inventory_and_rejects_selection_before_resolution() {
        let owner = Arc::new(InventorySource {
            reads: AtomicUsize::new(0),
            resolutions: AtomicUsize::new(0),
        });
        let source = admit_source(
            owner.clone(),
            ModelRef("host/known".into()),
            None,
            Some(ModelSelectionCeiling {
                models: ["host/allowed".into()].into(),
                effort: ThinkingLevel::Low,
            }),
        );
        assert_eq!(
            owner.reads.load(Ordering::SeqCst),
            1,
            "capture occurs at admission"
        );
        let first = source.catalogue().unwrap();
        owner.catalogue().unwrap(); // Later host inventory changes cannot affect this run.
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(source.catalogue().unwrap()).unwrap()
        );
        assert!(
            !first
                .models
                .iter()
                .find(|model| model.id == "host/known")
                .unwrap()
                .selection_allowed
        );
        assert!(
            first
                .models
                .iter()
                .find(|model| model.id == "host/allowed")
                .unwrap()
                .selection_allowed
        );
        assert_eq!(first.effort_ceiling, Some(ThinkingLevel::Low));
        for request in [
            GenerationModelRequest::new(GenerationRole::Subagent),
            GenerationModelRequest::new(GenerationRole::Subagent)
                .with_model(ModelRef("host/known".into())),
            GenerationModelRequest::new(GenerationRole::Subagent)
                .with_model(ModelRef("host/allowed".into()))
                .with_thinking(ThinkingLevel::High),
        ] {
            let error = futures::executor::block_on(source.resolve(request))
                .err()
                .unwrap();
            assert_eq!(error.class, GenerationModelErrorClass::Usage);
        }
        assert_eq!(owner.resolutions.load(Ordering::SeqCst), 0);
        let error = futures::executor::block_on(
            source.resolve(
                GenerationModelRequest::new(GenerationRole::Subagent)
                    .with_model(ModelRef("host/allowed".into()))
                    .with_thinking(ThinkingLevel::Low),
            ),
        )
        .err()
        .unwrap();
        assert_eq!(error.message, "owner reached");
        assert_eq!(owner.resolutions.load(Ordering::SeqCst), 1);
    }
}
