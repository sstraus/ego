//! contracts for regression case's shared CLI session surfaces.

use ego_cli::catalogue::Listing;
use ego_cli::repl::actions::{ActionId, ActionRegistry, Availability};
use ego_core::event::id::{Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::session::SessionLineage;
use ego_core::store::session_store::SessionMeta;
use serde_json::json;

#[test]
fn fork_command_accepts_only_the_session_id_on_the_existing_cli_parser() {
    let source = "00000000-0000-0000-0000-000000000065";
    let parsed = ego_cli::cli::parse(vec!["ego".to_owned(), "fork".to_owned(), source.to_owned()]);
    assert!(
        parsed.is_ok(),
        "ego fork must be a built command: {parsed:?}"
    );

    let extra_selector = ego_cli::cli::parse(vec![
        "ego".to_owned(),
        "fork".to_owned(),
        source.to_owned(),
        "--at-seq".to_owned(),
        "7".to_owned(),
    ]);
    assert!(
        extra_selector.is_err(),
        "fork must refuse a point selector rather than silently changing the durable boundary"
    );
}

#[test]
fn fork_sessions_and_new_share_enabled_action_registrations_and_slash_routes() {
    let registry = ActionRegistry::final_surface();
    for action in [ActionId::Fork, ActionId::Sessions, ActionId::New] {
        assert_eq!(registry.availability(action), Availability::Enabled);
        assert_eq!(registry.help(action).id(), action);
        assert!(registry.help(action).disabled_reason().is_none());
    }
    assert_eq!(registry.resolve_slash("/fork"), Some(ActionId::Fork));
    assert_eq!(
        registry.resolve_slash("/sessions"),
        Some(ActionId::Sessions)
    );
    assert_eq!(registry.resolve_slash("/new"), Some(ActionId::New));
}

#[test]
fn catalogue_versions_the_listing_envelope_and_serialises_tagged_lineage_rows() {
    let id = SessionId::new();
    let at_seq: Seq = serde_json::from_value(json!(7_u64)).unwrap();
    let meta = SessionMeta {
        profiles: Default::default(),
        id,
        workspace_root: "/workspace".into(),
        model: None,
        created: Timestamp::from_millis(1),
        updated: Timestamp::from_millis(2),
        title: Some("fork".to_owned()),
        summary: None,
        summary_failures: 0,
        lineage: Some(SessionLineage::Fork { source: id, at_seq }),
    };
    let compacted = SessionMeta {
        profiles: Default::default(),
        id: SessionId::new(),
        workspace_root: "/workspace".into(),
        model: None,
        created: Timestamp::from_millis(3),
        updated: Timestamp::from_millis(4),
        title: Some("compacted".to_owned()),
        summary: None,
        summary_failures: 0,
        lineage: Some(SessionLineage::Compaction {
            source: id,
            source_seq: at_seq,
            prompt_revision: 3,
        }),
    };
    let listing = serde_json::to_value(Listing::of(&[meta, compacted])).unwrap();
    assert_eq!(listing["v"], 3);
    assert!(listing["sessions"][0].get("catalogue_version").is_none());
    assert_eq!(listing["sessions"][0]["lineage"]["kind"], "fork");
    assert_eq!(
        listing["sessions"][0]["lineage"]["source"],
        serde_json::to_value(id).unwrap()
    );
    assert_eq!(listing["sessions"][0]["lineage"]["at_seq"], 7);
    assert_eq!(listing["sessions"][1]["lineage"]["kind"], "compaction");
    assert_eq!(listing["sessions"][1]["lineage"]["source_seq"], 7);
    assert_eq!(listing["sessions"][1]["lineage"]["prompt_revision"], 3);
}
