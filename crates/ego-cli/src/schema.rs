//! Validate schema-constrained output and re-prompt on failure. Each re-prompt is a durable
//! turn and counts toward turn and budget limits. Exhausted schema attempts are task failure,
//! not a command-line usage error.

use jsonschema::Validator;
use serde_json::Value;

use crate::cli::UsageError;

/// A compiled `--schema`, and the text it was compiled from.
pub struct Schema {
    validator: Validator,
}

/// Why an answer did not satisfy the schema.
///
/// Carries the reasons so the re-prompt can quote them. A model told only
/// "that was invalid" is being asked to guess, and guessing is what the
/// retries are being spent on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub reasons: Vec<String>,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reasons.join("; "))
    }
}

impl Schema {
    /// Compile a schema, already resolved from `@file` by `prompt::at_file`.
    pub fn compile(text: &str) -> Result<Self, UsageError> {
        let document: Value = serde_json::from_str(text)
            .map_err(|err| UsageError(format!("--schema is not JSON: {err}")))?;
        let validator = jsonschema::validator_for(&document)
            .map_err(|err| UsageError(format!("--schema is not a JSON Schema: {err}")))?;
        Ok(Schema { validator })
    }

    /// Check one answer.
    ///
    /// The answer must be JSON *and* valid. A model that replied in prose has
    /// not produced a document that fails validation — it has produced no
    /// document — and saying so is more useful than a parser error about a
    /// character at offset 0.
    pub fn check(&self, answer: &str) -> Result<Value, Invalid> {
        let document: Value = serde_json::from_str(answer.trim()).map_err(|err| Invalid {
            reasons: vec![format!("the answer is not JSON: {err}")],
        })?;
        let reasons: Vec<String> = self
            .validator
            .iter_errors(&document)
            .map(|err| format!("{}: {err}", err.instance_path()))
            .collect();
        if reasons.is_empty() {
            Ok(document)
        } else {
            Err(Invalid { reasons })
        }
    }
}

/// What to ask next when the answer did not validate.
///
/// A turn of its own, quoting what was wrong. It does not re-send the original
/// prompt: the conversation already holds it, and repeating it would make the
/// model answer the question twice.
pub fn repair_prompt(invalid: &Invalid) -> String {
    format!(
        "That answer did not satisfy the required schema: {invalid}.\n\
         Reply with the corrected JSON document and nothing else — no prose, \
         no code fence."
    )
}
