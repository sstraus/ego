#![allow(dead_code)]

use ego_core::permit::{Authorized, Authority, Permit, PreparedCall};

fn forge_authorized(call: PreparedCall, permit: Permit) {
    let _ = Authorized {
        call,
        permit,
        by: Authority::Party,
    };
}

fn main() {}
