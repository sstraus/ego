use super::*;
use crate::permit::StreamProvenance;
use crate::store::evidence::{EvidenceId, EvidenceRef};

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn stored(omit: Vec<LineRule>) -> StoredRecipe {
    let recipe = Recipe {
        argv: argv(&["cargo", "build"]),
        require: vec!["Finished".into()],
        omit,
    };
    StoredRecipe {
        id: recipe.id(),
        revision: 2,
        enabled: true,
        recipe,
        checked_bytes: 0,
        projected_bytes: 0,
    }
}

fn reference(byte: char, bytes: usize) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceId::parse(byte.to_string().repeat(64)).unwrap(),
        bytes as u64,
    )
}

fn run(stored: &StoredRecipe, stdout: &[u8], stderr: &[u8]) -> Result<String, Fallback> {
    let (out, err) = (reference('a', stdout.len()), reference('b', stderr.len()));
    project(
        stored,
        &[
            StreamInput {
                stream: StreamProvenance::Stdout,
                bytes: stdout,
                reference: &out,
            },
            StreamInput {
                stream: StreamProvenance::Stderr,
                bytes: stderr,
                reference: &err,
            },
        ],
    )
}

#[test]
fn projection_omits_only_matching_noise_and_keeps_warnings_with_the_same_prefix() {
    // Catches: a rule for progress lines also hides an actionable warning or
    // an unexpected line, or the answer loses the evidence references.
    let recipe = stored(vec![LineRule::Prefix("   Compiling ".into())]);
    let progress: String = (0..40).map(|i| format!("   Compiling c{i} v1\n")).collect();
    let stderr =
        format!("{progress}   Compiling c v1 warning: unused\nnote: odd line\n    Finished dev\n");
    let text = run(&recipe, b"bin ready\n", stderr.as_bytes()).unwrap();
    assert!(!text.contains("Compiling c0 v1"), "{text}");
    assert!(!text.contains("Compiling c39 v1"), "{text}");
    assert!(text.contains("Compiling c v1 warning: unused"), "{text}");
    assert!(text.contains("note: odd line"), "{text}");
    assert!(text.contains("Finished dev"), "{text}");
    assert!(text.contains("bin ready"), "{text}");
    assert!(text.contains("omitted 40 of 44 lines"), "{text}");
    assert!(
        text.contains(&format!("read_tool_result {}", reference('a', 10))),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "read_tool_result {}",
            reference('b', stderr.len())
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("{} revision 2", recipe.id)),
        "{text}"
    );
}

#[test]
fn unknown_format_binary_no_reduction_and_oversize_fall_back() {
    // Catches: a recipe is applied to output of a different format, to bytes
    // it cannot read as text, or produces an answer no smaller or unbounded.
    let recipe = stored(vec![LineRule::Contains("Compiling".into())]);
    assert_eq!(
        run(&recipe, b"Compiling x\n", b"other tool\n"),
        Err(Fallback::UnknownFormat {
            marker: "Finished".into()
        })
    );
    assert_eq!(
        run(&recipe, b"Finished\n\xff\n", b""),
        Err(Fallback::NotText)
    );
    assert_eq!(
        run(&recipe, b"Finished\nnothing to omit\n", b""),
        Err(Fallback::NoReduction)
    );
    // One omitted line still makes an answer longer than the output.
    assert_eq!(
        run(&recipe, b"Compiling a\nFinished\n", b""),
        Err(Fallback::NoReduction)
    );
    let kept = "kept line that no rule matches\n".repeat(400);
    assert_eq!(
        run(
            &recipe,
            format!("Compiling a\nFinished\n{kept}").as_bytes(),
            b""
        ),
        Err(Fallback::AnswerTooLarge)
    );
    let huge = vec![b'x'; extract::MAX_INPUT_BYTES as usize + 1];
    assert_eq!(run(&recipe, &huge, b""), Err(Fallback::InputTooLarge));
}

#[test]
fn recipe_bounds_refuse_pathological_proposals() {
    // Catches: an empty marker matches every output, a multi-line or huge
    // rule escapes the line model, or an unbounded rule list is stored.
    let good = stored(vec![LineRule::Prefix("x".into())]).recipe;
    assert!(good.validate().is_ok());
    let mut cases = Vec::new();
    for change in 0..6 {
        let mut recipe = good.clone();
        match change {
            0 => recipe.require = vec![" ".into()],
            1 => recipe.require.clear(),
            2 => recipe.omit = vec![LineRule::Contains("a\nb".into())],
            3 => recipe.omit = vec![LineRule::Prefix("x".into()); MAX_RULES + 1],
            4 => recipe.omit = vec![LineRule::Prefix("y".repeat(MAX_TEXT + 1))],
            _ => recipe.argv.clear(),
        }
        cases.push(recipe.validate());
    }
    assert!(cases.iter().all(Result::is_err), "{cases:?}");
}

#[test]
fn identity_and_lookup_bind_one_exact_enabled_argv() {
    // Catches: two commands that differ in an output-affecting flag share a
    // recipe, or a disabled recipe is still applied.
    let release = Recipe {
        argv: argv(&["cargo", "build", "--release"]),
        ..stored(vec![LineRule::Prefix("x".into())]).recipe
    };
    let debug = stored(vec![LineRule::Prefix("x".into())]);
    assert_ne!(release.id(), debug.id);
    let mut book = RecipeBook {
        version: BOOK_VERSION,
        recipes: vec![debug.clone()],
    };
    assert!(book.for_argv(&release.argv).is_none());
    assert_eq!(book.for_argv(&debug.recipe.argv), Some(&debug));
    book.find_mut(&debug.id).unwrap().enabled = false;
    assert!(book.for_argv(&debug.recipe.argv).is_none());
}

#[test]
fn book_round_trips_and_refuses_unknown_versions_and_fields() {
    // Catches: a newer layout is read as an empty book and overwritten, or a
    // stored recipe gains an unchecked field such as a command rewrite.
    let book = RecipeBook {
        version: BOOK_VERSION,
        recipes: vec![stored(vec![LineRule::Contains("Compiling".into())])],
    };
    let bytes = serde_json::to_vec(&book).unwrap();
    assert_eq!(RecipeBook::parse(&bytes).unwrap(), book);
    let mut newer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    newer["version"] = 2.into();
    assert!(RecipeBook::parse(newer.to_string().as_bytes()).is_err());
    let mut rewrite: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    rewrite["recipes"][0]["recipe"]["run"] = serde_json::json!(["sh", "-c", "x"]);
    assert!(RecipeBook::parse(rewrite.to_string().as_bytes()).is_err());
}

#[test]
fn lines_naming_warn_fatal_exception_or_deprecation_survive_a_broad_rule() {
    // Catches: a progress rule hides a `WARN`, `fatal`, `exception` or
    // `deprecated` line, none of which spell "warning" or "error".
    let recipe = stored(vec![LineRule::Prefix("npm ".into())]);
    let noise: String = (0..40)
        .map(|i| format!("npm http fetch GET 200 p{i}\n"))
        .collect();
    let out = format!(
        "{noise}npm WARN deprecated left-pad@1.0.0\nnpm fatal: x\nnpm Exception in y\nnpm notice z is deprecated\nFinished\n"
    );
    let text = run(&recipe, out.as_bytes(), b"").unwrap();
    for line in [
        "npm WARN deprecated",
        "npm fatal",
        "npm Exception",
        "is deprecated",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!text.contains("fetch GET 200 p0"), "{text}");
}
