#![allow(dead_code)]

use ego_core::permit::{Authorized, Permit};

fn impossible<T>() -> T {
    panic!("compile-fail fixture value")
}

fn access_permit_scope(permit: &Permit) {
    let _ = &permit.scope;
}

fn access_authorized(authorized: &Authorized) {
    let _ = &authorized.call;
}
fn main() {}
