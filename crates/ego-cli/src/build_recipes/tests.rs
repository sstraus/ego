use super::*;
use ego_core::build_recipe::{BOOK_VERSION, LineRule, Recipe, StoredRecipe};

fn recipe(word: &str) -> StoredRecipe {
    let recipe = Recipe {
        argv: vec!["cargo".into(), word.into()],
        require: vec!["Finished".into()],
        omit: vec![LineRule::Prefix("   Compiling ".into())],
    };
    StoredRecipe {
        id: recipe.id(),
        revision: 1,
        enabled: true,
        recipe,
        checked_bytes: 900,
        projected_bytes: 120,
    }
}

fn book(recipes: Vec<StoredRecipe>) -> RecipeBook {
    RecipeBook {
        version: BOOK_VERSION,
        recipes,
    }
}

#[test]
fn off_gives_no_store_and_workspaces_never_share_a_book() {
    // Catches: recipes off (or --bare) still resolve a store, or two checkouts
    // with the same directory name read each other's recipes.
    let home = tempfile::tempdir().unwrap();
    assert!(store(false, false, Some(home.path()), Path::new("/w")).is_none());
    assert!(store(true, true, Some(home.path()), Path::new("/w")).is_none());
    assert!(store(true, false, None, Path::new("/w")).is_none());
    let one = store(true, false, Some(home.path()), Path::new("/a/app")).unwrap();
    let two = store(true, false, Some(home.path()), Path::new("/b/app")).unwrap();
    one.write(&book(vec![recipe("build")])).unwrap();
    assert_eq!(two.read().unwrap(), None);
    assert_eq!(one.read().unwrap(), Some(book(vec![recipe("build")])));
    assert!(one.location().contains("build-recipes"));
}

#[test]
fn write_replaces_whole_book_privately_and_keeps_newer_layouts() {
    // Catches: a stale temp file stays, the book is world-readable, or an
    // older ego reads a newer book as empty and later overwrites it.
    let home = tempfile::tempdir().unwrap();
    let store = store(true, false, Some(home.path()), Path::new("/w")).unwrap();
    store
        .write(&book(vec![recipe("build"), recipe("test")]))
        .unwrap();
    store.write(&book(vec![recipe("test")])).unwrap();
    assert_eq!(store.read().unwrap(), Some(book(vec![recipe("test")])));
    let dir = home.path().join("build-recipes");
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(entries.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.location())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    std::fs::write(store.location(), r#"{"version":99,"recipes":[]}"#).unwrap();
    assert!(store.read().is_err());
    assert!(apply(store.as_ref(), &BuildRecipesAction::Rm { id: "x".into() }).is_err());
    assert!(
        std::fs::read_to_string(store.location())
            .unwrap()
            .contains("99")
    );
}

#[test]
fn user_lists_shows_disables_enables_and_deletes_recipes() {
    // Catches: disable does not persist, delete leaves the recipe, or an
    // unknown id silently succeeds.
    let home = tempfile::tempdir().unwrap();
    let store = store(true, false, Some(home.path()), Path::new("/w")).unwrap();
    let build = recipe("build");
    let id = build.id.clone();
    store.write(&book(vec![build.clone()])).unwrap();
    let listed = apply(store.as_ref(), &BuildRecipesAction::Ls).unwrap();
    assert!(
        listed.contains(&id) && listed.contains("cargo build"),
        "{listed}"
    );
    let shown = apply(store.as_ref(), &BuildRecipesAction::Show { id: id.clone() }).unwrap();
    assert!(shown.contains("   Compiling "), "{shown}");
    apply(
        store.as_ref(),
        &BuildRecipesAction::Disable { id: id.clone() },
    )
    .unwrap();
    let read = store.read().unwrap().unwrap();
    assert!(read.for_argv(&build.recipe.argv).is_none());
    apply(
        store.as_ref(),
        &BuildRecipesAction::Enable { id: id.clone() },
    )
    .unwrap();
    assert!(
        store
            .read()
            .unwrap()
            .unwrap()
            .for_argv(&build.recipe.argv)
            .is_some()
    );
    apply(store.as_ref(), &BuildRecipesAction::Rm { id: id.clone() }).unwrap();
    assert_eq!(store.read().unwrap(), Some(book(Vec::new())));
    assert!(apply(store.as_ref(), &BuildRecipesAction::Disable { id }).is_err());
}
