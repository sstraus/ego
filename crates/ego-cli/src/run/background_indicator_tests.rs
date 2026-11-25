use super::*;
#[cfg(unix)]
use ego_core::event::id::DetachedExecId;
#[cfg(unix)]
use ego_core::permit::{EnvPolicy, SpawnSpec};

#[cfg(unix)]
#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

#[cfg(unix)]
impl Diagnostics for Notes {
    fn note(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

// Catches: counting launched/completed jobs instead of live ownership, emitting
// human text into structured output, or consuming the normal completion delivery.
#[cfg(unix)]
#[tokio::test]
async fn background_indicator_tracks_live_children_and_processes_at_the_output_boundary() {
    let root = tempdir().unwrap();
    let store = Arc::new(crate::session::jsonl(
        root.path().join("sessions"),
        root.path().to_path_buf(),
    ));
    let executor = Arc::new(LaunchBarrierExecutor {
        crossed: Mutex::new(None),
        release: Some(Arc::new(tokio::sync::Notify::new())),
        interrupt_failure: false,
        submit_error: false,
    });
    let crossed = executor.arm();
    let fixture = DrainFixture::new(root.path(), &store, executor);
    let (ports, _, _) = run_effect_ports(
        RunConfinement::new(Confinement::Off, false, CanonPath::new(root.path())).unwrap(),
        SessionEvidenceStore::open(root.path(), fixture.session, 1024 * 1024).unwrap(),
        CancelToken::new(),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let notes = Notes::default();
    let report = |output, human, width| {
        background_indicator::report(
            output,
            human,
            width,
            &fixture.owner.hooks.hook_state().state(),
            ports.process(),
            &fixture.owner,
            &notes,
        );
        std::mem::take(&mut *notes.0.lock().unwrap())
    };
    assert!(report(Output::Text, true, 80).is_empty());
    let child = fixture.background_task().await;
    crossed.await.unwrap();

    let exec = DetachedExecId::new();
    fixture
        .owner
        .hooks
        .hook_state()
        .journal()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::DetachedExecStarted { id: exec },
        )
        .unwrap();
    let settled = Arc::new(tokio::sync::Notify::new());
    let notify = settled.clone();
    ports.set_background_waker(Arc::new(move || notify.notify_one()));
    let spec = SpawnSpec::new(
        vec!["/bin/sh".into(), "-c".into(), "exec /bin/sleep 10".into()],
        Some(CanonPath::new(root.path())),
        Some(Duration::from_secs(10)),
        EnvPolicy::default(),
    )
    .unwrap();
    ports.process().start_background(exec, &spec).await.unwrap();

    let active = report(Output::Text, true, 80);
    assert_eq!(active.len(), 1);
    assert!(
        active[0].starts_with("↻ Background: 2 active · check · exec …"),
        "{active:?}"
    );
    assert!(!active[0].contains("exec /bin/sleep"));
    let narrow = report(Output::Text, true, 33);
    assert_eq!(narrow, ["↻ Background: 2 active · +2"]);
    for (output, human) in [
        (Output::Json, true),
        (Output::Jsonl, true),
        (Output::Text, false),
    ] {
        assert!(report(output, human, 80).is_empty());
    }

    fixture
        .owner
        .inspect_tasks("cancel t1", &fixture.owner.hooks.hook_state().state())
        .await
        .unwrap();
    fixture.execution_finished(child).await;
    ports.process().cancel_background(exec).unwrap();
    tokio::time::timeout(Duration::from_secs(5), settled.notified())
        .await
        .unwrap();
    // Neither settlement has been collected into the parent state yet.
    assert!(report(Output::Text, true, 80).is_empty());
    use ego_core::turn::BackgroundTaskPort;
    fixture
        .owner
        .refresh_tasks(fixture.owner.hooks.hook_state().state())
        .await
        .unwrap();
    assert!(
        fixture
            .owner
            .hooks
            .hook_state()
            .state()
            .delivered_subagents
            .contains_key(&child)
    );
}

// Catches: long/untrusted Unicode names overflowing or injecting terminal controls,
// hiding tasks beyond the two-name budget, or ignoring the rendered notice prefix.
#[test]
fn background_indicator_bounds_names_and_retains_overflow_count() {
    let labels = vec![
        format!("private-token{}", "界\x1b[2J\n".repeat(50)),
        "mail-watch".into(),
        "third".into(),
    ];
    let secrets = ego_core::secrets::SecretValues::new([b"private-token".to_vec()]);
    let line = background_indicator::compact(&labels, 80, &secrets).unwrap();
    assert!(line.starts_with("↻ Background: 3 active"));
    assert!(line.ends_with(" · +1"), "{line}");
    assert!(!line.contains(['\x1b', '\n', '\r']));
    assert!(!line.contains("private-token"));
    assert!(line.contains("[REDACTED]"));
    let row = format!("{}: {line}", crate::DISPLAY_NAME);
    assert!(crate::render::Line::plain(&row).width() <= 79);
    for count in [2, 3] {
        let labels = vec!["x".repeat(24); count];
        for width in [32, 80, 100] {
            let line = background_indicator::compact(&labels, width, &secrets).unwrap();
            let row = format!("{}: {line}", crate::DISPLAY_NAME);
            assert!(
                crate::render::Line::plain(&row).width() < width,
                "{width} columns: {row}"
            );
        }
    }
}
