//! Compiler contract for the private fields that make permits unforgeable.

#[test]
fn permit_authority_fields_are_private() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/permit_fields_are_private.rs");
    cases.compile_fail("tests/ui/permit_construction_is_private.rs");
    cases.compile_fail("tests/ui/authorized_construction_is_private.rs");
    cases.compile_fail("tests/ui/scope_construction_is_private.rs");
    cases.compile_fail("tests/ui/permit_boundaries_are_private.rs");
}
