//! The permission rule grammar.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md`. Test
//! names are prefixed `rule_` so `cargo test -p ego-core rule` selects them:
//! cargo's filter matches the test path, not the file name.

use std::path::{Path, PathBuf};

use ego_core::permit::rule::{
    ArgvToken, GRAMMAR_VERSION, Head, HeadHint, Host, Rule, RuleId, RuleLoadError, RuleParseError,
    Segment,
};
use ego_core::permit::{CanonPath, Op};

fn parse(raw: &str) -> Rule {
    Rule::parse(raw).unwrap_or_else(|err| panic!("`{raw}` should parse: {err}"))
}

fn err(raw: &str) -> RuleParseError {
    Rule::parse(raw).expect_err(&format!("`{raw}` should not parse"))
}

fn root() -> PathBuf {
    PathBuf::from("/ws")
}

fn read(path: &str) -> Op {
    Op::Read {
        path: CanonPath::new(path),
    }
}

fn list(root: &str) -> Op {
    Op::List {
        root: CanonPath::new(root),
        pattern: "**".to_string(),
    }
}

// --- §1, §2: heads name effects ----------------------------------------

#[test]
fn rule_every_head_parses_and_reports_itself() {
    let cases = [
        ("read(src/main.rs)", Head::Read),
        ("write(out/**)", Head::Write),
        ("list(src)", Head::List),
        ("search(src)", Head::Search),
        ("exec(git *)", Head::Exec),
        ("net(api.github.com)", Head::Net),
        ("tool(github/create_issue)", Head::Tool),
    ];

    for (raw, head) in cases {
        assert_eq!(parse(raw).head(), head, "`{raw}`");
    }
}

#[test]
fn rule_a_tool_name_as_a_head_is_refused_and_names_the_effect_instead() {
    // The correction to `CLI.md:102`, and the reason it is an error rather
    // than an alias: an alias is a second spelling, and either the two share
    // an ID — so `rules ls` renders one of them back as something the
    // operator did not write — or they do not, and `revoke` on one leaves the
    // other standing.
    let denied = err("bash(git *)");

    assert!(
        matches!(
            &denied,
            RuleParseError::UnknownHead { head, hint: Some(HeadHint::Effect(Head::Exec)), .. }
                if head == "bash"
        ),
        "got {denied:?}"
    );

    let message = denied.to_string();
    assert!(message.contains("unknown rule head `bash`"), "{message}");
    assert!(
        message.contains("Rules name effects, not tools"),
        "{message}"
    );
    assert!(message.contains("`exec(…)`"), "{message}");
    assert!(
        message.contains("including bash"),
        "the message has to say why exec is the answer: {message}"
    );
}

#[test]
fn rule_a_misspelled_head_is_refused_and_suggests_the_head_not_an_effect() {
    let denied = err("raed(src/**)");
    assert!(
        matches!(
            &denied,
            RuleParseError::UnknownHead {
                hint: Some(HeadHint::Typo(Head::Read)),
                ..
            }
        ),
        "got {denied:?}"
    );
    assert!(denied.to_string().contains("`read(…)`"));
}

#[test]
fn rule_an_unknown_head_is_never_a_rule_that_matches_nothing() {
    // The worst failure mode available: the operator believes a boundary
    // exists. Every unrecognised head is an error, including ones no
    // suggestion fits.
    let denied = err("qqqq(x)");
    assert!(matches!(
        denied,
        RuleParseError::UnknownHead { hint: None, .. }
    ));
    assert!(denied.to_string().contains("read, write, list, search"));
}

#[test]
fn rule_a_bare_head_is_refused_and_names_the_parenthesised_form() {
    // `exec` alone would mean "any argv", which is a real thing to want and
    // already has a spelling. Two spellings of one predicate is what the
    // canonicalisation rules exist to prevent.
    let denied = err("exec");
    assert!(matches!(
        denied,
        RuleParseError::MissingPattern {
            head: Head::Exec,
            ..
        }
    ));
    assert!(denied.to_string().contains("exec(*)"));
    assert_eq!(parse("exec(*)").head(), Head::Exec);
}

#[test]
fn rule_an_empty_pattern_is_refused() {
    for raw in ["read()", "read(   )", "exec()", "net()"] {
        assert_eq!(err(raw), RuleParseError::EmptyPattern, "`{raw}`");
    }
}

#[test]
fn rule_something_that_is_not_head_of_pattern_is_refused() {
    for raw in ["", "read(src", "hello there"] {
        assert!(
            matches!(err(raw), RuleParseError::Malformed(_)),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
}

// --- §5: the AST is the rule, the text is a spelling of it -------------

#[test]
fn rule_two_spellings_of_one_predicate_are_one_rule_with_one_id() {
    // The property `ARCHITECTURE.md:245` asks for by name.
    let cases = [
        ("read( src/** )", "read(src/**)"),
        ("read(./src/lib.rs)", "read(src/lib.rs)"),
        ("read(src//lib.rs)", "read(src/lib.rs)"),
        ("read(src/./lib.rs)", "read(src/lib.rs)"),
        ("read(src/)", "read(src)"),
        ("read(a/../b)", "read(b)"),
        ("exec(git   *)", "exec(git *)"),
        // Adjacent wildcards collapse. Without this, `exec(git **)` would be
        // a second spelling that means something *different* — one token
        // matching anything, rather than any number of them — which is worse
        // than a second spelling that means the same thing.
        ("exec(git **)", "exec(git *)"),
        ("exec(cargo-**-x)", "exec(cargo-*-x)"),
        ("net(0177.0.0.1)", "net(127.0.0.1)"),
        ("net(2130706433)", "net(127.0.0.1)"),
        ("net(0x7f.0.0.1)", "net(127.0.0.1)"),
        ("net(127.1)", "net(127.0.0.1)"),
        ("net(API.GitHub.com)", "net(api.github.com)"),
    ];

    for (written, canonical) in cases {
        let (a, b) = (parse(written), parse(canonical));
        assert_eq!(a, b, "`{written}` and `{canonical}` are not one AST");
        assert_eq!(a.id(), b.id(), "`{written}` and `{canonical}` differ by id");
        assert_eq!(
            a.to_string(),
            canonical,
            "`{written}` does not render as its canonical spelling"
        );
    }
}

#[test]
fn rule_a_dotdot_that_survives_normalisation_is_refused_not_rewritten() {
    // `src/../../etc/**` does not become `../etc/**`. A relative rule that
    // leaves the workspace is either a mistake or an attack, and rewriting
    // either into a working rule is the wrong service to perform.
    for raw in ["read(src/../../etc/**)", "read(../etc)", "read(*/..)"] {
        assert!(
            matches!(err(raw), RuleParseError::EscapingPattern(_)),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
}

#[test]
fn rule_a_dotdot_may_not_cancel_a_wildcard() {
    // `*/..` is not `.`: `*` stands for a segment nobody has named, and
    // pruning it would silently widen the pattern.
    assert!(matches!(
        err("read(src/*/../secrets)"),
        RuleParseError::EscapingPattern(_)
    ));
}

#[test]
fn rule_there_are_exactly_two_star_constructs() {
    for raw in ["read(***)", "read(a**b)", "read(src/**/*.rs/**x)"] {
        assert!(
            matches!(err(raw), RuleParseError::BadStar(_)),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
    // And both of the two are accepted, in the shapes they name.
    let Rule::Read(pattern) = parse("read(src/**/*.rs)") else {
        panic!("a read rule");
    };
    assert!(matches!(
        pattern.segments(),
        [Segment::One(_), Segment::Any, Segment::One(_)]
    ));
}

#[test]
fn rule_an_absolute_pattern_stays_absolute_and_is_a_different_rule() {
    let Rule::Read(absolute) = parse("read(/etc/hosts)") else {
        panic!("a read rule");
    };
    let Rule::Read(relative) = parse("read(etc/hosts)") else {
        panic!("a read rule");
    };

    assert!(absolute.is_absolute());
    assert!(!relative.is_absolute());
    assert_ne!(
        parse("read(/etc/hosts)").id(),
        parse("read(etc/hosts)").id(),
        "an absolute rule and a relative one are not the same rule"
    );
}

#[test]
fn rule_a_relative_pattern_is_anchored_at_match_time_not_at_write_time() {
    // The property that makes a rule committed in `.ego/config.toml` mean the
    // same thing in every checkout — and the reason the id is computed over
    // the unanchored pattern.
    let rule = parse("read(src/**)");
    let id = rule.id();

    assert!(rule.matches(&read("/ws/src/main.rs"), Path::new("/ws")));
    assert!(rule.matches(&read("/elsewhere/src/main.rs"), Path::new("/elsewhere")));
    assert!(!rule.matches(&read("/ws/src/main.rs"), Path::new("/elsewhere")));
    assert_eq!(rule.id(), id, "matching does not change the identity");
}

// --- §5: what the two constructs mean -----------------------------------

#[test]
fn rule_a_single_star_does_not_cross_a_slash() {
    let rule = parse("read(src/*.rs)");

    assert!(rule.matches(&read("/ws/src/main.rs"), &root()));
    assert!(
        !rule.matches(&read("/ws/src/deep/lib.rs"), &root()),
        "`*` matched across a `/`"
    );
}

#[test]
fn rule_a_double_star_matches_zero_or_more_whole_segments() {
    let rule = parse("read(src/**/lib.rs)");

    assert!(
        rule.matches(&read("/ws/src/lib.rs"), &root()),
        "`**` has to match zero segments"
    );
    assert!(rule.matches(&read("/ws/src/a/lib.rs"), &root()));
    assert!(rule.matches(&read("/ws/src/a/b/c/lib.rs"), &root()));
    assert!(!rule.matches(&read("/ws/other/lib.rs"), &root()));
}

#[test]
fn rule_matching_folds_no_case_and_normalises_no_unicode() {
    // Deliberate, and it fails closed: matching compares the canonical path
    // — what the platform's own `canonicalize` returned — byte for byte.
    // Folding would make an id differ by platform, and an id that is not
    // stable across machines cannot be revoked from a config file.
    assert!(!parse("read(SRC/**)").matches(&read("/ws/src/main.rs"), &root()));
    // Composed é (U+00E9) against decomposed e + U+0301.
    assert!(!parse("read(caf\u{e9}.txt)").matches(&read("/ws/cafe\u{301}.txt"), &root()));
    // The rule still works when both sides agree.
    assert!(parse("read(caf\u{e9}.txt)").matches(&read("/ws/caf\u{e9}.txt"), &root()));
}

#[test]
fn rule_a_path_outside_the_root_matches_no_relative_rule() {
    assert!(!parse("read(**)").matches(&read("/etc/passwd"), &root()));
    assert!(parse("read(/etc/passwd)").matches(&read("/etc/passwd"), &root()));
}

// --- §2: the heads are separate authorities ----------------------------

#[test]
fn rule_a_read_rule_does_not_cover_a_list_of_the_same_path() {
    // `read`/`list`/`search` stay separate heads for the reason the scope
    // already gives: a grammar that collapsed them would re-introduce the
    // exfiltrating glob.
    let read_rule = parse("read(src/**)");
    let list_rule = parse("list(src)");

    assert!(read_rule.matches(&read("/ws/src/main.rs"), &root()));
    assert!(!read_rule.matches(&list("/ws/src"), &root()));
    assert!(list_rule.matches(&list("/ws/src"), &root()));
    assert!(!list_rule.matches(&read("/ws/src"), &root()));
}

#[test]
fn rule_a_call_is_matched_one_operation_at_a_time() {
    // Which is why there is no conjunction across heads: `exec(git *) &&
    // write(src/**)` describes a call with two operations, and a call with
    // two operations is matched by two rules.
    let ops = [read("/ws/src/main.rs"), list("/ws/src")];
    let rules = [parse("read(src/**)"), parse("list(src)")];

    for op in &ops {
        assert!(
            rules.iter().any(|rule| rule.matches(op, &root())),
            "{op:?} is covered by no rule"
        );
    }
    assert!(!parse("read(src/**)").matches(&ops[1], &root()));
}

// --- §6: exec patterns are token vectors -------------------------------

#[test]
fn rule_an_exec_pattern_is_tokenised_once_at_parse_time() {
    let Rule::Exec(pattern) = parse(r#"exec(git commit -m "a b")"#) else {
        panic!("an exec rule");
    };

    let tokens: Vec<String> = pattern
        .tokens()
        .iter()
        .map(|token| match token {
            ArgvToken::One(glob) => glob.to_string(),
            ArgvToken::AnyOne | ArgvToken::Rest => "*".to_string(),
        })
        .collect();
    assert_eq!(tokens, ["git", "commit", "-m", "a b"]);
}

#[test]
fn rule_a_bare_star_means_one_token_in_the_middle_and_any_number_at_the_end() {
    let Rule::Exec(trailing) = parse("exec(git *)") else {
        panic!("an exec rule");
    };
    assert!(matches!(trailing.tokens()[1], ArgvToken::Rest));
    assert!(trailing.matches(&["git".into()]), "zero remaining tokens");
    assert!(trailing.matches(&["git".into(), "push".into(), "--force".into()]));
    assert!(!trailing.matches(&["cargo".into(), "build".into()]));

    let Rule::Exec(middle) = parse("exec(git * --dry-run)") else {
        panic!("an exec rule");
    };
    assert!(matches!(middle.tokens()[1], ArgvToken::AnyOne));
    assert!(middle.matches(&["git".into(), "push".into(), "--dry-run".into()]));
    assert!(
        !middle.matches(&["git".into(), "--dry-run".into()]),
        "a middle `*` is exactly one token"
    );
}

#[test]
fn rule_a_star_inside_a_token_globs_within_that_token() {
    let Rule::Exec(pattern) = parse("exec(cargo-*)") else {
        panic!("an exec rule");
    };

    assert!(pattern.matches(&["cargo-nextest".into()]));
    assert!(!pattern.matches(&["cargo".into(), "nextest".into()]));
}

#[test]
fn rule_a_backslash_escapes_a_star_and_a_quote_only_groups() {
    // One spelling for the wildcard and one for the literal. Quoting groups
    // whitespace; it does not disarm a `*`, for the same reason `'a'` is
    // still the letter `a`.
    let Rule::Exec(literal) = parse(r"exec(echo \*)") else {
        panic!("an exec rule");
    };
    assert!(literal.matches(&["echo".into(), "*".into()]));
    assert!(!literal.matches(&["echo".into(), "anything".into()]));

    let Rule::Exec(quoted) = parse("exec(echo '*')") else {
        panic!("an exec rule");
    };
    assert_eq!(
        quoted, literal,
        "a quoted star and an escaped star are one rule"
    );
}

#[test]
fn rule_an_unclosed_quote_is_refused() {
    for raw in [r#"exec(echo "a)"#, r"exec(echo \)"] {
        assert!(
            matches!(err(raw), RuleParseError::UnbalancedQuote(_)),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
}

#[test]
fn rule_an_exec_pattern_is_never_rendered_back_into_a_shell_string() {
    // `rules ls` renders the token vector. The parentheses in the command
    // survive because the closing one is the last character of the rule, not
    // the first `)` found.
    assert_eq!(parse("exec(git   push)").to_string(), "exec(git push)");
    assert_eq!(parse("exec(echo (hi))").to_string(), "exec(echo (hi))");
}

// --- §5: endpoints are parsed authorities, not URLs --------------------

#[test]
fn rule_the_p19_confusion_is_unspellable() {
    // `net(127.0.0.1@evil.com)` is the destination confusion `POTENTIALS.md`
    // P19 records. It is a parse error rather than a rule that quietly means
    // the wrong host.
    for raw in [
        "net(127.0.0.1@evil.com)",
        "net(http://api.github.com)",
        "net(api.github.com/v3)",
        "net(api.github.com?x=1)",
    ] {
        assert!(
            matches!(err(raw), RuleParseError::BadEndpoint { .. }),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
}

#[test]
fn rule_a_subdomain_pattern_matches_subdomains_and_not_the_apex() {
    let Rule::Net(endpoint) = parse("net(*.github.com)") else {
        panic!("a net rule");
    };
    assert_eq!(endpoint.host(), &Host::Subdomains("github.com".to_string()));

    assert!(endpoint.matches("api.github.com", 443));
    assert!(endpoint.matches("a.b.github.com", 443));
    assert!(
        !endpoint.matches("github.com", 443),
        "the apex is not a subdomain; an operator who wants both writes both"
    );
    assert!(
        !endpoint.matches("evilgithub.com", 443),
        "the suffix has to end at a label boundary"
    );
}

#[test]
fn rule_a_host_name_never_matches_an_address_and_an_address_never_matches_a_name() {
    let Rule::Net(name) = parse("net(localhost)") else {
        panic!("a net rule");
    };
    let Rule::Net(address) = parse("net(127.0.0.1)") else {
        panic!("a net rule");
    };

    assert!(!name.matches("127.0.0.1", 80));
    assert!(!address.matches("localhost", 80));
    assert!(
        address.matches("2130706433", 80),
        "same address, other base"
    );
}

#[test]
fn rule_an_endpoint_without_a_port_covers_every_port() {
    let Rule::Net(any_port) = parse("net(api.github.com)") else {
        panic!("a net rule");
    };
    let Rule::Net(one_port) = parse("net(api.github.com:443)") else {
        panic!("a net rule");
    };

    assert!(any_port.matches("api.github.com", 80));
    assert!(any_port.matches("api.github.com", 443));
    assert!(one_port.matches("api.github.com", 443));
    assert!(!one_port.matches("api.github.com", 80));
    assert_eq!(one_port.port(), Some(443));
}

#[test]
fn rule_an_ipv6_literal_is_bracketed_and_canonicalised() {
    assert_eq!(parse("net([0:0:0:0:0:0:0:1])").to_string(), "net([::1])");
    assert_eq!(parse("net([::1]:8080)").to_string(), "net([::1]:8080)");
}

#[test]
fn rule_a_wildcard_in_a_host_is_only_the_leading_subdomain_marker() {
    assert!(matches!(
        err("net(api.*.com)"),
        RuleParseError::BadEndpoint { .. }
    ));
    let Rule::Net(any) = parse("net(*)") else {
        panic!("a net rule");
    };
    assert_eq!(any.host(), &Host::Any);
    assert!(any.matches("anything.example", 443));
}

// --- §3: the one identity predicate ------------------------------------

#[test]
fn rule_a_tool_rule_names_a_server_and_a_tool() {
    assert_eq!(
        parse("tool(github/create_issue)"),
        Rule::Tool {
            server: "github".to_string(),
            name: "create_issue".to_string(),
        }
    );
    for raw in ["tool(create_issue)", "tool(github/)", "tool(/create_issue)"] {
        assert!(
            matches!(err(raw), RuleParseError::BadToolId(_)),
            "`{raw}` gave {:?}",
            err(raw)
        );
    }
}

// --- what cannot match yet, and says so --------------------------------

#[test]
fn rule_exec_net_and_tool_match_no_operation_that_exists_today() {
    // Not a hole — there is no operation of any of those shapes in this
    // workspace. Recorded as a test so that the day one appears, whoever adds
    // it finds this failing and knows exactly which arm is missing.
    let ops = [read("/ws/git"), list("/ws")];
    for raw in ["exec(*)", "net(*)", "tool(github/create_issue)"] {
        for op in &ops {
            assert!(
                !parse(raw).matches(op, &root()),
                "`{raw}` matched {op:?}, which means an arm was added without a test"
            );
        }
    }
}

// --- §8: stable rule ids ------------------------------------------------

#[test]
fn rule_an_id_carries_its_grammar_generation_and_round_trips() {
    let id = parse("read(src/**)").id();

    let rendered = id.to_string();
    assert!(
        rendered.starts_with(&format!("r{GRAMMAR_VERSION}:")),
        "{rendered}"
    );
    assert_eq!(
        rendered.split_once(':').unwrap().1.len(),
        12,
        "60 bits is twelve characters: {rendered}"
    );
    assert_eq!(rendered.parse::<RuleId>().unwrap(), id);
    assert!(id.is_known());
}

#[test]
fn rule_an_id_from_a_later_generation_parses_but_is_not_known() {
    // Reported as unknown-generation and refused by the caller, never guessed
    // at: an `r2:` id on a build that only knows `r1` cannot be verified to
    // mean anything.
    let future: RuleId = "r2:9f2kq7x4mzt8".parse().expect("it is well formed");

    assert_eq!(future.generation(), 2);
    assert!(!future.is_known());
}

#[test]
fn rule_a_malformed_id_is_refused() {
    for raw in [
        "9f2kq7x4mzt8",
        "r1:",
        "rx:9f2kq7x4mzt8",
        "r1:short",
        "r1:!!",
    ] {
        assert!(raw.parse::<RuleId>().is_err(), "`{raw}` parsed");
    }
}

#[test]
fn rule_different_predicates_have_different_ids() {
    let ids: Vec<String> = [
        "read(src/**)",
        "write(src/**)",
        "list(src/**)",
        "search(src/**)",
        "read(src/*)",
        "read(/src/**)",
        "exec(git *)",
        "exec(git push)",
        "net(github.com)",
        "net(*.github.com)",
        "net(github.com:443)",
        "tool(github/create_issue)",
    ]
    .into_iter()
    .map(|raw| parse(raw).id().to_string())
    .collect();

    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        ids.len(),
        "two predicates share an id: {ids:?}"
    );
}

// --- §6a: a head with no operations refuses at load ---------------------

/// The distinction the whole section rests on. `net(...)` is a **valid
/// sentence** — it parses, canonicalises and takes an ID, because
/// `ARCHITECTURE.md:252` gates on the grammar being published whole. What it
/// lacks is an operation of that shape to match.
#[test]
fn rule_an_unenforceable_head_still_parses_and_still_has_an_id() {
    // `write(src/**)` and `exec(git *)` both left this list when real tools
    // began constructing their operations. Their IDs did not move, which is
    // what the design promised.
    for raw in ["net(github.com)", "tool(github/create_issue)"] {
        let rule = parse(raw);
        assert!(
            rule.head().awaiting().is_some(),
            "`{raw}` should have no operations in this build"
        );
        let id = rule.id().to_string();
        assert!(id.starts_with(&format!("r{GRAMMAR_VERSION}:")), "{id}");
        assert_eq!(
            id,
            parse(raw).id().to_string(),
            "an id nobody can enforce is still stable"
        );
    }
}

/// The other half: `load` refuses it, so nobody stores a boundary that is not
/// there. An operator who writes `net(github.com)` in a deny list would
/// otherwise get a rule that matches nothing and **cannot tell that apart from
/// a boundary that works** — "vacuous rather than fail-open" is a distinction
/// visible only to us.
#[test]
fn rule_load_refuses_a_head_with_no_operations_and_parse_does_not() {
    // `write(src/**)` and `exec(git *)` both left this list when real tools
    // began constructing their operations. Their IDs did not move.
    for raw in ["net(github.com)", "tool(github/create_issue)"] {
        assert!(Rule::parse(raw).is_ok(), "`{raw}` is a valid sentence");
        let err = Rule::load(raw).expect_err("nothing can enforce it");
        assert!(
            matches!(err, RuleLoadError::Unenforceable(_)),
            "`{raw}` is not a grammar error: {err:?}"
        );
    }
}

/// **Not a parse error**, and the type says so rather than the message. A
/// grammar error here would be a temporary lie about a permanent grammar, and
/// it would churn the rule's ID on the day the head became usable.
#[test]
fn rule_unenforceable_is_a_different_error_from_a_head_that_does_not_exist() {
    assert!(matches!(
        Rule::load("net(github.com)"),
        Err(RuleLoadError::Unenforceable(_))
    ));
    assert!(matches!(
        Rule::load("bash(git *)"),
        Err(RuleLoadError::Parse(RuleParseError::UnknownHead { .. }))
    ));
}

/// The refusal names the rule, the effect, and what is missing — because an
/// operator who is told only "refused" removes the wrong line, or removes the
/// rule and believes the boundary went with it.
#[test]
fn rule_the_refusal_says_which_rule_which_effect_and_what_is_missing() {
    let said = Rule::load("net(github.com)").unwrap_err().to_string();
    assert!(said.contains("net(github.com)"), "{said}");
    assert!(
        said.contains("no tool in this build produces net"),
        "{said}"
    );
    assert!(said.contains("a tool that reaches the network"), "{said}");
}

/// The heads that name operations this build produces are not refused. The
/// control that stops the test above from passing because `load` refuses
/// everything.
///
/// **`write(src/**)` left this list and has come back**, and the round trip is
/// worth more than either state. It was here originally because `Op::Write`
/// exists as a variant, not because anything constructed one — no builtin did
/// — so a stored `write(...)` rule loaded and matched nothing, which is the
/// failure §6a exists to prevent. `WriteTool` constructs one now, so the head
/// is enforceable again, and no rule was rewritten and no ID recomputed on
/// either crossing.
///
/// What decides which list this spelling belongs in is not this file. It is
/// `ego-tools/tests/tools.rs::the_heads_this_build_says_it_enforces_are_the_ones_its_tools_ask_for`,
/// which derives the answer from the registry instead of restating it — and
/// which was itself blind to a newly registered tool until it started
/// checking its sample list against `builtins()`.
#[test]
fn rule_load_admits_every_head_that_names_an_operation_that_exists() {
    for raw in [
        "read(src/**)",
        "write(src/**)",
        "list(src)",
        "search(src/**)",
        "exec(git *)",
    ] {
        assert_eq!(
            parse(raw).head().awaiting(),
            None,
            "`{raw}` names an operation this build produces"
        );
        Rule::load(raw).unwrap_or_else(|err| panic!("`{raw}` should load: {err}"));
    }
}

/// A host whose last label is a number is an **address**, and a broken address
/// is refused rather than demoted to a name.
///
/// Falling through to `Host::Name("256.1.1.1")` would store a rule that can
/// never match any destination, held by an operator who believes they named an
/// address — §2's worst failure mode reached by a typo. The transport agrees:
/// WHATWG makes `http://256.1.1.1/` an invalid URL, and
/// `ego-ai/tests/endpoint_agreement.rs` pins the two together.
#[test]
fn rule_a_host_that_looks_like_a_broken_address_is_not_quietly_a_name() {
    for spelling in [
        "256.1.1.1",
        "999.1.1.1",
        "127.0.0.256",
        "1.2.3.4.5",
        "0x1g.1.1.1",
        "a.1.1.1",
    ] {
        let RuleParseError::BadEndpoint { why, .. } = err(&format!("net({spelling})")) else {
            panic!("`{spelling}` should be refused as an endpoint, not parsed");
        };
        assert!(why.contains("not a valid one"), "{why}");
    }
}

/// The control: a numeric-looking host that *is* an address still parses, in
/// every base. Without this, refusing all of them would satisfy the test above
/// and break every address rule in the grammar.
#[test]
fn rule_a_host_that_looks_like_an_address_and_is_one_still_parses() {
    for spelling in ["127.0.0.1", "8.8.8.8", "12.34", "0x7f000001", "2130706433"] {
        let Rule::Net(endpoint) = parse(&format!("net({spelling})")) else {
            panic!("`{spelling}` should parse as an endpoint");
        };
        assert!(
            matches!(endpoint.host(), Host::Ip(_)),
            "`{spelling}` is an address"
        );
    }
}

/// §9's structural half: the head grammar is decomposed in exactly one place.
///
/// `Rule::parse`'s doc comment claims "there is no second one, in this crate
/// or any other". That was a sentence with nothing holding it, which is the
/// shape a claim takes right before it stops being true.
///
/// **Why a source scan is sound here rather than best-effort.** `parse::split`
/// is `pub` inside a *private* module and is not re-exported, so the compiler
/// already guarantees that every possible caller lives under this one
/// directory. The scan is not searching for callers it might miss; it is
/// enumerating a set the type system has already closed.
///
/// **What it does not cover, stated so nobody reads more into a green test.**
/// `Rule`'s variants are public and so are `PathPattern::parse`,
/// `ArgvPattern::parse` and `Endpoint::parse`, so another crate can build
/// `Rule::Read(PathPattern::parse("**")?)` and never touch the head grammar or
/// §6a's enforceability check. This test says the head is split once. It does
/// not say a `Rule` can only be made by splitting one.
#[test]
fn rule_the_head_grammar_has_exactly_one_caller() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/permit/rule");
    let mut callers = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("the rule module is where it has always been") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a readable source file");
        for (n, line) in source.lines().enumerate() {
            // `.split(` is `str`'s method and is not this. `fn split(` is the
            // definition, which is not a caller either.
            if line.contains("split(") && !line.contains(".split(") && !line.contains("fn split(") {
                callers.push(format!("{}:{}", path.display(), n + 1));
            }
        }
    }
    assert_eq!(
        callers.len(),
        1,
        "one caller, and it is `Rule::parse`. A second is how the flags path \
         and the frontmatter path start disagreeing about what one string \
         means — `AGENTS.md:60-64` wants recorded evidence of a different \
         trust boundary before that happens, and a rule in frontmatter is a \
         rule. Found: {callers:?}"
    );
}
