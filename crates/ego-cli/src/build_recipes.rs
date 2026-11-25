//! The file behind learned build-output recipes (`ego_core::build_recipe`),
//! and `ego build-recipes`, the user's view of it.
//!
//! One JSON book per workspace under `$EGO_HOME/build-recipes/`, named like
//! the workspace memory file, so two workspaces never share recipes. The book
//! is replaced atomically and is readable only by its owner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::build_recipe::{BuildRecipeStore, RecipeBook, RecipeStoreError, read_book};

/// What `ego build-recipes` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum BuildRecipesAction {
    /// List the recipes of this workspace.
    Ls,
    /// Print one recipe as JSON.
    Show {
        /// Recipe ID, as `ego build-recipes ls` prints it.
        id: String,
    },
    /// Stop using one recipe; it stays stored.
    Disable {
        /// Recipe ID, as `ego build-recipes ls` prints it.
        id: String,
    },
    /// Use one disabled recipe again.
    Enable {
        /// Recipe ID, as `ego build-recipes ls` prints it.
        id: String,
    },
    /// Delete one recipe.
    Rm {
        /// Recipe ID, as `ego build-recipes ls` prints it.
        id: String,
    },
}

/// The recipe book of `root` under `home`.
pub(crate) fn file_for(home: &Path, root: &Path) -> PathBuf {
    crate::memory::workspace_file(home, "build-recipes", root, "json")
}

/// The store for this run, or `None` when recipes are off for it:
/// `build_recipes` unset, `--bare`, or no `$EGO_HOME`.
pub(crate) fn store(
    enabled: bool,
    bare: bool,
    home: Option<&Path>,
    root: &Path,
) -> Option<Arc<dyn BuildRecipeStore>> {
    if !enabled || bare {
        return None;
    }
    Some(Arc::new(FileStore {
        path: file_for(home?, root),
    }))
}

/// Run one `ego build-recipes` action for the working directory.
pub fn execute(action: &BuildRecipesAction) -> Result<String, String> {
    let home = crate::config::ego_home().ok_or("no $EGO_HOME, so there are no recipes")?;
    let root = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("the working directory could not be resolved: {error}"))?;
    apply(
        &FileStore {
            path: file_for(&home, &root),
        },
        action,
    )
}

fn apply(store: &dyn BuildRecipeStore, action: &BuildRecipesAction) -> Result<String, String> {
    let mut book = read_book(store).map_err(|error| error.to_string())?;
    let missing = |id: &str| format!("no build recipe {id} in {}", store.location());
    let text = match action {
        BuildRecipesAction::Ls => {
            if book.recipes.is_empty() {
                return Ok(format!("no build recipes ({})", store.location()));
            }
            let mut lines = vec![format!("build recipes ({})", store.location())];
            lines.extend(book.recipes.iter().map(|stored| {
                format!(
                    "{}  r{}  {}  {} -> {} bytes  {}",
                    stored.id,
                    stored.revision,
                    if stored.enabled {
                        "enabled "
                    } else {
                        "disabled"
                    },
                    stored.checked_bytes,
                    stored.projected_bytes,
                    shell_words(&stored.recipe.argv)
                )
            }));
            return Ok(lines.join("\n"));
        }
        BuildRecipesAction::Show { id } => {
            let stored = book.recipes.iter().find(|stored| &stored.id == id);
            let stored = stored.ok_or_else(|| missing(id))?;
            return Ok(serde_json::to_string_pretty(stored).expect("a recipe is serializable"));
        }
        BuildRecipesAction::Disable { id } | BuildRecipesAction::Enable { id } => {
            let enable = matches!(action, BuildRecipesAction::Enable { .. });
            book.find_mut(id).ok_or_else(|| missing(id))?.enabled = enable;
            format!(
                "build recipe {id} {}",
                if enable { "enabled" } else { "disabled" }
            )
        }
        BuildRecipesAction::Rm { id } => {
            let before = book.recipes.len();
            book.recipes.retain(|stored| &stored.id != id);
            if book.recipes.len() == before {
                return Err(missing(id));
            }
            format!("build recipe {id} deleted")
        }
    };
    store.write(&book).map_err(|error| error.to_string())?;
    Ok(text)
}

fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            if !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c))
            {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct FileStore {
    path: PathBuf,
}

impl BuildRecipeStore for FileStore {
    fn location(&self) -> String {
        self.path.display().to_string()
    }

    fn read(&self) -> Result<Option<RecipeBook>, RecipeStoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => RecipeBook::parse(&bytes).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RecipeStoreError(error.to_string())),
        }
    }

    fn write(&self, book: &RecipeBook) -> Result<(), RecipeStoreError> {
        let fail = |error: std::io::Error| RecipeStoreError(error.to_string());
        let dir = self.path.parent().expect("the recipe book has a directory");
        std::fs::create_dir_all(dir).map_err(fail)?;
        let bytes = serde_json::to_vec_pretty(book).expect("a recipe book is serializable");
        // Same directory, so the rename is atomic: a concurrent run reads the
        // old book or the new one, never half of one.
        // A unique staging file created owner-only, so concurrent writers do
        // not share it and the book is never readable by others.
        let mut staged = tempfile::Builder::new()
            .prefix(".build-recipes")
            .tempfile_in(dir)
            .map_err(fail)?;
        std::io::Write::write_all(&mut staged, &bytes).map_err(fail)?;
        staged
            .persist(&self.path)
            .map(drop)
            .map_err(|error| fail(error.error))
    }
}

#[cfg(test)]
mod tests;
