//! The loopback provider stub must give its socket back when it is dropped.
//!
//! Every test in this binary shares one process and one descriptor table. A
//! stub that keeps its listener after the test ends leaks one socket per test,
//! and a few hundred tests later every `bind`, `spawn` and file write fails
//! with "Too many open files".

use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::support::{Plan, Stub};

#[test]
fn a_dropped_stub_stops_accepting_connections() {
    let stub = Stub::start(Plan::default());
    let address = stub.address.clone();
    TcpStream::connect(&address).expect("a live stub accepts connections");

    drop(stub);

    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(&address).is_ok() {
        assert!(
            Instant::now() < deadline,
            "{address} still accepts connections 5s after its stub was dropped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
