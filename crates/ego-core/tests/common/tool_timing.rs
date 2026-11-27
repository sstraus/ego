use super::*;

#[derive(Debug)]
struct DelayedHuman;
impl HumanApprover for DelayedHuman {
    fn decide<'a>(
        &'a self,
        _: ItemId,
        _: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        Box::pin(async {
            std::thread::sleep(std::time::Duration::from_millis(40));
            PermissionDecision::Allow
        })
    }
}

// Catches: missing monotonic measurement or measuring approval from the fixed
// envelope clock, and timing written only after completion so live loses it.
#[test]
fn monotonic_tool_measurement_is_durable_before_completion() {
    let mut fx = Fixture::new(writing("/ws/out.txt", "allowed")).with_write_tool();
    fx.policy_source = Arc::new(FixedPolicySource {
        mode: Mode::Default,
        permissions: Ruleset::default(),
        human: Some(Arc::new(DelayedHuman)),
    });
    let (exit, ok, state) = run(&fx, SessionState::default(), "write it");
    assert_eq!(exit, ExitReason::Completed);
    assert!(ok);
    let events = fx.store.events();
    let (index, event, timing) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| {
            if let EventKind::ToolCallMeasured { timing } = event.kind {
                Some((index, event, timing))
            } else {
                None
            }
        })
        .expect("tool measurements were persisted");
    assert!(timing.approval_wait_ms >= 40);
    assert!(timing.execution_ms.is_some());
    assert!(matches!(
        events[index + 1].kind,
        EventKind::ToolCallCompleted { .. }
    ));
    assert_eq!(
        state.items[&event.env.item.unwrap()].tool_timing,
        Some(timing)
    );
    assert_eq!(SessionState::replay(events), state);
}
