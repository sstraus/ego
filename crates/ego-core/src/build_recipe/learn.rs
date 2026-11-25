//! The model-facing `build_recipe` tool: propose one recipe, checked on evidence.
//!
//! The tool only writes the workspace recipe book. It runs no process and
//! reads only evidence this session already owns, so it cannot widen what
//! exec may run or reach. A recipe is stored only when it applies to the
//! named evidence, which proves the markers exist and the rules omit lines.

use serde::Deserialize;

use super::extract::{MAX_INPUT_BYTES, StreamInput, project};
use super::{BuildRecipeStore, LineRule, MAX_RECIPES, Recipe, StoredRecipe, TOOL_NAME, read_book};
use crate::permit::StreamProvenance;
use crate::store::evidence::{EvidenceRead, EvidenceRef, EvidenceStore};

/// One proposal from the model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnRequest {
    pub argv: Vec<String>,
    pub require: Vec<String>,
    pub omit: Vec<LineRule>,
    #[serde(default)]
    pub stdout: Option<EvidenceRef>,
    #[serde(default)]
    pub stderr: Option<EvidenceRef>,
}

impl LearnRequest {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let request: Self = serde_json::from_value(value)
            .map_err(|error| format!("{TOOL_NAME} was called with invalid arguments: {error}"))?;
        request.recipe().validate()?;
        if request.stdout.is_none() && request.stderr.is_none() {
            return Err("name the stdout or stderr evidence the recipe was learned from".into());
        }
        Ok(request)
    }

    fn recipe(&self) -> Recipe {
        Recipe {
            argv: self.argv.clone(),
            require: self.require.clone(),
            omit: self.omit.clone(),
        }
    }

    fn streams(&self) -> impl Iterator<Item = (StreamProvenance, &EvidenceRef)> {
        [
            (StreamProvenance::Stdout, self.stdout.as_ref()),
            (StreamProvenance::Stderr, self.stderr.as_ref()),
        ]
        .into_iter()
        .filter_map(|(stream, reference)| reference.map(|reference| (stream, reference)))
    }
}

/// Read one complete evidence object, refusing objects over the input bound.
pub async fn read_complete(
    evidence: &dyn EvidenceStore,
    reference: &EvidenceRef,
) -> Result<Vec<u8>, String> {
    if reference.bytes > MAX_INPUT_BYTES {
        return Err(format!(
            "evidence {reference} exceeds {MAX_INPUT_BYTES} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(reference.bytes as usize);
    let mut offset = 0;
    loop {
        let request = EvidenceRead::new(reference.clone(), offset).map_err(|e| e.to_string())?;
        let page = evidence
            .read_page(&request)
            .await
            .map_err(|e| e.to_string())?;
        bytes.extend_from_slice(&page.bytes);
        match page.next_offset {
            Some(next) if next > offset => offset = next,
            Some(_) => return Err(format!("evidence {reference} did not advance")),
            None => return Ok(bytes),
        }
    }
}

/// Check `request` on its evidence and store it as the next revision.
pub async fn learn(
    store: &dyn BuildRecipeStore,
    evidence: &dyn EvidenceStore,
    request: &LearnRequest,
) -> Result<String, String> {
    let recipe = request.recipe();
    recipe.validate()?;
    let total: u64 = request
        .streams()
        .map(|(_, reference)| reference.bytes)
        .sum();
    if total > MAX_INPUT_BYTES {
        return Err(format!("the evidence exceeds {MAX_INPUT_BYTES} bytes"));
    }
    let mut owned = Vec::new();
    for (stream, reference) in request.streams() {
        owned.push((stream, reference, read_complete(evidence, reference).await?));
    }
    let mut book =
        read_book(store).map_err(|error| format!("recipes could not be read: {error}"))?;
    let id = recipe.id();
    let previous = book.recipes.iter().position(|stored| stored.id == id);
    let candidate = StoredRecipe {
        id: id.clone(),
        revision: previous.map_or(1, |index| book.recipes[index].revision + 1),
        // A new revision keeps the owner's choice: the model cannot re-enable
        // a recipe the owner disabled.
        enabled: previous.is_none_or(|index| book.recipes[index].enabled),
        recipe,
        checked_bytes: total,
        projected_bytes: 0,
    };
    let streams: Vec<_> = owned
        .iter()
        .map(|(stream, reference, bytes)| StreamInput {
            stream: *stream,
            bytes,
            reference,
        })
        .collect();
    let answer = project(&candidate, &streams)
        .map_err(|fallback| format!("the recipe does not apply to this evidence: {fallback}"))?;
    let stored = StoredRecipe {
        projected_bytes: answer.len() as u64,
        ..candidate
    };
    let summary = format!(
        "learned build recipe {} revision {}: on this evidence the answer is {} of {} bytes. It applies only to later successful runs of exactly this argv whose output has every marker; any other run keeps the complete answer.",
        stored.id, stored.revision, stored.projected_bytes, stored.checked_bytes
    );
    match previous {
        Some(index) => book.recipes[index] = stored,
        None if book.recipes.len() >= MAX_RECIPES => {
            return Err(format!(
                "this workspace already has {MAX_RECIPES} recipes; delete one with `ego build-recipes delete ID`"
            ));
        }
        None => book.recipes.push(stored),
    }
    store
        .write(&book)
        .map_err(|error| format!("recipes could not be written: {error}"))?;
    Ok(summary)
}

pub fn descriptor() -> crate::generator::types::ToolDescriptor {
    let rule = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["match", "text"],
        "properties": {
            "match": {"type": "string", "enum": ["prefix", "contains"]},
            "text": {"type": "string", "minLength": 1, "maxLength": super::MAX_TEXT}
        }
    });
    crate::generator::types::ToolDescriptor {
        name: TOOL_NAME.into(),
        description: format!(
            "Save a recipe that shortens your view of the output of one exact exec argv that repeats in this workspace, such as a build or test command. Name the stdout and/or stderr full-output references of a successful run of that argv. require lists literal text that every output of this command contains; omit lists literal line rules for noise such as progress lines. Lines with error, warn, fail, fatal, panic, exception or deprecat are never omitted, and every other line stays; the answer must be smaller than the output. The recipe is checked on the evidence you name; later successful runs of exactly this argv then give you the kept lines plus full-output references. It never changes what runs. At most {} markers and {} rules.",
            super::MAX_MARKERS,
            super::MAX_RULES
        ),
        parameters: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["argv", "require", "omit"],
            "properties": {
                "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": super::MAX_ARGV},
                "require": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": super::MAX_TEXT}, "minItems": 1, "maxItems": super::MAX_MARKERS},
                "omit": {"type": "array", "items": rule, "minItems": 1, "maxItems": super::MAX_RULES},
                "stdout": {"type": "string", "description": "Full-output reference of the stdout stream, exactly as a prior exec result printed it."},
                "stderr": {"type": "string", "description": "Full-output reference of the stderr stream, exactly as a prior exec result printed it."}
            }
        }),
    }
}
