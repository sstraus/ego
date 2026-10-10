use super::{command_line, printed_path};
use ego_core::permit::{EnvPolicy, SpawnSpec};

// Lines recorded from macOS `cat`, `ls` and `touch` under `sandbox-exec`
// with a profile that does not open the path, 2026-10-02.
#[test]
fn recorded_macos_tool_lines_yield_their_path() {
    assert_eq!(
        printed_path("cat: /Users/u/Gits/.tmp/x//outside.txt: Operation not permitted"),
        Some("/Users/u/Gits/.tmp/x//outside.txt")
    );
    assert_eq!(
        printed_path("touch: /Users/u/new: Operation not permitted"),
        Some("/Users/u/new")
    );
    assert_eq!(
        printed_path("/bin/sh: /Users/u/o2: Operation not permitted"),
        Some("/Users/u/o2")
    );
}

#[test]
fn a_refused_program_names_its_path() {
    assert_eq!(
        printed_path(
            "sandbox-exec: execvp() of '/opt/homebrew/Cellar/p/3.14/bin/python3.14' failed: Operation not permitted"
        ),
        Some("/opt/homebrew/Cellar/p/3.14/bin/python3.14")
    );
    assert_eq!(
        printed_path("sandbox-exec: execvp() of 'prog' failed: Operation not permitted"),
        None
    );
}

#[test]
fn other_errors_and_relative_paths_are_not_sandbox_questions() {
    assert_eq!(
        printed_path("cat: /Users/u/missing: No such file or directory"),
        None
    );
    assert_eq!(
        printed_path("cat: notes.txt: Operation not permitted"),
        None
    );
    assert_eq!(printed_path("Operation not permitted"), None);
}

#[test]
fn the_command_line_quotes_what_a_person_could_misread() {
    let spec = SpawnSpec::new(
        ["/bin/sh", "-c", "echo hi there", ""]
            .map(Into::into)
            .to_vec(),
        None,
        None,
        EnvPolicy::Inherited,
    )
    .expect("spawn spec");
    assert_eq!(command_line(&spec), "/bin/sh -c 'echo hi there' ''");
}
