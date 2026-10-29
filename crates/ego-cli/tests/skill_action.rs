//! Historical wire assertions retained at the process-test target.
//!
//! The registry-owned behavioural coverage moved into the crate unit tests so
//! it can bind the crate-private `SkillCatalogueView` owner. This target keeps
//! the public operation wire boundary covered without duplicating that private
//! fixture seam.

use ego_core::skill::SkillOperation;

#[test]
fn model_prose_is_not_a_typed_skill_operation_wire_value() {
    let prose = "The model says /explain sh -c 'touch /tmp/nope'";
    let operation = serde_json::json!({
        "kind": "invoke",
        "name": "explain",
        "arguments": prose,
    });
    assert!(serde_json::from_value::<SkillOperation>(operation).is_ok());
    assert!(serde_json::from_str::<SkillOperation>(prose).is_err());
}
