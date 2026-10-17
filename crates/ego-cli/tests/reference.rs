//! The generated artifacts, bound to the parser that generates them.
//!
//! `CLI.md` §4: "CLI reference and completions are generated from the same
//! final clap definitions and snapshot-tested… hand-maintained parallel
//! command lists are not release evidence." Two things have to hold for that
//! sentence to mean anything, and one of them is easy to forget.
//!
//! The easy one is the snapshot: the file on disk equals what the generator
//! produces now. The one that is easy to forget is the *inventory* — the
//! generator's own command tree is a hand-written list too, and if it drifts
//! from [`BUILT`] then the reference and the completions agree perfectly about
//! a surface ego does not have.

use std::fs;
use std::path::{Path, PathBuf};

use ego_cli::cli::{BUILT, root_command};
use ego_cli::reference::{COMPLETIONS, REFERENCE_PATH, completion, reference};

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

/// Compare the checked-in artifact with the generated one, or write it.
///
/// `EGO_UPDATE_ARTIFACTS=1` regenerates instead of asserting. An env var and
/// not a separate binary: a `--bin` that exists only to write two files is a
/// compilation unit in every build, and the thing that has to stay true here
/// is that the artifacts and the parser were produced in the same run.
///
/// The failure message names the command, because "left != right" over a
/// three-hundred-line shell script is not something anyone can act on.
fn snapshot(path: &Path, generated: &str) {
    if std::env::var_os("EGO_UPDATE_ARTIFACTS").is_some() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the artifact directory is creatable");
        }
        fs::write(path, generated).expect("the artifact is writable");
        return;
    }
    let checked_in = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "{} is missing ({error}): regenerate with \
             `EGO_UPDATE_ARTIFACTS=1 cargo test -p ego-cli --test reference`",
            path.display()
        )
    });
    assert_eq!(
        checked_in,
        generated,
        "{} is stale: regenerate it with \
         `EGO_UPDATE_ARTIFACTS=1 cargo test -p ego-cli --test reference`",
        path.display(),
    );
}

#[test]
fn the_generated_tree_offers_exactly_the_commands_this_build_offers() {
    let root = root_command();
    let mut generated: Vec<&str> = root
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();
    let mut built: Vec<&str> = BUILT.to_vec();
    generated.sort_unstable();
    built.sort_unstable();

    // Both directions. A command in `BUILT` and not in the tree is missing
    // from the reference and uncompletable; a command in the tree and not in
    // `BUILT` is a documented invocation that refuses by name.
    assert_eq!(
        generated, built,
        "the generated command tree and the parser inventory disagree",
    );
}

#[test]
fn the_reference_is_the_one_the_current_parser_produces() {
    snapshot(&repository().join(REFERENCE_PATH), &reference());
}

#[test]
fn every_supported_shell_has_the_completions_this_parser_produces() {
    let repository = repository();
    for (shell, path) in COMPLETIONS {
        snapshot(&repository.join(path), &completion(*shell));
    }
}

#[test]
fn a_completion_script_names_every_command_the_operator_can_type() {
    for (shell, _) in COMPLETIONS {
        let script = completion(*shell);
        for command in BUILT {
            assert!(
                script.contains(command),
                "the {shell} completions never mention `ego {command}`",
            );
        }
    }
}

#[test]
fn the_reference_documents_every_command_and_no_others() {
    let reference = reference();
    let headings: Vec<String> = reference
        .lines()
        .filter_map(|line| line.strip_prefix("## `ego "))
        .filter_map(|line| line.strip_suffix("`"))
        .map(str::to_owned)
        .collect();

    assert_eq!(
        headings,
        BUILT
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        "the reference's sections are the parser's commands, in the parser's order",
    );
}

#[test]
fn docs_review_root_completion_does_not_omit_interactive_options() {
    for (shell, _) in COMPLETIONS {
        let script = completion(*shell);
        for option in ["model", "mode", "version"] {
            assert!(script.contains(option), "{shell}: missing root {option}");
        }
    }
    let mut root = root_command();
    root.build();
    for option in ["model", "mode", "directory", "version"] {
        assert!(
            root.get_arguments().any(|arg| arg.get_id() == option),
            "missing root {option}"
        );
    }
}
