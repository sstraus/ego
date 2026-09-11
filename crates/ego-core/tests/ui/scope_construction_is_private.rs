#![allow(dead_code)]

use ego_core::permit::Scope;

fn compose_scope() {
    let _ = Scope {
        read_paths: Vec::new(),
        ..Scope::default()
    };
}

fn main() {}
