//! Observable publication contracts for the live skill-rescan command.
use super::*;

#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

impl Diagnostics for Notes {
    fn note(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

struct RescanFixture {
    _directory: tempfile::TempDir,
    user: PathBuf,
    catalogue: crate::skill::SkillCatalogue,
    store: Arc<JsonlStore>,
    args: Args,
    effective: Effective,
    notes: Notes,
}

impl RescanFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        let user = directory.path().join("user");
        let project = directory.path().join("project");
        for root in [&system, &user, &project] {
            std::fs::create_dir_all(root).unwrap();
        }
        let catalogue = crate::skill::SkillCatalogue::discover(
            crate::skill::SkillDiscovery::new(system, user.clone(), project, false),
            &["help", "skills", "hooks", "quit"],
        )
        .unwrap();
        let crate::cli::Invocation::Interactive(args) =
            crate::cli::parse(vec!["ego".into()]).unwrap()
        else {
            panic!("interactive arguments")
        };
        let store = Arc::new(session::jsonl(
            directory.path().join("sessions"),
            directory.path().to_owned(),
        ));
        Self {
            _directory: directory,
            user,
            catalogue,
            store,
            args: *args,
            effective: ego_core::config::resolve(&[], Trust::Trusted, None).unwrap(),
            notes: Notes::default(),
        }
    }

    fn write_skill(&self, contents: &str) {
        let skill = self.user.join("new-evidence");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), contents).unwrap();
    }

    fn rescan(&self, pending: bool) -> Result<Option<Vec<RegisteredTool>>, RunTermination> {
        let seat = super::super::tests::CompactionDecisionSeat {
            confirm: false,
            activate: true,
        };
        rescan_session_skills(
            pending,
            Some(&self.catalogue),
            &seat,
            (
                &self.args,
                &self.effective,
                &self.store,
                ego_core::permit::EnvPolicy::Inherited,
                std::path::Path::new("/ws"),
            ),
            &self.notes,
        )
    }
}

const VALID_SKILL: &str =
    "---\nname: new-evidence\ndescription: Collect new evidence.\n---\nRead the evidence.\n";

// Catches: a pending-work refusal still publishing changed skill files.
#[test]
fn pending_rescan_keeps_the_published_catalogue_unchanged() {
    let fixture = RescanFixture::new();
    let before = fixture.catalogue.capture().unwrap();
    fixture.write_skill(VALID_SKILL);
    assert!(fixture.rescan(true).unwrap().is_none());
    let after = fixture.catalogue.capture().unwrap();
    assert_eq!(before.snapshot_digest(), after.snapshot_digest());
    assert!(
        !after
            .descriptors()
            .iter()
            .any(|skill| skill.name() == "new-evidence")
    );
    assert_eq!(
        *fixture.notes.0.lock().unwrap(),
        ["cannot rescan skills while work is pending; wait for it to finish"]
    );
}

// Catches: committing the prospective catalogue before model-tool preflight succeeds.
#[test]
fn failed_rescan_tool_preflight_does_not_publish_the_candidate() {
    let mut fixture = RescanFixture::new();
    let before = fixture.catalogue.capture().unwrap();
    fixture.write_skill(VALID_SKILL);
    fixture.args.tools = Some(vec!["not-a-tool".to_owned()]);
    let error = match fixture.rescan(false) {
        Err(error) => error,
        Ok(_) => panic!("invalid model tool selection must refuse the rescan"),
    };
    assert!(
        matches!(error, RunTermination::Failed { message } if message.contains("could not rebuild model tools"))
    );
    let after = fixture.catalogue.capture().unwrap();
    assert_eq!(before.snapshot_digest(), after.snapshot_digest());
    assert!(
        !after
            .descriptors()
            .iter()
            .any(|skill| skill.name() == "new-evidence")
    );
}

// Catches: a rescan failing to refresh model tools, or invalid files replacing last-good skills.
#[test]
fn rescan_refreshes_model_tools_and_retains_last_good_after_invalid_edits() {
    let fixture = RescanFixture::new();
    fixture.write_skill(VALID_SKILL);
    let registry = fixture.rescan(false).unwrap().unwrap();
    let published = fixture.catalogue.capture().unwrap();
    assert!(
        published
            .descriptors()
            .iter()
            .any(|skill| skill.name() == "new-evidence")
    );
    assert!(
        registry
            .iter()
            .any(|tool| tool.descriptor().name == "skill")
    );
    assert!(
        registry
            .iter()
            .any(|tool| tool.descriptor().name == "context")
    );
    assert!(fixture.notes.0.lock().unwrap()[0].starts_with("skills: published generation "));

    fixture.write_skill("---\nname: [unterminated\n---\nInvalid metadata.\n");
    let retained_registry = fixture.rescan(false).unwrap().unwrap();
    let retained = fixture.catalogue.capture().unwrap();
    assert_eq!(published.snapshot_digest(), retained.snapshot_digest());
    assert!(retained.retained_last_good());
    assert!(
        retained_registry
            .iter()
            .any(|tool| tool.descriptor().name == "skill")
    );
    assert!(
        fixture.notes.0.lock().unwrap()[1].starts_with("skills: retained last-good generation ")
    );
}
