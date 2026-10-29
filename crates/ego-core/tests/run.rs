//! How an invocation ends: the exit codes of `CLI.md` §4.3 and the
//! `exit_reason` words of §4.1, and how a turn's ending becomes a run's.
//!
//! See `docs/architecture.md` and
//! `plans/m0b-vertical-slice.md` Step 13.6.
//!
//! These are contract tests, not implementation tests: every assertion here
//! quotes a ratified line of `CLI.md`. If one fails, either the mapping
//! drifted or `CLI.md` changed — and the second is a decision, not a fix.

use ego_core::config::{Mode, Source};
use ego_core::event::domain::{CancelCause, ExitReason};
use ego_core::permit::rule::Rule;
use ego_core::permit::{Authority, RuleAuthority};
use ego_core::run::{Denial, DeniedBy, RunTermination, Signal};

/// A denial nobody answered, for the tests that only care that there was one.
fn nobody(action: &str) -> Denial {
    Denial {
        action: action.to_string(),
        by: DeniedBy::Nobody,
    }
}

/// Every value the `after_turn` mapping can be asked about, so a new
/// `ExitReason` variant cannot be added without this file failing to compile.
fn every_exit_reason() -> Vec<ExitReason> {
    vec![
        ExitReason::Completed,
        ExitReason::Cancelled(CancelCause::User),
        ExitReason::Cancelled(CancelCause::Timeout),
        ExitReason::Cancelled(CancelCause::Host),
        ExitReason::Budget,
        ExitReason::PermissionRequired,
        ExitReason::Provider,
        ExitReason::Internal,
    ]
}

#[test]
fn the_exit_codes_are_the_table_cli_md_publishes() {
    // §4.3, verbatim:
    //   0 done   1 task failed   2 usage error   3 permission needed
    //   4 provider/network   5 budget or turn limit   124 timeout
    //   130 SIGINT   143 SIGTERM
    let table: Vec<(RunTermination, u8)> = vec![
        (RunTermination::Done, 0),
        (RunTermination::TaskFailed, 1),
        (
            RunTermination::Failed {
                message: "a defect".to_string(),
            },
            1,
        ),
        (
            RunTermination::UsageError {
                message: "no such flag".to_string(),
            },
            2,
        ),
        (
            RunTermination::PermissionRequired {
                action: "read /etc/passwd".to_string(),
                by: DeniedBy::Nobody,
            },
            3,
        ),
        (
            RunTermination::Provider {
                message: "connection reset".to_string(),
            },
            4,
        ),
        (RunTermination::BudgetExceeded, 5),
        (RunTermination::MaxTurns, 5),
        (RunTermination::TimedOut, 124),
        (RunTermination::Signalled(Signal::Int), 130),
        (RunTermination::Signalled(Signal::Term), 143),
    ];

    for (termination, code) in table {
        assert_eq!(
            termination.exit_code(),
            code,
            "CLI.md §4.3 gives {termination:?} exit {code}"
        );
    }
}

#[test]
fn only_a_finished_run_exits_zero() {
    // The one assertion a script writes first, and the one that must never
    // become "well, mostly": a non-zero code is the only thing that stops a
    // `set -e` pipeline, so anything that is not `Done` has to produce one.
    for termination in [
        RunTermination::TaskFailed,
        RunTermination::Failed {
            message: "a defect".to_string(),
        },
        RunTermination::UsageError {
            message: "no such flag".to_string(),
        },
        RunTermination::PermissionRequired {
            action: "read /etc/passwd".to_string(),
            by: DeniedBy::Nobody,
        },
        RunTermination::Provider {
            message: "connection reset".to_string(),
        },
        RunTermination::BudgetExceeded,
        RunTermination::MaxTurns,
        RunTermination::TimedOut,
        RunTermination::Signalled(Signal::Int),
        RunTermination::Signalled(Signal::Term),
    ] {
        assert_ne!(
            termination.exit_code(),
            0,
            "{termination:?} is not a run that finished, so it cannot exit 0"
        );
    }
    assert_eq!(RunTermination::Done.exit_code(), 0);
}

#[test]
fn our_own_defect_never_tells_a_script_to_retry() {
    // §4.3 separates 4 from 1 so a caller can retry the first and not the
    // second. Fusing them — which is what a single `Error` variant did before
    // this taxonomy — advertises "retry me" for our own bug, and a retry loop
    // takes that literally.
    assert_eq!(
        RunTermination::Failed {
            message: "a defect".to_string()
        }
        .exit_code(),
        1
    );
    assert_eq!(
        RunTermination::Provider {
            message: "connection reset".to_string()
        }
        .exit_code(),
        4
    );
}

#[test]
fn the_exit_reason_words_are_the_ones_cli_md_publishes() {
    // §4.1: "done|budget|max_turns|timeout|permission|error". Six words, and
    // all six are reachable — a word in a ratified contract that nothing can
    // emit is a contract we only claim to meet.
    let words: Vec<(RunTermination, &str)> = vec![
        (RunTermination::Done, "done"),
        (RunTermination::BudgetExceeded, "budget"),
        (RunTermination::MaxTurns, "max_turns"),
        (RunTermination::TimedOut, "timeout"),
        (
            RunTermination::PermissionRequired {
                action: "read /etc/passwd".to_string(),
                by: DeniedBy::Decided(Authority::Boundary),
            },
            "permission",
        ),
        (RunTermination::TaskFailed, "error"),
        (
            RunTermination::UsageError {
                message: "no such flag".to_string(),
            },
            "error",
        ),
        (
            RunTermination::Provider {
                message: "connection reset".to_string(),
            },
            "error",
        ),
        (
            RunTermination::Failed {
                message: "a defect".to_string(),
            },
            "error",
        ),
        (RunTermination::Signalled(Signal::Int), "error"),
        (RunTermination::Signalled(Signal::Term), "error"),
    ];

    let published = [
        "done",
        "budget",
        "max_turns",
        "timeout",
        "permission",
        "error",
    ];
    let mut unspoken: Vec<&str> = published.to_vec();
    for (termination, word) in words {
        assert_eq!(
            termination.exit_reason(),
            word,
            "CLI.md §4.1 calls {termination:?} `{word}`"
        );
        unspoken.retain(|w| *w != word);
    }
    assert!(
        unspoken.is_empty(),
        "CLI.md §4.1 publishes words nothing can produce: {unspoken:?}"
    );
}

#[test]
fn the_two_limits_share_a_code_and_not_a_word() {
    // The one place §4.1 and §4.3 cut differently, and the reason they are
    // two functions rather than one: §4.3 gives both limits exit 5, §4.1
    // gives them separate words. A caller that needs the distinction has
    // somewhere to read it.
    assert_eq!(
        RunTermination::BudgetExceeded.exit_code(),
        RunTermination::MaxTurns.exit_code()
    );
    assert_ne!(
        RunTermination::BudgetExceeded.exit_reason(),
        RunTermination::MaxTurns.exit_reason()
    );
}

#[test]
fn a_completed_turn_does_not_end_the_run() {
    // Only the controller knows whether another turn follows, so the turn
    // level must not answer for it. `None` is that ignorance, stated.
    assert_eq!(
        RunTermination::after_turn(ExitReason::Completed, None),
        None
    );
}

#[test]
fn every_other_way_a_turn_can_end_ends_the_run() {
    // The gap this taxonomy closed: `ExitReason` had four variants and §4.3
    // needed nine outcomes, so several endings had no run-level answer at
    // all. This asserts there is no such hole left.
    for reason in every_exit_reason() {
        if reason == ExitReason::Completed {
            continue;
        }
        let termination = RunTermination::after_turn(reason, Some(nobody("read /etc/passwd")));
        assert!(
            termination.is_some(),
            "{reason:?} ends a turn but nothing says how the run ends"
        );
    }
}

#[test]
fn a_refused_call_carries_its_subject_all_the_way_to_exit_three() {
    // §4.4: an unresolvable permission exits 3 "with the denied action in the
    // JSON result". A 3 with no subject tells a script nothing it can act on,
    // so the action travels with the outcome rather than being looked up.
    let termination = RunTermination::after_turn(
        ExitReason::PermissionRequired,
        Some(Denial {
            action: "read /etc/passwd, outside the workspace".to_string(),
            by: DeniedBy::Decided(Authority::Boundary),
        }),
    )
    .expect("a refused call ends the run");

    assert_eq!(termination.exit_code(), 3);
    assert_eq!(termination.exit_reason(), "permission");
    match termination {
        RunTermination::PermissionRequired { action, .. } => assert_eq!(
            action, "read /etc/passwd, outside the workspace",
            "the action reaches the result unedited"
        ),
        other => panic!("a refused call is not {other:?}"),
    }
}

/// The fifth case, and the whole reason `by` exists.
///
/// `CLI.md` §4.4's three opt-ins "act on different axes": the boundary wants
/// `--sandbox` and explicitly not `--yolo`, a question nobody answered wants
/// `--allow`, and a deny rule wants none of them — it wants the config layer
/// edited. Every one of those is exit 3 and every one is the word
/// `permission`, so if the two endings below were equal the distinction would
/// exist nowhere a caller can read it.
#[test]
fn a_refusal_somebody_made_is_not_a_question_nobody_answered() {
    let refused = RunTermination::after_turn(
        ExitReason::PermissionRequired,
        Some(Denial {
            action: "edit may not write /ws/x".to_string(),
            by: DeniedBy::Decided(Authority::Boundary),
        }),
    )
    .expect("a refused call ends the run");
    let unanswered = RunTermination::after_turn(
        ExitReason::PermissionRequired,
        Some(nobody("edit may not write /ws/x")),
    )
    .expect("an unanswerable call ends the run too");

    // Same code, same word — which is why neither can carry the difference.
    assert_eq!(refused.exit_code(), unanswered.exit_code());
    assert_eq!(refused.exit_reason(), unanswered.exit_reason());
    assert_ne!(
        refused, unanswered,
        "exit 3 has two causes and the run termination must say which"
    );
}

/// A deny rule's authority reaches exit 3 with the layer still on it.
///
/// The one operational question an attribution answers is *can I change
/// this*, and `2026-08-21-a-deny-that-names-who-can-lift-it.md` is the reason
/// `RuleAuthority` carries `from` at all. A run that ends at exit 3 has no
/// second invocation in which to show it, so it travels here or nowhere.
#[test]
fn a_rule_that_denied_reaches_exit_three_with_the_layer_that_owns_it() {
    let rule = Rule::load("read(**)").expect("a rule this build enforces");
    let id = rule.id();
    let termination = RunTermination::after_turn(
        ExitReason::PermissionRequired,
        Some(Denial {
            action: "read may not read /ws/secret".to_string(),
            by: DeniedBy::Decided(Authority::Rules {
                rules: vec![RuleAuthority {
                    id: id.clone(),
                    from: Source::System,
                    also: Default::default(),
                }],
            }),
        }),
    )
    .expect("a refused call ends the run");

    match termination {
        RunTermination::PermissionRequired {
            by: DeniedBy::Decided(Authority::Rules { rules }),
            ..
        } => {
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].id, id);
            assert_eq!(
                rules[0].from,
                Source::System,
                "an operator told `user` would go and edit a file that cannot lift it"
            );
        }
        other => panic!("a rule denied it, so the authority is a rule: {other:?}"),
    }
}

/// The mode is its own authority and not a rule.
///
/// `plan` refuses every mutating op at §6 step 2, above the allow list, so
/// there is no rule to grep for and no `ego rules revoke` to run. Recording
/// `Rules { rules: [] }` instead would send an operator looking for a line
/// nobody wrote.
#[test]
fn a_mode_that_forbade_it_names_the_mode_and_no_rule() {
    let termination = RunTermination::after_turn(
        ExitReason::PermissionRequired,
        Some(Denial {
            action: "edit may not write /ws/x".to_string(),
            by: DeniedBy::Decided(Authority::Mode { mode: Mode::Plan }),
        }),
    )
    .expect("a refused call ends the run");

    assert!(matches!(
        termination,
        RunTermination::PermissionRequired {
            by: DeniedBy::Decided(Authority::Mode { mode: Mode::Plan }),
            ..
        }
    ));
}

#[test]
fn a_permission_required_turn_with_no_action_is_our_defect_not_exit_three() {
    // Inventing a placeholder subject would hide the bug that produced it,
    // and exiting 3 with an empty one would tell a script "ask for this" and
    // then not say what. Both are worse than admitting the defect.
    let termination = RunTermination::after_turn(ExitReason::PermissionRequired, None)
        .expect("it still ends the run");

    assert_eq!(
        termination.exit_code(),
        1,
        "we do not report our own bug as a permission the user could grant"
    );
    match termination {
        RunTermination::Failed { message } => assert!(
            message.contains("recorded no action"),
            "the message says what went wrong: {message}"
        ),
        other => panic!("a subjectless denial is not {other:?}"),
    }
}

#[test]
fn the_three_cancellation_causes_exit_differently() {
    // The whole reason `Cancelled` carries a cause: a deadline and a SIGINT
    // are 124 and 130, and before this they were the same value.
    let codes: Vec<u8> = [CancelCause::Timeout, CancelCause::User, CancelCause::Host]
        .into_iter()
        .map(|cause| {
            RunTermination::after_turn(ExitReason::Cancelled(cause), None)
                .expect("a cancelled turn ends the run")
                .exit_code()
        })
        .collect();

    assert_eq!(codes, vec![124, 130, 143]);
}

#[test]
fn a_deadline_is_a_timeout_and_not_an_error() {
    // §4.1 spells a deadline `timeout`, distinctly from `error`: a scheduled
    // job that reads `error` cannot tell "the model was slow" from "the
    // provider is down", and those want different responses.
    let termination = RunTermination::after_turn(ExitReason::Cancelled(CancelCause::Timeout), None)
        .expect("a cancelled turn ends the run");
    assert_eq!(termination.exit_reason(), "timeout");
}

#[test]
fn a_turn_that_ran_out_of_budget_is_not_reported_as_an_error() {
    // A ceiling the run was configured with is not a failure of the run.
    let termination =
        RunTermination::after_turn(ExitReason::Budget, None).expect("a capped turn ends the run");
    assert_eq!(termination, RunTermination::BudgetExceeded);
    assert_eq!(termination.exit_code(), 5);
    assert_eq!(termination.exit_reason(), "budget");
}

#[test]
fn an_upstream_failure_and_an_internal_one_stay_apart_across_the_two_levels() {
    // The distinction is only worth adding to `ExitReason` if it survives
    // the trip to the run level. This is the trip.
    let provider = RunTermination::after_turn(ExitReason::Provider, None).expect("it ends the run");
    let internal = RunTermination::after_turn(ExitReason::Internal, None).expect("it ends the run");

    assert_eq!(provider.exit_code(), 4);
    assert_eq!(internal.exit_code(), 1);
    assert_ne!(provider, internal);
}
