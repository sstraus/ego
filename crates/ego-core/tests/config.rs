//! Strict config schema, precedence resolution and the trust gate.
//!
//! See `plans/m0a-contracts.md` Step 12 and
//! `ARCHITECTURE.md`.

use ego_core::ceiling::{Budget, Ceilings};
use ego_core::config::{
    AuthProviderLayer, ConfigError, ConfigLayer, Mode, ModelRef, PermCfg, Posture, ProfileLayer,
    Source, Sourced, SourcedRule, Trust, resolve,
};
use ego_core::cost::{Amount, CurrencyCode, Money};
use ego_core::permit::rule::Rule;

fn layer(source: Source, value: ConfigLayer) -> Sourced<ConfigLayer> {
    Sourced { source, value }
}

/// A rule these tests build directly, bypassing the TOML they usually arrive
/// in. `load` and not `parse`, so a test cannot hand `resolve` a rule that a
/// config file could not have contained.
fn rule(raw: &str) -> Rule {
    Rule::load(raw).expect("a rule this build can enforce")
}

/// Whether `list` carries `raw` at all, ignoring which layers it came from.
///
/// Replaces `list.contains(&rule(raw))`, which stopped compiling when the
/// resolved lists started carrying origins. The tests that only care *whether*
/// a rule survived resolution say so with this; the ones that care *where it
/// came from* use `origins` below and say that instead.
fn carries(list: &[SourcedRule], raw: &str) -> bool {
    origins(list, raw).is_some()
}

/// Every layer `list` records as carrying `raw`, or `None` if no entry does.
///
/// Collected into a `Vec` so a test can write the expected layers in order.
/// `OriginSet` iterates most-authoritative-first, so that order is the
/// authority order and not the order the layers happened to be merged in.
fn origins(list: &[SourcedRule], raw: &str) -> Option<Vec<Source>> {
    list.iter()
        .find(|entry| entry.rule() == &rule(raw))
        .map(|entry| entry.origins().iter().collect())
}

fn amount(decimal: &str, currency: &str) -> Option<Money> {
    Some(Money::new(
        decimal.parse::<Amount>().expect("a valid test amount"),
        CurrencyCode::new(currency).expect("a valid test code"),
    ))
}

fn money(decimal: &str, currency: &str) -> Budget {
    Budget::Money(amount(decimal, currency).expect("built just above"))
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
        "model = \"anthropic/claude-sonnet-5\"\nmode = \"edits\"\n\n[permissions]\nallow = [\"read(src/**)\"]\ndeny = []\n",
    )
    .unwrap();
    assert_eq!(sourced.source, Source::Flags);
    assert_eq!(sourced.value.model, model("anthropic/claude-sonnet-5"));
    assert_eq!(sourced.value.mode, Some(Mode::Edits));
    assert_eq!(
        sourced.value.permissions,
        Some(PermCfg {
            allow: vec![rule("read(src/**)")],
            deny: vec![],
        })
    );
}

#[test]
fn auth_uses_the_same_strict_layer_parser_as_generation_config() {
    let sourced = ConfigLayer::parse(
        Source::User,
        "model = \"openai/gpt-5\"\n[auth.openai-codex]\ncredential_command = \"mint-codex\"\n[auth.openai-codex.headers]\noriginator = \"operator\"\n",
    )
    .unwrap();
    let effective = resolve(&[sourced], Trust::Untrusted, None).unwrap();

    let auth = effective.auth.get("openai-codex").unwrap();
    assert_eq!(auth.credential_command.as_deref(), Some("mint-codex"));
    assert_eq!(
        auth.headers.get("originator").map(String::as_str),
        Some("operator")
    );
}

#[test]
fn auth_precedence_merges_command_and_headers_per_field_and_name() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                auth: std::collections::HashMap::from([(
                    "openai-codex".to_string(),
                    AuthProviderLayer {
                        credential_command: Some("system-mint".to_string()),
                        headers: std::collections::HashMap::from([
                            ("Originator".to_string(), "system".to_string()),
                            ("user-agent".to_string(), "system-agent".to_string()),
                        ]),
                    },
                )]),
                ..Default::default()
            },
        ),
        layer(
            Source::User,
            ConfigLayer {
                auth: std::collections::HashMap::from([(
                    "openai-codex".to_string(),
                    AuthProviderLayer {
                        credential_command: None,
                        headers: std::collections::HashMap::from([
                            ("originator".to_string(), "operator".to_string()),
                            ("x-client".to_string(), "ego".to_string()),
                        ]),
                    },
                )]),
                ..Default::default()
            },
        ),
    ];

    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    let auth = effective.auth.get("openai-codex").unwrap();
    assert_eq!(auth.credential_command.as_deref(), Some("system-mint"));
    assert_eq!(auth.headers.len(), 3, "{:#?}", auth.headers);
    assert_eq!(
        auth.headers.get("originator").map(String::as_str),
        Some("operator")
    );
    assert_eq!(
        auth.headers.get("user-agent").map(String::as_str),
        Some("system-agent")
    );
    assert_eq!(
        auth.headers.get("x-client").map(String::as_str),
        Some("ego")
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

// --- ceilings (`2026-08-21-a-run-with-no-ceiling.md` §4) ---------------

/// The gap this closes against a published contract. `CLI.md:296-299`
/// promises an administrator a `budget` key on `/etc/ego/config.toml` and
/// calls the system layer "the only source of ceilings" — and every layer is
/// `deny_unknown_fields`, so until the key existed, writing the ceiling
/// `CLI.md` promises produced `ConfigError::UnknownKey`. The promise could
/// not be kept by anybody.
#[test]
fn config_a_budget_key_is_a_known_key_in_every_layer() {
    let base = ConfigLayer::parse(Source::System, "budget = \"5$\"\n").expect("a known key");
    assert_eq!(base.value.budget, Some(money("5", "USD")));

    let profiled = ConfigLayer::parse(Source::System, "[profile.ci]\nbudget = \"400k\"\n")
        .expect("a known key in a profile too");
    assert_eq!(
        profiled.value.profile["ci"].budget,
        Some(Budget::Tokens(400_000))
    );
}

/// The same grammar the flag takes, refused the same way. A spelling the flag
/// accepts and the config key rejects would be a ceiling an administrator
/// believes they set.
#[test]
fn config_a_budget_that_names_no_unit_is_refused_by_the_layer() {
    let err = ConfigLayer::parse(Source::System, "budget = \"400\"\n").unwrap_err();
    let said = err.to_string();
    assert!(said.contains("names no unit"), "{said}");
}

/// Ordinary precedence for the ordinary case: a higher layer's ceiling
/// replaces a lower one's, and raising your own limit is yours to do.
#[test]
fn config_a_higher_layer_may_raise_its_own_ceiling() {
    let layers = [
        layer(
            Source::User,
            ConfigLayer {
                budget: Some(money("1", "USD")),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                budget: Some(money("20", "USD")),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.ceilings.money, amount("20", "USD"));
    assert!(!effective.clamped.any(), "nobody capped anything");
}

/// The construction the deny ceiling already uses, one field over: a
/// `system` ceiling is gathered outside the ordinary merge and re-applied
/// after, so no higher-precedence layer can raise it. `min` and not "system
/// wins" — `CLI.md:311` binds everyone to the administrator's *maximum*.
#[test]
fn config_a_system_ceiling_cannot_be_raised_by_flags() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                budget: Some(money("5", "USD")),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                budget: Some(money("20", "USD")),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();

    assert_eq!(effective.ceilings.money, amount("5", "USD"));
    assert_eq!(
        effective.clamped.money,
        amount("20", "USD"),
        "and what was asked for is reported, because §4 requires the clamp be stated"
    );
}

/// The two fields clamp independently, which is what stops a user escaping a
/// money cap by naming a token ceiling instead.
#[test]
fn config_naming_a_token_ceiling_does_not_escape_a_system_money_cap() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                budget: Some(money("5", "USD")),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                budget: Some(Budget::Tokens(10_000_000)),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();

    assert_eq!(
        effective.ceilings.money,
        amount("5", "USD"),
        "the cap on the field nobody named is still in force"
    );
    assert_eq!(
        effective.ceilings.tokens,
        Some(10_000_000),
        "and the token ceiling the operator did name is untouched, \
         because no administrator capped it"
    );
}

/// And it survives a profile that overrides the layer, the same way the deny
/// ceiling does: a `[profile.NAME]` that names a larger budget changes what
/// its own layer contributes, not what the administrator's caps at.
#[test]
fn config_a_system_ceiling_survives_a_profile_that_raises_it() {
    let mut flags = ConfigLayer::default();
    flags.profile.insert(
        "ci".to_string(),
        ProfileLayer {
            budget: Some(money("50", "USD")),
            ..Default::default()
        },
    );
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                budget: Some(money("5", "USD")),
                ..Default::default()
            },
        ),
        layer(Source::Flags, flags),
    ];
    let effective = resolve(&layers, Trust::Trusted, Some("ci")).unwrap();

    assert_eq!(effective.ceilings.money, amount("5", "USD"));
    assert_eq!(effective.clamped.money, amount("50", "USD"));
}

/// A `system` layer's own active profile contributes to the ceiling, so an
/// administrator can cap a named profile more tightly than the fleet default.
#[test]
fn config_a_system_profile_tightens_the_ceiling_it_names() {
    let mut system = ConfigLayer {
        budget: Some(money("20", "USD")),
        ..Default::default()
    };
    system.profile.insert(
        "ci".to_string(),
        ProfileLayer {
            budget: Some(money("2", "USD")),
            ..Default::default()
        },
    );
    let layers = [
        layer(Source::System, system),
        layer(
            Source::Flags,
            ConfigLayer {
                budget: Some(money("10", "USD")),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, Some("ci")).unwrap();

    assert_eq!(effective.ceilings.money, amount("2", "USD"));
    assert_eq!(effective.clamped.money, amount("10", "USD"));
}

/// Nothing imposes a ceiling. `CLI.md:296-299` — "ego itself imposes none" —
/// so a resolution with nobody writing one produces two absences and not two
/// defaults.
#[test]
fn config_no_layer_writing_a_budget_leaves_no_ceiling_at_all() {
    let layers = [layer(
        Source::User,
        ConfigLayer {
            model: model("anthropic/claude-sonnet-5"),
            ..Default::default()
        },
    )];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();

    assert_eq!(effective.ceilings, Ceilings::default());
    assert!(!effective.clamped.any());
}

/// The ceiling **survives the merge**. It does not decide anything.
///
/// This test was called `config_system_deny_ceiling_cannot_be_relaxed_by_flags`
/// and that name is now a claim it does not check. "Cannot be relaxed" was
/// true of `resolve` while `resolve` ended with
/// `allow.retain(|e| !deny.contains(e))`, and that line is gone
/// (`docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6): whether a
/// deny beats an allow is now decided per call, by the engine, against a
/// canonical `PreparedCall`.
///
/// So the property did not lose its test — it changed owner. The owner is
/// `tests/permit.rs`'s
/// `permit_step1_a_deny_beats_an_allow_the_old_string_match_would_have_kept`,
/// which uses an allow *narrower* than the deny: that is the case the retain
/// let through, and the case a test with equal strings cannot see.
///
/// The two together are the whole property, and neither half is it alone.
/// A green test whose *name* claims a security property nothing checks is how
/// the next person concludes the property is covered, which is why the name
/// moved with the property rather than staying here to look reassuring.
///
/// What is checked here is real and still worth checking: a `Flags` layer that
/// sets `deny = []` would otherwise overwrite the system layer's deny outright,
/// and the ceiling at `resolve.rs:121-147` puts it back.
#[test]
fn config_system_deny_survives_a_higher_precedence_layer_that_clears_it() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec![rule("list(**)")],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![rule("list(**)")],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(carries(&effective.permissions.deny, "list(**)"));
    // And the flags layer's allow is still *there*. Resolution's job is to
    // guarantee the system deny survives the merge, not to decide which of the
    // two wins: that is decided per call, against a canonical `PreparedCall`,
    // by the engine.
    //
    // This assertion used to read `!allow.contains(...)`, satisfied by an
    // `allow.retain(|e| !deny.contains(e))` at the end of `resolve`. It passed
    // for a reason that does not generalise — the two strings happened to be
    // equal. Change the allow to `list(src/**)` and the retain kept it,
    // while the deny it violates sat right beside it in the same `Effective`.
    // See `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6.
    assert!(carries(&effective.permissions.allow, "list(**)"));
}

#[test]
fn config_non_system_deny_can_be_relaxed_by_higher_precedence() {
    let layers = [
        layer(
            Source::Project,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec![rule("read(secrets/**)")],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![rule("read(secrets/**)")],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(carries(&effective.permissions.allow, "read(secrets/**)"));
    assert!(!carries(&effective.permissions.deny, "read(secrets/**)"));
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
fn config_system_deny_survives_a_profile_override_that_clears_it() {
    let toml = "[permissions]\ndeny = [\"list(**)\"]\n\n[profile.ci.permissions]\nallow = [\"list(**)\"]\ndeny = []\n";
    let system = ConfigLayer::parse(Source::System, toml).unwrap();
    let effective = resolve(&[system], Trust::Trusted, Some("ci")).unwrap();
    assert!(carries(&effective.permissions.deny, "list(**)"));
    // Carried, not subtracted — see
    // `config_system_deny_survives_a_higher_precedence_layer_that_clears_it`.
    assert!(carries(&effective.permissions.allow, "list(**)"));
}

#[test]
fn config_system_deny_unions_across_multiple_system_layers() {
    let system_a = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec![],
                deny: vec![rule("list(**)")],
            }),
            ..Default::default()
        },
    );
    let system_b = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec![rule("list(**)")],
                deny: vec![rule("read(secrets/**)")],
            }),
            ..Default::default()
        },
    );
    let effective = resolve(&[system_a, system_b], Trust::Trusted, None).unwrap();
    assert!(carries(&effective.permissions.deny, "list(**)"));
    assert!(carries(&effective.permissions.deny, "read(secrets/**)"));
    // Carried, not subtracted — see
    // `config_system_deny_survives_a_higher_precedence_layer_that_clears_it`.
    assert!(carries(&effective.permissions.allow, "list(**)"));
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
                    allow: vec![rule("list(**)")],
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

// --- rules are parsed by the layer, not carried as text -----------------

/// A rule reaches `Effective` as a [`Rule`], so whatever consumes it cannot
/// choose its own parser. `CLI.md:29-31` promises one language across four
/// surfaces, and one language means one parser: a second one is how the config
/// path and the frontmatter path start disagreeing about what a string means.
#[test]
fn config_permission_rules_arrive_parsed_and_not_as_strings() {
    let layer = ConfigLayer::parse(
        Source::Project,
        "[permissions]\nallow = [\"read( src/** )\"]\ndeny = [\"list(**)\"]\n",
    )
    .expect("two well-formed rules");
    let permissions = layer.value.permissions.expect("the table was written");

    assert_eq!(permissions.allow, vec![rule("read(src/**)")]);
    assert_eq!(permissions.deny, vec![rule("list(**)")]);
    assert_eq!(
        permissions.allow[0].to_string(),
        "read(src/**)",
        "the spelling the operator used is not what is stored — the AST is"
    );
}

/// The teaching message survives the trip through serde. An operator meets the
/// grammar here first, and `unknown rule head` with no suggestion would send
/// them to look for a `bash` tool that is not the problem.
#[test]
fn config_a_tool_named_rule_is_refused_with_the_effect_it_should_have_named() {
    let err = ConfigLayer::parse(
        Source::Project,
        "[permissions]\nallow = [\"bash(git *)\"]\n",
    )
    .expect_err("`bash` is not a head");
    let said = err.to_string();

    assert!(said.contains("unknown rule head `bash`"), "{said}");
    assert!(said.contains("Rules name effects, not tools"), "{said}");
    assert!(said.contains("exec"), "{said}");
}

/// §6a at the surface an operator actually types into. The rule is well formed
/// and this build has nothing to match it with, so it is refused **here** —
/// not accepted and quietly ignored at the moment it would have mattered.
#[test]
fn config_a_rule_this_build_cannot_enforce_is_refused_at_load() {
    let err = ConfigLayer::parse(
        Source::System,
        "[permissions]\ndeny = [\"exec(rm -rf /)\"]\n",
    )
    .expect_err("no tool in this build produces exec operations");
    let said = err.to_string();

    assert!(said.contains("cannot be enforced"), "{said}");
    assert!(said.contains("a shell tool"), "{said}");
}

/// The refusal is not confined to `deny`, and this is the direction that would
/// be easy to miss: an unenforceable *allow* is harmless to the machine and
/// still a lie to the operator, who believes they have widened something.
#[test]
fn config_an_unenforceable_allow_is_refused_the_same_way() {
    let err = ConfigLayer::parse(
        Source::Project,
        "[permissions]\nallow = [\"net(api.github.com)\"]\n",
    )
    .expect_err("nothing in this build reaches the network through a rule");
    assert!(err.to_string().contains("cannot be enforced"));
}

/// A `[profile.NAME]` table's rules go through the same parser. A profile that
/// could smuggle an unparsed rule past the check would be a second surface,
/// which is the thing §9 exists to prevent.
#[test]
fn config_profile_rules_are_parsed_by_the_same_parser() {
    let err = ConfigLayer::parse(
        Source::Project,
        "[profile.ci.permissions]\nallow = [\"bash(cargo test)\"]\n",
    )
    .expect_err("`bash` is not a head in a profile either");
    assert!(err.to_string().contains("unknown rule head `bash`"));
}

#[test]
fn config_a_deny_written_in_two_layers_reports_the_layer_that_can_lift_it() {
    // The system layer and the user layer deny the same predicate. The merge
    // loop leaves `deny` holding the *user* copy; the ceiling pass then finds
    // the system copy already present.
    //
    // One rule, one entry — that part is correct for enforcement and has not
    // changed. What changed is that the entry now records *both* layers, so
    // the attribution can name the one whose deletion actually lifts the
    // denial. The old skip kept whichever origin arrived first, which is the
    // user's: the one line an operator can delete without effect.
    //
    // `docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md`.
    let overlapping = || PermCfg {
        allow: vec![],
        deny: vec![rule("read(/etc/**)")],
    };
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                permissions: Some(overlapping()),
                ..Default::default()
            },
        ),
        layer(
            Source::User,
            ConfigLayer {
                permissions: Some(overlapping()),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();

    assert_eq!(effective.permissions.deny.len(), 1, "one rule, one entry");
    let entry = &effective.permissions.deny[0];
    assert_eq!(entry.rule(), &rule("read(/etc/**)"));

    // Both layers are recorded. The user copy is what the merge loop left and
    // the system copy is what the ceiling pass unioned in, but the set reads
    // back most-authoritative-first: the order rules arrived in is an
    // implementation detail and nothing should depend on it.
    assert_eq!(
        entry.origins().iter().collect::<Vec<_>>(),
        [Source::System, Source::User]
    );

    // And the answer to "can I change this" is the system layer — the *least*
    // recently added and the *most* authoritative. An attribution that took
    // the first origin, or the highest merge rank, would say `user` here and
    // send the operator to delete a line that changes nothing.
    assert_eq!(entry.authority(), Source::System);
}

/// The complement, and the reason `authority` is not just "the first origin".
///
/// Nothing system-level is involved, so the operator's own layer is both the
/// only origin and the answer. Without this, a bug that hardcoded
/// `Source::System` as the authority would pass the test above.
#[test]
fn config_a_deny_only_the_operator_wrote_is_attributed_to_them() {
    let layers = [layer(
        Source::User,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec![],
                deny: vec![rule("read(/etc/**)")],
            }),
            ..Default::default()
        },
    )];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    let entry = &effective.permissions.deny[0];
    assert_eq!(entry.origins().iter().collect::<Vec<_>>(), [Source::User]);
    assert_eq!(entry.authority(), Source::User);
}

/// A rule the operator restated does not become theirs to lift, and the order
/// they arrive in does not decide it.
///
/// The test above has the user copy arriving first because that is what the
/// merge loop produces. This one pins the *rule* rather than that accident:
/// authority is a property of the layer, and no amount of restating moves it
/// (decision 1). Two layers above system, so the winner is not simply "the
/// last one added" either.
#[test]
fn config_authority_is_the_layer_not_the_arrival_order() {
    let mut entry = SourcedRule::new(rule("read(/etc/**)"), Source::Flags);
    assert_eq!(entry.authority(), Source::Flags);
    entry.also_carried_by(Source::User);
    assert_eq!(entry.authority(), Source::User, "user outranks flags");
    entry.also_carried_by(Source::Project);
    assert_eq!(
        entry.authority(),
        Source::User,
        "project is above user in merge precedence and below it in authority"
    );
    entry.also_carried_by(Source::System);
    assert_eq!(entry.authority(), Source::System);
    // Idempotent: restating does not duplicate.
    entry.also_carried_by(Source::System);
    assert_eq!(
        entry.origins().iter().collect::<Vec<_>>(),
        [Source::System, Source::User, Source::Project, Source::Flags]
    );
}
