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
