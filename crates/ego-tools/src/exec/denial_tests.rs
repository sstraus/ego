use super::{command_line, printed_path};
use ego_core::permit::{EnvPolicy, SpawnSpec};

// Catches: a denial preview recompiles floor patterns for every stderr line.
#[test]
fn adaptive_large_denial_preview_compiles_constant_patterns_once() {
    use ego_core::permit::{
        AdaptivePerimeter, CanonPath, Confinement, RunConfinement, StreamProvenance,
        ToolContentBlock, ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = CanonPath::new(root.path());
    let controls = (0..8)
        .flat_map(|index| {
            let metadata = root.path().join(format!("metadata-{index}"));
            [
                CanonPath::new(metadata.join("config")),
                CanonPath::new(metadata.join("hooks")),
            ]
        })
        .collect();
    let policy = RunConfinement::new(Confinement::Adaptive, false, workspace.clone())
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: workspace.clone(),
            writable: vec![],
            credentials: vec![],
            controls,
        });
    let path = root.path().join("missing").display().to_string();
    let spec = SpawnSpec::new(
        vec!["/bin/cat".into(), path.clone().into()],
        Some(workspace),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let result = ToolResult::new(vec![ToolContentBlock {
        text: format!("cat: {path}: Permission denied\n").repeat(1000),
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    super::super::adaptive::floor::COMPILATIONS.with(|count| count.set(0));
    let denial = super::analyze(&spec, &result, &policy, &[]).unwrap();
    assert_eq!(denial.printed, path);
    assert_eq!(denial.request, None);
    let compilations = super::super::adaptive::floor::COMPILATIONS.with(|count| count.get());
    assert!(
        compilations <= 5,
        "compiled {compilations} patterns for one preview"
    );
}

// Catches: denial analysis consults ambient HOME instead of the admitted home.
#[test]
fn captured_home_cannot_be_offered_as_a_grant() {
    use ego_core::permit::{
        AdaptivePerimeter, CanonPath, Confinement, RunConfinement, StreamProvenance,
        ToolContentBlock, ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    let root = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let home = base.join("captured-home");
    let workspace = base.join("workspace");
    let narrow = home.join("ordinary");
    for path in [&home, &workspace, &narrow] {
        std::fs::create_dir(path).unwrap();
    }
    let policy = RunConfinement::new(Confinement::Adaptive, false, CanonPath::new(&workspace))
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new(&home),
            writable: vec![],
            credentials: vec![],
            controls: vec![],
        });
    for (path, offered) in [
        (&home, false),
        (&base, false),
        (&workspace, false),
        (&narrow, true),
    ] {
        let text = path.display().to_string();
        let spec = SpawnSpec::new(
            vec!["/bin/cat".into(), text.clone().into()],
            Some(CanonPath::new(&workspace)),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let result = ToolResult::new(vec![ToolContentBlock {
            text: format!("cat: {text}: Permission denied"),
            provenance: ToolOutputProvenance::Process {
                stream: StreamProvenance::Stderr,
                encoding: ToolOutputEncoding::Utf8,
                truncated: false,
                omitted_bytes: 0,
            },
            full_output: None,
        }]);
        assert_eq!(
            super::analyze(&spec, &result, &policy, &[])
                .unwrap()
                .request
                .is_some(),
            offered,
            "{text}"
        );
    }
}

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
