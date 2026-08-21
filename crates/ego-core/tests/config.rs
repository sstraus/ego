//! Strict config schema, precedence resolution and the trust gate.
//!
//! See `plans/m0a-contracts.md` Step 12 and
//! `ARCHITECTURE.md`.

use ego_core::config::{
    ConfigError, ConfigLayer, Mode, ModelRef, PermCfg, Posture, Source, Sourced, Trust, resolve,
};

fn layer(source: Source, value: ConfigLayer) -> Sourced<ConfigLayer> {
    Sourced { source, value }
}

fn model(name: &str) -> Option<ModelRef> {
    Some(ModelRef(name.to_string()))
}

#[test]
fn config_unknown_top_level_key_names_key_and_source() {
    let err = ConfigLayer::parse(Source::Project, "bogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::Project,
        }
    );
    let message = err.to_string();
    assert!(
        message.contains("bogus"),
        "message should name the key: {message}"
    );
    assert!(
        message.contains("project"),
        "message should name the source: {message}"
    );
}

#[test]
fn config_unknown_key_in_nested_permissions_table_is_named() {
    let err = ConfigLayer::parse(Source::User, "[permissions]\nbogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::User,
        }
    );
}

#[test]
fn config_unknown_key_in_profile_table_is_named() {
    let err = ConfigLayer::parse(Source::System, "[profile.ci]\nbogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::System,
        }
    );
}

#[test]
fn config_known_keys_parse_cleanly() {
    let sourced = ConfigLayer::parse(
        Source::Flags,
        "model = \"anthropic/claude-sonnet-5\"\nmode = \"edits\"\n\n[permissions]\nallow = [\"bash(cargo *)\"]\ndeny = []\n",
    )
    .unwrap();
    assert_eq!(sourced.source, Source::Flags);
    assert_eq!(sourced.value.model, model("anthropic/claude-sonnet-5"));
    assert_eq!(sourced.value.mode, Some(Mode::Edits));
    assert_eq!(
        sourced.value.permissions,
        Some(PermCfg {
            allow: vec!["bash(cargo *)".to_string()],
            deny: vec![],
        })
    );
}

#[test]
fn config_full_precedence_order_flags_beats_env_beats_local_beats_project_beats_user_beats_system()
{
    let all = [
        layer(
            Source::System,
            ConfigLayer {
                model: model("system"),
                ..Default::default()
            },
        ),
        layer(
            Source::User,
            ConfigLayer {
                model: model("user"),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                ..Default::default()
            },
        ),
        layer(
            Source::Local,
            ConfigLayer {
                model: model("local"),
                ..Default::default()
            },
        ),
        layer(
            Source::Env,
            ConfigLayer {
                model: model("env"),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                model: model("flags"),
                ..Default::default()
            },
        ),
    ];

    assert_eq!(
        resolve(&all, Trust::Trusted, None).unwrap().model,
        model("flags")
    );
    assert_eq!(
        resolve(&all[..5], Trust::Trusted, None).unwrap().model,
        model("env")
    );
    assert_eq!(
        resolve(&all[..4], Trust::Trusted, None).unwrap().model,
        model("local")
    );
    assert_eq!(
        resolve(&all[..3], Trust::Trusted, None).unwrap().model,
        model("project")
    );
    assert_eq!(
        resolve(&all[..2], Trust::Trusted, None).unwrap().model,
        model("user")
    );
    assert_eq!(
        resolve(&all[..1], Trust::Trusted, None).unwrap().model,
        model("system")
    );
}

#[test]
fn config_precedence_is_independent_of_slice_order() {
    let scrambled = [
        layer(
            Source::Flags,
            ConfigLayer {
                model: model("flags"),
                ..Default::default()
            },
        ),
        layer(
            Source::System,
            ConfigLayer {
                model: model("system"),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                ..Default::default()
            },
        ),
    ];
    assert_eq!(
        resolve(&scrambled, Trust::Trusted, None).unwrap().model,
        model("flags")
    );
}

#[test]
fn config_system_deny_ceiling_cannot_be_relaxed_by_flags() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec!["bash(rm -rf *)".to_string()],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec!["bash(rm -rf *)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    // And the flags layer's allow is still *there*. Resolution's job is to
    // guarantee the system deny survives the merge, not to decide which of the
    // two wins: that is decided per call, against a canonical `PreparedCall`,
    // by the engine.
    //
    // This assertion used to read `!allow.contains(...)`, satisfied by an
    // `allow.retain(|e| !deny.contains(e))` at the end of `resolve`. It passed
    // for a reason that does not generalise — the two strings happened to be
    // equal. Change the allow to `bash(rm -rf /tmp/x)` and the retain kept it,
    // while the deny it violates sat right beside it in the same `Effective`.
    // See `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6.
    assert!(
        effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_non_system_deny_can_be_relaxed_by_higher_precedence() {
    let layers = [
        layer(
            Source::Project,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec!["bash(curl *)".to_string()],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec!["bash(curl *)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .allow
            .contains(&"bash(curl *)".to_string())
    );
    assert!(
        !effective
            .permissions
            .deny
            .contains(&"bash(curl *)".to_string())
    );
}

#[test]
fn config_profile_overrides_apply_only_when_active_and_only_within_their_own_layer() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let user = layer(
        Source::User,
        ConfigLayer {
            mode: Some(Mode::Auto),
            ..Default::default()
        },
    );

    // No active profile: project's base mode wins over user (project > user).
    let effective = resolve(&[project.clone(), user.clone()], Trust::Trusted, None).unwrap();
    assert_eq!(effective.mode, Some(Mode::Default));

    // Active profile "ci": project's profile override wins.
    let effective = resolve(&[project.clone(), user.clone()], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Yolo));

    // A higher-precedence layer without the profile still wins over the
    // lower layer's profile override.
    let flags = layer(
        Source::Flags,
        ConfigLayer {
            mode: Some(Mode::Plan),
            ..Default::default()
        },
    );
    let effective = resolve(&[project, user, flags], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Plan));
}

#[test]
fn config_unknown_profile_name_is_a_hard_error() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let err = resolve(&[project], Trust::Trusted, Some("nope")).unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownProfile {
            name: "nope".to_string(),
        }
    );
}

#[test]
fn config_profile_defined_by_only_one_eligible_layer_still_applies() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let user = layer(
        Source::User,
        ConfigLayer {
            mode: Some(Mode::Auto),
            ..Default::default()
        },
    );
    // Only `project` defines "ci" — that's enough for the profile to be
    // valid, and it still applies within the layer that defines it.
    let effective = resolve(&[project, user], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Yolo));
}

#[test]
fn config_system_deny_ceiling_survives_profile_override_that_clears_deny() {
    let toml = "[permissions]\ndeny = [\"bash(rm -rf *)\"]\n\n[profile.ci.permissions]\nallow = [\"bash(rm -rf *)\"]\ndeny = []\n";
    let system = ConfigLayer::parse(Source::System, toml).unwrap();
    let effective = resolve(&[system], Trust::Trusted, Some("ci")).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    // Carried, not subtracted — see
    // `config_system_deny_ceiling_cannot_be_relaxed_by_flags`.
    assert!(
        effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_system_deny_ceiling_unions_across_multiple_system_layers() {
    let system_a = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec![],
                deny: vec!["bash(rm -rf *)".to_string()],
            }),
            ..Default::default()
        },
    );
    let system_b = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec!["bash(rm -rf *)".to_string()],
                deny: vec!["bash(curl *)".to_string()],
            }),
            ..Default::default()
        },
    );
    let effective = resolve(&[system_a, system_b], Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(curl *)".to_string())
    );
    // Carried, not subtracted — see
    // `config_system_deny_ceiling_cannot_be_relaxed_by_flags`.
    assert!(
        effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_untrusted_project_and_local_cannot_affect_resolution() {
    let layers = [
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                mode: Some(Mode::Yolo),
                permissions: Some(PermCfg {
                    allow: vec!["bash(*)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Local,
            ConfigLayer {
                model: model("local"),
                mode: Some(Mode::Yolo),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
    assert_eq!(effective.model, None);
    assert_eq!(effective.mode, None);
    assert!(effective.permissions.allow.is_empty());
}

#[test]
fn config_untrusted_project_profile_is_not_eligible() {
    let toml = "[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let err = resolve(&[project], Trust::Untrusted, Some("ci")).unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownProfile {
            name: "ci".to_string(),
        }
    );
}

#[test]
fn config_bare_forced_excludes_project_and_local_even_when_trusted() {
    let layers = [
        layer(
            Source::Flags,
            ConfigLayer {
                bare: Some(true),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                mode: Some(Mode::Yolo),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
    assert_eq!(effective.mode, None);
}

#[test]
fn config_unknown_key_containing_backtick_is_named_correctly() {
    let err = ConfigLayer::parse(Source::User, "\"bad`key\" = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bad`key".to_string(),
            source: Source::User,
        }
    );
}

#[test]
fn config_untrusted_workspace_resolves_bare_regardless_of_config() {
    let layers = [layer(Source::Project, ConfigLayer::default())];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
}

#[test]
fn config_trusted_workspace_allows_ambient() {
    let layers = [layer(Source::Project, ConfigLayer::default())];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Ambient);
}

#[test]
fn config_bare_flag_forces_bare_even_when_trusted() {
    let layers = [layer(
        Source::Flags,
        ConfigLayer {
            bare: Some(true),
            ..Default::default()
        },
    )];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
}

// -- Instruction discovery (AGENTS.md) ----------------------------------
//
// The pure half: which files, in what order, and — the part that matters —
// that an untrusted workspace's files are never *touched*. See
// `docs/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`.

mod instructions {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use ego_core::config::{
        Found, Instruction, InstructionIo, InstructionSet, Limits, Posture, Refusal, Selection,
        SystemPrompt, load, plan, system_prompt,
    };
    use ego_core::public::InstructionSource;

    /// An `InstructionIo` that answers from a table and counts every call.
    ///
    /// The count is the point: "an untrusted workspace loads nothing" is a
    /// weaker claim than "an untrusted workspace is never looked at", and only
    /// the second one keeps a permission dialog off the screen.
    struct Fake {
        files: Vec<(PathBuf, Found)>,
        calls: RefCell<Vec<PathBuf>>,
    }

    impl Fake {
        fn new(files: Vec<(&str, Found)>) -> Self {
            Fake {
                files: files
                    .into_iter()
                    .map(|(p, f)| (PathBuf::from(p), f))
                    .collect(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<PathBuf> {
            self.calls.borrow().clone()
        }
    }

    impl InstructionIo for Fake {
        fn load(&self, path: &Path, _max_bytes: u64) -> Found {
            self.calls.borrow_mut().push(path.to_path_buf());
            self.files
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, f)| f.clone())
                .unwrap_or(Found::Absent)
        }
    }

    fn read(text: &str) -> Found {
        Found::Read {
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn texts(loaded: &[Instruction]) -> Vec<&str> {
        loaded.iter().map(|i| i.text.as_str()).collect()
    }

    // -- The gate ------------------------------------------------------

    /// The Step 13 mechanism: an untrusted workspace does not merely load
    /// nothing, it never asks the filesystem anything. No stat, no open — so
    /// there is nothing for a dialog to be raised about, which is what a
    /// headless untrusted run depends on.
    #[test]
    fn instructions_a_bare_posture_never_touches_the_filesystem() {
        let io = Fake::new(vec![("/ws/AGENTS.md", read("do the thing"))]);
        let selection = Selection::of(Posture::Bare, None);
        assert_eq!(selection, Selection::Disabled);

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/src"),
            &selection,
            Limits::default(),
        )
        .unwrap();

        assert!(set.loaded.is_empty());
        assert!(set.skipped.is_empty());
        assert_eq!(io.calls(), Vec::<PathBuf>::new(), "the port was called");
    }

    /// An explicitly named file is *chosen*, not discovered, so the ambient
    /// gate does not apply to it — which is the whole point of the flag in a
    /// headless untrusted run.
    #[test]
    fn instructions_an_explicit_file_survives_a_bare_posture() {
        let io = Fake::new(vec![("/elsewhere/rules.md", read("the rules"))]);
        let selection = Selection::of(Posture::Bare, Some(PathBuf::from("/elsewhere/rules.md")));
        assert_eq!(
            selection,
            Selection::Explicit(PathBuf::from("/elsewhere/rules.md"))
        );

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/src"),
            &selection,
            Limits::default(),
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["the rules"]);
    }

    /// An explicit file *replaces* the ambient set rather than adding to it
    /// (ADR §9): no `AGENTS.md` is even looked for.
    #[test]
    fn instructions_an_explicit_file_replaces_the_ambient_set() {
        let io = Fake::new(vec![
            ("/ws/AGENTS.md", read("ambient")),
            ("/elsewhere/rules.md", read("explicit")),
        ]);
        let selection = Selection::of(Posture::Ambient, Some(PathBuf::from("/elsewhere/rules.md")));

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/src"),
            &selection,
            Limits::default(),
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["explicit"]);
        assert_eq!(io.calls(), [PathBuf::from("/elsewhere/rules.md")]);
    }

    // -- Ordering ------------------------------------------------------

    /// Root first, cwd last, one candidate per directory in between — so a
    /// later entry is always the nearer one.
    #[test]
    fn instructions_are_planned_from_the_root_down_to_the_working_directory() {
        let paths = plan(
            Path::new("/ws"),
            Path::new("/ws/crates/core"),
            &Selection::Ambient,
        )
        .unwrap();

        assert_eq!(
            paths,
            [
                PathBuf::from("/ws/AGENTS.md"),
                PathBuf::from("/ws/crates/AGENTS.md"),
                PathBuf::from("/ws/crates/core/AGENTS.md"),
            ]
        );
    }

    #[test]
    fn instructions_at_the_root_itself_plan_exactly_one_candidate() {
        let paths = plan(Path::new("/ws"), Path::new("/ws"), &Selection::Ambient).unwrap();
        assert_eq!(paths, [PathBuf::from("/ws/AGENTS.md")]);
    }

    #[test]
    fn instructions_load_in_planned_order_skipping_directories_without_one() {
        let io = Fake::new(vec![
            ("/ws/AGENTS.md", read("workspace rules")),
            ("/ws/crates/core/AGENTS.md", read("crate rules")),
        ]);

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/crates/core"),
            &Selection::Ambient,
            Limits::default(),
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["workspace rules", "crate rules"]);
        assert!(set.skipped.is_empty(), "an absent file is not a refusal");
    }

    #[test]
    fn instructions_refuse_a_working_directory_outside_the_root() {
        let err = plan(
            Path::new("/ws"),
            Path::new("/somewhere/else"),
            &Selection::Ambient,
        )
        .unwrap_err();

        assert_eq!(err.root, PathBuf::from("/ws"));
        assert_eq!(err.cwd, PathBuf::from("/somewhere/else"));
    }

    // -- Refusals are reported, never silent ---------------------------

    /// A file that was there and did not contribute has to be nameable, with
    /// its reason: "your AGENTS.md was ignored" is not something a run may
    /// keep to itself.
    #[test]
    fn instructions_a_refused_file_is_reported_with_its_reason() {
        let io = Fake::new(vec![
            ("/ws/AGENTS.md", read("workspace rules")),
            ("/ws/src/AGENTS.md", Found::Refused(Refusal::Symlink)),
        ]);

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/src"),
            &Selection::Ambient,
            Limits::default(),
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["workspace rules"]);
        assert_eq!(set.skipped.len(), 1);
        assert_eq!(set.skipped[0].path, PathBuf::from("/ws/src/AGENTS.md"));
        assert_eq!(set.skipped[0].reason, Refusal::Symlink);
        assert!(
            set.skipped[0].reason.to_string().contains("symlink"),
            "the reason must say what it was: {}",
            set.skipped[0].reason
        );
    }

    #[test]
    fn instructions_that_are_not_utf8_are_refused_rather_than_mangled() {
        let io = Fake::new(vec![(
            "/ws/AGENTS.md",
            Found::Read {
                bytes: vec![0xff, 0xfe, 0x00],
            },
        )]);

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws"),
            &Selection::Ambient,
            Limits::default(),
        )
        .unwrap();

        assert!(set.loaded.is_empty());
        assert_eq!(set.skipped[0].reason, Refusal::NotText);
    }

    // -- Limits --------------------------------------------------------

    #[test]
    fn instructions_stop_at_the_total_byte_limit_and_say_so() {
        let io = Fake::new(vec![
            ("/ws/AGENTS.md", read("aaaaaaaa")),
            ("/ws/src/AGENTS.md", read("bbbbbbbb")),
        ]);
        let limits = Limits {
            max_total_bytes: 12,
            ..Limits::default()
        };

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/src"),
            &Selection::Ambient,
            limits,
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["aaaaaaaa"]);
        assert_eq!(
            set.skipped[0].reason,
            Refusal::TotalTooLarge { limit: 12 },
            "the second file must be refused, not truncated"
        );
    }

    #[test]
    fn instructions_stop_at_the_file_count_limit_and_say_so() {
        let io = Fake::new(vec![
            ("/ws/AGENTS.md", read("one")),
            ("/ws/a/AGENTS.md", read("two")),
            ("/ws/a/b/AGENTS.md", read("three")),
        ]);
        let limits = Limits {
            max_files: 2,
            ..Limits::default()
        };

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws/a/b"),
            &Selection::Ambient,
            limits,
        )
        .unwrap();

        assert_eq!(texts(&set.loaded), ["one", "two"]);
        assert_eq!(set.skipped[0].reason, Refusal::TooManyFiles { limit: 2 });
        assert!(
            !io.calls().contains(&PathBuf::from("/ws/a/b/AGENTS.md")),
            "a file past the count limit must not even be opened"
        );
    }

    /// The per-file limit belongs to the port — core must not ask for a file
    /// and then decide it was too big — so core's job is to pass it through
    /// unchanged and report what comes back.
    #[test]
    fn instructions_pass_the_per_file_limit_to_the_port() {
        struct Recorder(RefCell<Vec<u64>>);
        impl InstructionIo for Recorder {
            fn load(&self, _path: &Path, max_bytes: u64) -> Found {
                self.0.borrow_mut().push(max_bytes);
                Found::Refused(Refusal::TooLarge { limit: max_bytes })
            }
        }

        let io = Recorder(RefCell::new(Vec::new()));
        let limits = Limits {
            max_file_bytes: 4096,
            ..Limits::default()
        };
        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws"),
            &Selection::Ambient,
            limits,
        )
        .unwrap();

        assert_eq!(io.0.borrow().as_slice(), [4096]);
        assert_eq!(set.skipped[0].reason, Refusal::TooLarge { limit: 4096 });
    }

    // -- Digests -------------------------------------------------------

    /// SHA-256 of the bytes as read. The known vector is checked rather than
    /// merely "some hex", because a digest that is stable but wrong proves
    /// nothing about an export.
    #[test]
    fn instructions_carry_a_sha256_digest_of_their_bytes() {
        let io = Fake::new(vec![("/ws/AGENTS.md", read("abc"))]);

        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws"),
            &Selection::Ambient,
            Limits::default(),
        )
        .unwrap();

        assert_eq!(
            set.loaded[0].digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(set.loaded[0].bytes, 3);
        assert_eq!(set.bytes(), 3);
    }

    // -- The system prompt ---------------------------------------------

    fn loaded(files: Vec<(&str, &str)>, cwd: &str) -> InstructionSet {
        let io = Fake::new(
            files
                .into_iter()
                .map(|(path, text)| (path, read(text)))
                .collect(),
        );
        load(
            &io,
            Path::new("/ws"),
            Path::new(cwd),
            &Selection::Ambient,
            Limits::default(),
        )
        .unwrap()
    }

    /// The whole reason `SystemPrompt` carries both halves: what the model is
    /// sent and what the session header publishes are produced in one pass, so
    /// a descriptor cannot describe a file whose words never went anywhere.
    #[test]
    fn a_system_prompt_describes_exactly_the_files_it_quotes() {
        let set = loaded(
            vec![
                ("/ws/AGENTS.md", "run the tests"),
                ("/ws/crates/AGENTS.md", "and the clippy gate"),
            ],
            "/ws/crates",
        );

        let prompt = system_prompt(&set, Path::new("/ws"), &Selection::Ambient)
            .expect("two files contributed");

        let quoted: Vec<&str> = prompt.sources.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(quoted, ["/ws/AGENTS.md", "/ws/crates/AGENTS.md"]);
        for (source, instruction) in prompt.sources.iter().zip(&set.loaded) {
            assert!(
                prompt.text.contains(&instruction.text),
                "{} is described but not quoted",
                source.path
            );
            assert_eq!(source.sha256, instruction.digest);
        }
    }

    /// Root first, so the file nearest the working directory is the last word
    /// — and the preamble says that is what being last means. Without the
    /// framing, "the nearest file wins" is a rule nobody told the model.
    #[test]
    fn a_system_prompt_frames_its_files_nearest_last() {
        let set = loaded(
            vec![
                ("/ws/AGENTS.md", "the outer rule"),
                ("/ws/crates/AGENTS.md", "the inner rule"),
            ],
            "/ws/crates",
        );

        let prompt =
            system_prompt(&set, Path::new("/ws"), &Selection::Ambient).expect("two files loaded");

        let outer = prompt.text.find("the outer rule").expect("the outer file");
        let inner = prompt.text.find("the inner rule").expect("the inner file");
        assert!(outer < inner, "the nearer file must come last");
        assert!(
            prompt.text.starts_with("The following instructions"),
            "the text must say what it is before saying it: {}",
            prompt.text
        );
        assert_eq!(prompt.sources[0].scope, ".");
        assert_eq!(prompt.sources[1].scope, "crates");
    }

    /// A workspace that offered nothing is not a workspace that offered the
    /// empty string. `None` is the difference, and `TurnConfig` carries the
    /// `Option` so the request has no system message at all.
    #[test]
    fn a_workspace_that_loaded_nothing_has_no_system_prompt() {
        let set = loaded(Vec::new(), "/ws");

        assert!(set.loaded.is_empty());
        assert_eq!(
            system_prompt(&set, Path::new("/ws"), &Selection::Ambient),
            None
        );
    }

    /// `--system TEXT` is the user's own words, so it is attributable to no
    /// file — and says so with an empty `sources` rather than by inventing a
    /// path. A header built from this publishes no instructions, which is
    /// exactly right: none were used.
    #[test]
    fn a_verbatim_system_prompt_claims_no_file() {
        let prompt = SystemPrompt::verbatim("answer in French");

        assert_eq!(prompt.text, "answer in French");
        assert!(prompt.sources.is_empty());
    }

    /// An explicitly named file is marked `explicit`, because two files at the
    /// same path mean different things depending on whether the user asked for
    /// one — one is a claim the workspace made, the other is an instruction
    /// the operator gave.
    #[test]
    fn an_explicitly_named_file_is_published_as_explicit() {
        let io = Fake::new(vec![("/elsewhere/house-style.md", read("no semicolons"))]);
        let selection = Selection::Explicit(PathBuf::from("/elsewhere/house-style.md"));
        let set = load(
            &io,
            Path::new("/ws"),
            Path::new("/ws"),
            &selection,
            Limits::default(),
        )
        .unwrap();

        let prompt = system_prompt(&set, Path::new("/ws"), &selection).expect("one file loaded");

        assert_eq!(prompt.sources[0].source, InstructionSource::Explicit);
        // Outside the workspace, so it has no workspace-relative scope and
        // reports the directory it actually lives in.
        assert_eq!(prompt.sources[0].scope, "/elsewhere");
    }
}
