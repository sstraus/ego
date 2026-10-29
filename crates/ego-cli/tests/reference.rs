//! The generated artifacts, bound to the parser that generates them.
//!
//! Reference and completions derive from the same final clap definitions. The tests check
//! that generated artifacts remain synchronized.
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
    fn command_paths(command: &clap::Command, path: &str, paths: &mut Vec<String>) {
        paths.push(path.to_owned());
        for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
            command_paths(sub, &format!("{path} {}", sub.get_name()), paths);
        }
    }
    let headings: Vec<_> = reference
        .lines()
        .filter_map(|line| line.strip_prefix("## `"))
        .filter_map(|line| line.strip_suffix('`'))
        .map(str::to_owned)
        .collect();
    let mut paths = Vec::new();
    command_paths(&root_command(), "ego", &mut paths);
    assert_eq!(headings, paths, "every visible command path has a section");
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

#[test]
fn docs_review_reference_does_not_drop_nested_leaf_arguments() {
    let rendered = reference();
    assert!(rendered.lines().all(|line| line.trim_end() == line));
    for (path, argument) in [
        ("config batch", "--if-revision"),
        ("roots add", "--access"),
        ("mcp login", "--callback-port"),
        ("hooks ls", "--json"),
        ("orchestrator brief", "--root"),
        ("worktree cleanup", "RECEIPT"),
    ] {
        let heading = format!("## `ego {path}`\n");
        let section = rendered
            .split_once(&heading)
            .unwrap_or_else(|| panic!("missing {path}"))
            .1
            .split("\n## ")
            .next()
            .unwrap();
        assert!(section.contains(argument), "{path}: missing {argument}");
        assert!(section.contains("Usage:"), "{path}: missing usage");
    }
}

#[test]
fn docs_review_reference_does_not_erase_argument_constraints() {
    let rendered = reference();
    for metadata in [
        "Default: `2`",
        "Default: `10`",
        "Default: `20`",
        "Default: `read`",
        "Required.",
        "Repeatable.",
        "Values: `read`, `read-write`",
        "Conflicts: `--tools`",
        "default: 32",
        "default: 24h",
        "default: 134217728 bytes",
    ] {
        assert!(rendered.contains(metadata), "missing metadata: {metadata}");
    }
}

#[test]
fn docs_review_local_help_does_not_hide_removal_and_export_controls() {
    let mut root = root_command();
    let rm = root
        .find_subcommand_mut("rm")
        .unwrap()
        .render_long_help()
        .to_string();
    for explanation in [
        "confirmation",
        "uncertain prior run",
        "does not replace",
        "descendants",
        "session ID",
    ] {
        assert!(rm.contains(explanation), "rm missing: {explanation}");
    }
    let export = root
        .find_subcommand_mut("export")
        .unwrap()
        .render_long_help()
        .to_string();
    for explanation in [
        "unsanitized",
        "stdout",
        "different bytes",
        "JSONL",
        "session ID",
    ] {
        assert!(
            export.contains(explanation),
            "export missing: {explanation}"
        );
    }
}
