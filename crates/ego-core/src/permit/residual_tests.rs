use super::*;
use crate::permit::EnvPolicy;
use futures::FutureExt as _;
use futures::executor::block_on;

fn spec() -> SpawnSpec {
    SpawnSpec::new(vec!["/bin/true".into()], None, None, EnvPolicy::Inherited).unwrap()
}

fn evidence(path: &str) -> ResidualWriteEvidence {
    ResidualWriteEvidence {
        requested: path.into(),
        candidate: CanonPath::new(path),
        exists: false,
        kind: ResidualPathKind::Other,
        owned_by_workspace_user: None,
        listing: ResidualListing::default(),
        refusal: format!("{path}: Operation not permitted"),
        writable: vec![],
    }
}

fn granted(path: &str) -> ResidualDecision {
    ResidualDecision::Granted {
        path: CanonPath::new(path),
        recommended: ResidualScope::Session,
        reason: "r".into(),
    }
}

// Catches: a session keeps asking the paid judge after its decision budget,
// so a child printing endless refusals buys unbounded judge calls.
#[test]
fn the_session_stops_asking_after_its_decision_budget() {
    let desk = ResidualWriteDesk::new();
    let server = Arc::clone(&desk);
    for index in 0..MAX_DECISIONS_PER_SESSION {
        let path = format!("/out/{index}");
        let asked = desk.decide(spec(), evidence(&path), Box::new(|_| Ok(())));
        let answered = async {
            let request = server.next().await.unwrap();
            server.answer(
                request,
                ResidualDecision::Refused {
                    reason: "no".into(),
                },
            );
        };
        let (decision, ()) = block_on(futures::future::join(asked, answered));
        assert!(matches!(decision, ResidualDecision::Refused { .. }));
    }
    let decision = block_on(desk.decide(spec(), evidence("/out/last"), Box::new(|_| Ok(()))));
    assert!(matches!(decision, ResidualDecision::Unavailable { .. }));
    assert!(
        server.next().now_or_never().is_none(),
        "an over-budget refusal still reached the judge"
    );
}

// Catches: a grant the loop journaled is lost because the executor stopped
// waiting first, so the journal and the enforced perimeter disagree.
#[test]
fn a_journaled_grant_is_published_even_when_the_executor_stopped_waiting() {
    let desk = ResidualWriteDesk::new();
    let asked = desk.decide(spec(), evidence("/out/a"), Box::new(|_| Ok(())));
    // Poll once so the request is queued, then give up as a timeout would.
    let mut asked = Box::pin(asked);
    assert!((&mut asked).now_or_never().is_none());
    drop(asked);
    let request = block_on(desk.next()).unwrap();
    desk.answer(request, granted("/out/a"));
    assert_eq!(desk.grants(), vec![CanonPath::new("/out/a")]);
}

// Catches: an owner approval widens effects before its durable journal receipt.
#[test]
fn owner_grant_waits_for_receipt_and_never_becomes_a_judge_grant() {
    use crate::permit::{SandboxAccess, SandboxGrant};
    let desk = ResidualWriteDesk::new();
    let grant = SandboxGrant::new(CanonPath::new("/build/helper"), SandboxAccess::Exec);
    let mut requested = Box::pin(desk.record_owner_grant(grant.clone()));
    assert!(requested.as_mut().now_or_never().is_none());
    assert!(desk.owner_grants().is_empty());
    let request = block_on(desk.next_owner()).unwrap();
    desk.answer_owner(request);
    assert!(block_on(requested));
    assert_eq!(desk.owner_grants(), vec![grant]);
    assert!(desk.grants().is_empty());
}

// Catches: a failed owner receipt leaves an executable grant in the desk.
#[test]
fn lost_owner_receipt_grants_nothing() {
    use crate::permit::{SandboxAccess, SandboxGrant};
    let desk = ResidualWriteDesk::new();
    let mut requested = Box::pin(desk.record_owner_grant(SandboxGrant::new(
        CanonPath::new("/build/helper"),
        SandboxAccess::Exec,
    )));
    assert!(requested.as_mut().now_or_never().is_none());
    drop(block_on(desk.next_owner()).unwrap());
    assert!(!block_on(requested));
    assert!(desk.owner_grants().is_empty());
}

// Catches: configuration changes are ignored by the session decision budget.
#[test]
fn configured_session_limit_reaches_the_judge_budget() {
    let limits = ResidualLimits {
        decisions_per_session: std::num::NonZeroUsize::new(1).unwrap(),
        ..Default::default()
    };
    let desk = ResidualWriteDesk::with_limits(limits);
    let request = desk.decide(spec(), evidence("/out/first"), Box::new(|_| Ok(())));
    let answer = async {
        let request = desk.next().await.unwrap();
        desk.answer(request, granted("/out/first"));
    };
    block_on(futures::future::join(request, answer));
    assert!(matches!(
        block_on(desk.decide(spec(), evidence("/out/second"), Box::new(|_| Ok(())))),
        ResidualDecision::Unavailable { .. }
    ));
}

// Catches: a project config raises paid judge limits, or zero disables a bound.
#[test]
fn residual_limits_are_owner_configured_and_nonzero() {
    use crate::config::{ConfigLayer, Source, Sourced, Trust};
    let raw =
        "[residual_limits]\ndecisions_per_call=7\ndecisions_per_session=80\ndeadline_ms=20000\n";
    let layer = ConfigLayer::parse(Source::User, raw).unwrap().value;
    let resolved = crate::config::resolve(
        &[Sourced {
            source: Source::User,
            value: layer.clone(),
        }],
        Trust::Trusted,
        None,
    )
    .unwrap();
    assert_eq!(resolved.residual_limits.decisions_per_session.get(), 80);
    assert_eq!(resolved.residual_limits.deadline(), Duration::from_secs(20));
    let project = crate::config::resolve(
        &[Sourced {
            source: Source::Project,
            value: layer,
        }],
        Trust::Trusted,
        None,
    )
    .unwrap();
    assert_eq!(project.residual_limits, ResidualLimits::default());
    assert!(toml::from_str::<ConfigLayer>("[residual_limits]\ndeadline_ms=0").is_err());
}
