#![allow(dead_code)]

use ego_core::permit::{Approver, Permit, PolicyEngine, Scope};

fn impossible<T>() -> T {
    panic!("compile-fail fixture value")
}

fn assert_clone() {
    fn require_clone<T: Clone>() {}
    require_clone::<Permit>();
}

fn name_sealed() {
    let _ = ego_core::permit::Sealed(impossible());
}

fn name_mode_approver() {
    let _ = std::any::type_name::<ego_core::permit::ModeApprover>();
}

fn call_private_mint(engine: &PolicyEngine) {
    let _ = engine.mint(Scope::default());
}

fn call_private_with_approver(engine: &PolicyEngine) {
    let _ = engine.with_approver(impossible::<Box<dyn Approver>>());
}

fn main() {}
