//! Opt-in learned build-output recipes: compact model answers for one exact command.
//!
//! A recipe is data, never code. It names one exact argv, literal format
//! markers that must appear in the output, and literal line rules that omit
//! noise. It is applied only after a successful exec, to the complete
//! session evidence, and only changes the text the model receives for that
//! call. The raw stream blocks, their evidence references and the exit status
//! stay in the result. The recipe book is user state under `$EGO_HOME`, one
//! file per workspace, behind [`BuildRecipeStore`]; the file adapter lives in
//! `ego-cli`. See `CLI.md` "Build output recipes".

mod extract;
mod learn;

pub use extract::{Fallback, StreamInput, project};
pub use learn::{LearnRequest, descriptor, learn, read_complete};

use serde::{Deserialize, Serialize};

/// The tool name the model sees.
pub const TOOL_NAME: &str = "build_recipe";
/// The only recipe book layout this build reads or writes.
pub const BOOK_VERSION: u32 = 1;
/// Most recipes one workspace keeps.
pub const MAX_RECIPES: usize = 64;
/// Most argv entries a recipe can name.
pub const MAX_ARGV: usize = 64;
/// Most format markers in one recipe.
pub const MAX_MARKERS: usize = 8;
/// Most omit rules in one recipe.
pub const MAX_RULES: usize = 16;
/// Longest marker, rule text or argv entry, in bytes.
pub const MAX_TEXT: usize = 200;

/// Why the recipe book could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeStoreError(pub String);

impl std::fmt::Display for RecipeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one recipe book of one workspace.
pub trait BuildRecipeStore: Send + Sync {
    /// The full path, shown to the user.
    fn location(&self) -> String;
    /// The book, or `None` when no book exists yet.
    fn read(&self) -> Result<Option<RecipeBook>, RecipeStoreError>;
    /// Replace the whole book atomically.
    fn write(&self, book: &RecipeBook) -> Result<(), RecipeStoreError>;
}

/// One literal line rule. No pattern language: the cost of a match is
/// linear in the line and the rule, whatever the model proposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "match", content = "text", rename_all = "snake_case")]
pub enum LineRule {
    Prefix(String),
    Contains(String),
}

impl LineRule {
    fn text(&self) -> &str {
        match self {
            Self::Prefix(text) | Self::Contains(text) => text,
        }
    }

    pub(crate) fn matches(&self, line: &str) -> bool {
        match self {
            Self::Prefix(text) => line.starts_with(text.as_str()),
            Self::Contains(text) => line.contains(text.as_str()),
        }
    }
}

/// The selection a recipe makes. It has no field that names a process,
/// a permission or a rewrite, so it cannot change what exec runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The exact argv the recipe applies to.
    pub argv: Vec<String>,
    /// Literal markers that must all appear in the output.
    pub require: Vec<String>,
    /// Lines to omit from the model answer.
    pub omit: Vec<LineRule>,
}

impl Recipe {
    /// Check every bound before a recipe is used or stored.
    pub fn validate(&self) -> Result<(), String> {
        if self.argv.is_empty() || self.argv.len() > MAX_ARGV {
            return Err(format!("argv needs 1 to {MAX_ARGV} entries"));
        }
        if self.require.is_empty() || self.require.len() > MAX_MARKERS {
            return Err(format!("require needs 1 to {MAX_MARKERS} format markers"));
        }
        if self.omit.is_empty() || self.omit.len() > MAX_RULES {
            return Err(format!("omit needs 1 to {MAX_RULES} rules"));
        }
        for text in self.argv.iter().map(String::as_str) {
            if text.len() > MAX_TEXT || text.contains('\0') {
                return Err(format!(
                    "an argv entry is at most {MAX_TEXT} bytes without NUL"
                ));
            }
        }
        let texts = self
            .require
            .iter()
            .map(String::as_str)
            .chain(self.omit.iter().map(LineRule::text));
        for text in texts {
            if text.trim().is_empty() || text.len() > MAX_TEXT || text.contains(['\n', '\r']) {
                return Err(format!(
                    "a marker or rule is one non-blank line of at most {MAX_TEXT} bytes"
                ));
            }
        }
        Ok(())
    }

    /// The stable identity of the command this recipe serves.
    pub fn id(&self) -> String {
        recipe_id(&self.argv)
    }
}

/// The identity of one exact argv.
pub fn recipe_id(argv: &[String]) -> String {
    let encoded = serde_json::to_vec(argv).expect("argv is serializable");
    crate::digest::sha256_hex(&encoded)[..12].to_owned()
}

/// One stored recipe and its lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRecipe {
    pub id: String,
    /// Increases each time the recipe for this argv is learned again.
    pub revision: u64,
    pub enabled: bool,
    pub recipe: Recipe,
    /// Bytes of the evidence the recipe was checked on, and of its answer.
    pub checked_bytes: u64,
    pub projected_bytes: u64,
}

/// Every recipe of one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeBook {
    pub version: u32,
    pub recipes: Vec<StoredRecipe>,
}

impl Default for RecipeBook {
    fn default() -> Self {
        Self {
            version: BOOK_VERSION,
            recipes: Vec::new(),
        }
    }
}

impl RecipeBook {
    /// Parse a stored book. An unknown version is an error, not an empty
    /// book: a newer layout must not be overwritten by an older ego.
    pub fn parse(bytes: &[u8]) -> Result<Self, RecipeStoreError> {
        let book: Self = serde_json::from_slice(bytes)
            .map_err(|error| RecipeStoreError(format!("the recipe book is not valid: {error}")))?;
        if book.version != BOOK_VERSION {
            return Err(RecipeStoreError(format!(
                "the recipe book has version {}; this ego reads version {BOOK_VERSION}",
                book.version
            )));
        }
        Ok(book)
    }

    /// The enabled, valid recipe for exactly this argv.
    pub fn for_argv(&self, argv: &[String]) -> Option<&StoredRecipe> {
        self.recipes.iter().find(|stored| {
            stored.enabled && stored.recipe.argv == argv && stored.recipe.validate().is_ok()
        })
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut StoredRecipe> {
        self.recipes.iter_mut().find(|stored| stored.id == id)
    }
}

/// Read the book of `store`, treating a missing file as empty.
pub fn read_book(store: &dyn BuildRecipeStore) -> Result<RecipeBook, RecipeStoreError> {
    Ok(store.read()?.unwrap_or_default())
}

#[cfg(test)]
mod tests;
