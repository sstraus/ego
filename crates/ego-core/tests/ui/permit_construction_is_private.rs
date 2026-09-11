#![allow(dead_code)]

use ego_core::permit::{Permit, Scope};

fn impossible<T>() -> T {
    panic!("compile-fail fixture value")
}

fn forge_permit() {
    let _ = Permit {
        scope: Scope::default(),
        _sealed: impossible(),
    };
}

fn main() {}
