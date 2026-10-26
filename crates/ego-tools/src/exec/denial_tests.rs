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
    let base = std::fs::canonicalize(root.path()).unwrap();
    let workspace = CanonPath::new(&base);
    let controls = (0..8)
        .flat_map(|index| {
            let metadata = base.join(format!("metadata-{index}"));
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
    let path = base.join("missing").display().to_string();
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
        compilations <= 7,
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
    assert_eq!(
        printed_path("cat: /outside/f: Permission denied ignored"),
        None
    );
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

// Catches: Python's quoted file and errno prefix hide an external script read
// from the existing session-grant flow. Diagnostic recorded from Xcode Python
// under real macOS sandbox-exec on 2026-10-26; only the fixture path varies.
#[test]
fn python_script_read_denial_reaches_the_existing_grant_flow() {
    use ego_core::permit::{
        CanonPath, Confinement, RunConfinement, SandboxAccess, StreamProvenance, ToolContentBlock,
        ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = base.join("refused script.py");
    std::fs::write(&script, "print('REGRESSION-MARKER-READ')\n").unwrap();
    let python = "/Applications/Xcode.app/Contents/Developer/usr/bin/python3";
    let text = format!(
        "{python}: can't open file '{}': [Errno 1] Operation not permitted\n",
        script.display()
    );
    let spec = SpawnSpec::new(
        vec![python.into(), script.clone().into_os_string()],
        Some(CanonPath::new(&workspace)),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let policy =
        RunConfinement::new(Confinement::Workspace, false, CanonPath::new(&workspace)).unwrap();
    let result = ToolResult::new(vec![ToolContentBlock {
        text,
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    let denied = super::analyze(&spec, &result, &policy, &[])
        .expect("Python script refusal must reach sandbox grant handling");
    let request = denied.request.as_ref().expect("exact script read request");
    assert_eq!(request.path, CanonPath::new(&script));
    assert_eq!(request.access, SandboxAccess::Read);
    let note = super::note(&denied, &policy, super::Outcome::NobodyToAsk);
    assert!(note.contains("no one was available"), "{note}");
    assert!(note.contains(script.to_str().unwrap()), "{note}");
}

// Catches: Python ENOENT, relative or malformed diagnostics become grants,
// or an actual EACCES diagnostic is missed. EPERM/EACCES/ENOENT forms were
// recorded from Xcode Python on 2026-10-26; the remaining inputs are adversarial.
#[test]
fn python_denial_parser_rejects_non_authority_diagnostics() {
    let prefix = "/Applications/Xcode.app/Contents/Developer/usr/bin/python3: can't open file '";
    for (body, expected) in [
        (
            "/outside/refused script.py': [Errno 13] Permission denied",
            Some("/outside/refused script.py"),
        ),
        (
            "/outside/refused script.py': [Errno 2] No such file or directory",
            None,
        ),
        ("relative.py': [Errno 1] Operation not permitted", None),
        (
            "/outside/refused script.py': [Errno 1] Operation not permitted ignored",
            None,
        ),
        ("/outside/refused script.py': Operation not permitted", None),
    ] {
        let line = format!("{prefix}{body}");
        assert_eq!(printed_path(&line), expected, "{line}");
    }
}

// Catches: the parser drifts from the v3 evaluation set, so a recorded
// refusal line stops yielding its path or a prose/ENOENT/relative line starts
// to yield one.
#[test]
fn the_v3_parser_suite_matches_the_production_parser() {
    let set: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../evals/sandbox-auto/v3/cases.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let cases: Vec<_> = set["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["suite"] == "parser")
        .collect();
    assert_eq!(cases.len(), 7, "the v3 parser suite changed");
    for case in cases {
        let line = case["input"]["line"].as_str().unwrap();
        assert_eq!(
            printed_path(line),
            case["expected"]["path"].as_str(),
            "{}",
            case["id"]
        );
    }
}

// Catches: a new external pnpm store loses human consent when more than its
// final path component is absent. No ancestor may replace the requested path.
#[cfg(target_os = "macos")]
#[test]
fn custom_store_with_missing_parents_still_requests_exact_write_authority() {
    use ego_core::permit::{
        AdaptivePerimeter, CanonPath, Confinement, RunConfinement, SandboxAccess, SandboxGrant,
        StreamProvenance, ToolContentBlock, ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let home = base.join("home");
    let workspace = base.join("workspace");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let policy = RunConfinement::new(Confinement::Adaptive, false, CanonPath::new(&workspace))
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new(&home),
            writable: vec![],
            credentials: vec![CanonPath::new(home.join(".ssh"))],
            controls: vec![],
        });
    let spec = SpawnSpec::new(
        vec!["/opt/homebrew/bin/pnpm".into(), "install".into()],
        Some(CanonPath::new(&workspace)),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let result = ToolResult::new(vec![ToolContentBlock {
        text: "ERR_PNPM_STORE_DIR_OPEN_OPERATION_LOCK: Permission denied".into(),
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    let store = home.join("pnpm-stores/project/store");
    std::fs::write(
        workspace.join(".npmrc"),
        format!("store-dir={}\n", store.display()),
    )
    .unwrap();
    let denial = super::analyze(&spec, &result, &policy, &[]).expect("custom-store consent");
    assert_eq!(
        denial.request,
        Some(SandboxGrant::new(
            CanonPath::new(&store),
            SandboxAccess::Write
        ))
    );
    assert!(
        !home.join("pnpm-stores").exists(),
        "analysis must not create directories"
    );

    // The existing-parent case still names the store, not its writable parent.
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    assert_eq!(
        super::analyze(&spec, &result, &policy, &[]).unwrap(),
        denial
    );
    for path in [
        workspace.join("nested/store"),
        home.clone(),
        home.join(".ssh/new/store"),
    ] {
        std::fs::write(
            workspace.join(".npmrc"),
            format!("store-dir={}\n", path.display()),
        )
        .unwrap();
        assert!(
            super::analyze(&spec, &result, &policy, &[]).is_none(),
            "{}",
            path.display()
        );
        assert!(!path.exists() || path == home);
    }
}

// Catches: reading an external executable script is misreported as an exec
// denial, suppressing the read grant that would actually unblock cat.
#[cfg(unix)]
#[test]
fn reading_executable_script_still_offers_read_authority() {
    use ego_core::permit::{
        CanonPath, Confinement, RunConfinement, SandboxAccess, StreamProvenance, ToolContentBlock,
        ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    use std::os::unix::fs::PermissionsExt as _;
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = base.join("tool.sh");
    std::fs::write(&script, "#!/bin/sh\necho fixture\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let spec = SpawnSpec::new(
        vec!["/bin/cat".into(), script.clone().into_os_string()],
        Some(CanonPath::new(&workspace)),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let policy =
        RunConfinement::new(Confinement::Workspace, false, CanonPath::new(&workspace)).unwrap();
    let result = ToolResult::new(vec![ToolContentBlock {
        text: format!("cat: {}: Operation not permitted", script.display()),
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    let denied = super::analyze(&spec, &result, &policy, &[]).unwrap();
    assert_eq!(
        denied.request.as_ref().map(|request| request.access),
        Some(SandboxAccess::Read),
        "reading a script must not be classified as executing it: {denied:?}"
    );
    let granted = [denied.request.unwrap()];
    let denied = super::analyze(&spec, &result, &policy, &granted).unwrap();
    assert!(
        denied.request.is_none(),
        "a read allowance must not offer an executable write: {denied:?}"
    );
}

fn executable_refusal_fixture(bad_interpreter: bool) {
    use ego_core::permit::{
        CanonPath, Confinement, ProfileRoot, RootAccess, RunConfinement, SandboxAccess,
        SandboxGrant, StreamProvenance, ToolContentBlock, ToolOutputEncoding, ToolOutputProvenance,
        ToolResult,
    };
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    let tools = base.join("tools");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&tools).unwrap();
    let program = tools.join("program");
    std::fs::copy("/bin/echo", &program).unwrap();
    let spec = SpawnSpec::new(
        vec![
            "/bin/bash".into(),
            "-c".into(),
            program.clone().into_os_string(),
        ],
        Some(CanonPath::new(&workspace)),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let policy = RunConfinement::new(Confinement::Workspace, false, CanonPath::new(&workspace))
        .unwrap()
        .with_roots(vec![ProfileRoot::new(
            CanonPath::new(&tools),
            RootAccess::Read,
        )]);
    // Recorded diagnostic forms from usability round two, not an inferred
    // syscall: Bash names the executable or its refused interpreter chain.
    let text = if bad_interpreter {
        format!(
            "/bin/bash: {}: /usr/bin/env: bad interpreter: Operation not permitted",
            program.display()
        )
    } else {
        format!("/bin/bash: {}: Operation not permitted", program.display())
    };
    let result = ToolResult::new(vec![ToolContentBlock {
        text,
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    for grants in [
        vec![],
        vec![SandboxGrant::new(
            CanonPath::new(&program),
            SandboxAccess::Read,
        )],
    ] {
        let denied = super::analyze(&spec, &result, &policy, &grants)
            .expect("executable refusal recognized");
        assert!(
            denied.request.is_none(),
            "filesystem grant offered for executable: {denied:?}"
        );
        let note = super::note(&denied, &policy, super::Outcome::NobodyToAsk);
        for expected in [
            "exec",
            "exec_paths",
            "/quit",
            "ego resume",
            "--mode",
            "--sandbox",
            "--model",
            tools.to_str().unwrap(),
            program.to_str().unwrap(),
        ] {
            assert!(note.contains(expected), "missing {expected:?}: {note}");
        }
        assert!(note.contains("read-only root"), "{note}");
    }
}

// Catches: a shell-launched executable is misclassified as a read, then write grant.
#[test]
fn shell_exec_denial_never_offers_binary_read_or_write() {
    executable_refusal_fixture(false);
}

// Catches: bad-interpreter exit 126 is ignored and supplies no executable recovery.
#[test]
fn refused_interpreter_chain_names_exec_recovery_and_read_root() {
    executable_refusal_fixture(true);
}
