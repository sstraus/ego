//! ID-addressed notebook edits. No file is read before its permit exists.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};

use ego_core::permit::{
    Change, GuardedIo, NotebookCellEdit, NotebookCellType, Op, PrepareCtx, PreparedCall, ToolError,
    ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{resolve, wrong_op};

const MAX_NOTEBOOK_BYTES: usize = 16 * 1024 * 1024;
// One synchronous read/modify/write transaction across concurrent notebook calls
// in this process. External editors do not participate in this lock.
static EDIT_LOCK: Mutex<()> = Mutex::new(());
static SCHEMA: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(include_str!("nbformat.v4.schema.json"))
        .expect("recorded Jupyter schema is JSON");
    jsonschema::validator_for(&schema).expect("recorded Jupyter schema is valid")
});

#[derive(Debug, Deserialize)]
pub struct NotebookEditParams {
    /// Existing nbformat 4.5 notebook, absolute or relative to the working directory.
    pub path: String,
    #[serde(flatten)]
    pub edit: NotebookCellEdit,
}

// Keep the provider-facing object flat. Serde still validates NotebookCellEdit
// before preparation, including variant-required fields and unknown fields.
impl JsonSchema for NotebookEditParams {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NotebookEditParams".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "edit_mode"],
            "properties": {
                "path": { "type": "string", "description": "Existing nbformat 4.5 notebook, absolute or relative to the working directory." },
                "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"] },
                "cell_id": { "type": ["string", "null"], "description": "Required string for replace and delete. For insert, identifies the preceding cell; null or omitted inserts at the start." },
                "new_source": { "type": "string", "description": "Required for replace and insert; omit for delete." },
                "cell_type": { "type": "string", "enum": ["code", "markdown"], "description": "Required for insert; omit for replace and delete." }
            }
        })
    }
}

pub struct NotebookEditTool;

impl TypedTool for NotebookEditTool {
    type Params = NotebookEditParams;

    fn name(&self) -> &str {
        "notebook_edit"
    }

    fn description(&self) -> &str {
        "Edit one Jupyter nbformat 4.5 notebook cell by stable cell_id. Replace changes only \
         source, delete removes the cell, insert adds a code or markdown cell after cell_id \
         (null inserts at the start). Other cells, metadata and outputs are retained. Does not \
         execute code. Notebooks are limited to 16 MiB. Do not edit the file concurrently from \
         another process. Use write(path) permission rules as for edit."
    }

    fn prepare_typed(
        &self,
        params: NotebookEditParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let (id, source) = match &params.edit {
            NotebookCellEdit::Replace {
                cell_id,
                new_source,
            } => (Some(cell_id), Some(new_source)),
            NotebookCellEdit::Insert {
                cell_id,
                new_source,
                ..
            } => (cell_id.as_ref(), Some(new_source)),
            NotebookCellEdit::Delete { cell_id } => (Some(cell_id), None),
        };
        if id.is_some_and(|id| !valid_id(id)) {
            return Err(invalid(
                "cell_id must contain 1-64 ASCII letters, digits, hyphens or underscores",
            ));
        }
        if source.is_some_and(|source| source.len() > MAX_NOTEBOOK_BYTES) {
            return Err(invalid("new_source exceeds 16 MiB"));
        }
        let path = resolve(ctx, &params.path)?;
        Ok(
            PreparedCall::new(self.name(), Op::Read { path: path.clone() }).with_op(Op::Write {
                path,
                change: Change::Notebook(params.edit),
            }),
        )
    }

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let (_, ops) = call.into_parts();
            let mut ops = ops.into_iter();
            let (
                Some(Op::Read { path: read }),
                Some(Op::Write {
                    path,
                    change: Change::Notebook(edit),
                }),
                None,
            ) = (ops.next(), ops.next(), ops.next())
            else {
                return Err(wrong_op(self.name()));
            };
            if read != path {
                return Err(wrong_op(self.name()));
            }
            let _transaction = EDIT_LOCK
                .lock()
                .map_err(|_| invalid("notebook edit lock is poisoned"))?;
            let before = io.read_bounded(&path, MAX_NOTEBOOK_BYTES)?;
            let (after, id) = apply(&before, &edit)?;
            // Detect a non-cooperating editor observed before publication. Like
            // other path-based filesystem operations, this is not an OS CAS.
            if io.read_bounded(&path, MAX_NOTEBOOK_BYTES)? != before {
                return Err(invalid(
                    "notebook changed during this edit; read it again before retrying",
                ));
            }
            io.write(&path, &after)?;
            Ok(
                ToolResult::builtin(format!("edited cell {id} in {path}; code was not executed"))
                    .with_checkpoint(io.last_checkpoint(&path)),
            )
        })
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidParams(message.into())
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn validate(notebook: &Value) -> Result<(), ToolError> {
    if notebook.get("nbformat").and_then(Value::as_u64) != Some(4)
        || notebook.get("nbformat_minor").and_then(Value::as_u64) != Some(5)
    {
        return Err(invalid(
            "notebook_edit requires nbformat 4.5 with stable cell IDs",
        ));
    }
    if !SCHEMA.is_valid(notebook) {
        return Err(invalid(
            "notebook does not match the Jupyter nbformat 4.5 schema",
        ));
    }
    let mut ids = BTreeSet::new();
    for cell in notebook["cells"]
        .as_array()
        .expect("schema validated cells")
    {
        let id = cell["id"].as_str().expect("schema validated ID");
        if !valid_id(id) || !ids.insert(id) {
            return Err(invalid("notebook cell IDs must be valid and unique"));
        }
    }
    Ok(())
}

// Run only after JSON syntax validation. Inspect original tokens, not Value's
// numbers: out-of-range integers have already fallen back to lossy f64 there.
fn reject_lossy_integers(text: &str) -> Result<(), ToolError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                let number = &text[start..i];
                if !number.contains(['.', 'e', 'E'])
                    && number.parse::<i64>().is_err()
                    && number.parse::<u64>().is_err()
                {
                    return Err(invalid(
                        "notebook contains an integer outside i64/u64 range; edit refused to avoid rounding saved data",
                    ));
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

fn apply(before: &[u8], edit: &NotebookCellEdit) -> Result<(Vec<u8>, String), ToolError> {
    let text = std::str::from_utf8(before).map_err(|_| invalid("notebook is not UTF-8"))?;
    let mut notebook: Value =
        serde_json::from_str(text).map_err(|_| invalid("notebook is not valid JSON"))?;
    reject_lossy_integers(text)?;
    validate(&notebook)?;
    let cells = notebook["cells"]
        .as_array_mut()
        .expect("schema validated cells");
    let target = match edit {
        NotebookCellEdit::Replace { cell_id, .. } | NotebookCellEdit::Delete { cell_id } => {
            Some(cell_id)
        }
        NotebookCellEdit::Insert { cell_id, .. } => cell_id.as_ref(),
    };
    let index = match target {
        Some(id) => Some(
            cells
                .iter()
                .position(|cell| cell["id"].as_str() == Some(id))
                .ok_or_else(|| invalid("cell_id does not exist in the notebook"))?,
        ),
        None => None,
    };
    let id = match edit {
        NotebookCellEdit::Replace {
            cell_id,
            new_source,
        } => {
            cells[index.expect("replace has a target")]["source"] = json!(new_source);
            cell_id.clone()
        }
        NotebookCellEdit::Delete { cell_id } => {
            cells.remove(index.expect("delete has a target"));
            cell_id.clone()
        }
        NotebookCellEdit::Insert {
            cell_type,
            new_source,
            ..
        } => {
            let id = new_id(cells)?;
            let cell = match cell_type {
                NotebookCellType::Code => {
                    json!({"id": id, "cell_type": "code", "metadata": {}, "source": new_source, "execution_count": null, "outputs": []})
                }
                NotebookCellType::Markdown => {
                    json!({"id": id, "cell_type": "markdown", "metadata": {}, "source": new_source})
                }
            };
            cells.insert(index.map_or(0, |i| i + 1), cell);
            id
        }
    };
    validate(&notebook)?;
    let mut after = serde_json::to_vec_pretty(&notebook)
        .map_err(|_| invalid("notebook serialization failed"))?;
    after.push(b'\n');
    if after.len() > MAX_NOTEBOOK_BYTES {
        return Err(invalid("edited notebook exceeds 16 MiB"));
    }
    Ok((after, id))
}

fn new_id(cells: &[Value]) -> Result<String, ToolError> {
    for _ in 0..16 {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| invalid("could not generate a cell ID"))?;
        let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        if !cells.iter().any(|cell| cell["id"].as_str() == Some(&id)) {
            return Ok(id);
        }
    }
    Err(invalid("could not generate a unique cell ID"))
}
