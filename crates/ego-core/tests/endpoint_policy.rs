//! Contract for shared inbound endpoint policy.

use std::net::SocketAddr;

use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::{Admitted, BearerCredential};
use ego_core::permit::Confinement;

fn address(value: &str) -> SocketAddr {
    value.parse().expect("test address is valid")
}

#[test]
fn public_endpoints_cap_adaptive_and_off_without_widening_strict_choices() {
    let public = Admitted::Public(address("0.0.0.0:8080"));
    assert_eq!(
        public.cap_confinement(Confinement::Off),
        Confinement::Workspace
    );
    assert_eq!(
        public.cap_confinement(Confinement::Workspace),
        Confinement::Workspace
    );
    assert_eq!(public.cap_confinement(Confinement::Ro), Confinement::Ro);
    assert_eq!(
        public.cap_confinement(Confinement::Adaptive),
        Confinement::Workspace
    );
}

#[test]
fn local_and_loopback_endpoints_preserve_the_configured_ceiling() {
    let local = Admitted::Local {
        name: "ego.sock".to_owned(),
    };
    let loopback = Admitted::Loopback(address("127.0.0.1:8080"));
    for endpoint in [local, loopback] {
        for configured in Confinement::ALL {
            assert_eq!(endpoint.cap_confinement(configured), configured);
        }
    }
}

#[test]
fn shared_bearer_matcher_compares_secret_bytes_without_http_parsing() {
    let expected = BearerCredential::new(SecretMaterial::new(b"shared-secret".to_vec()));
    assert!(expected.matches(b"shared-secret"));
    assert!(!expected.matches(b"shared-secreT"));
    assert!(!expected.matches(b"Bearer shared-secret"));
    assert!(!expected.matches(b"shared-secret "));
}

#[test]
fn empty_bearer_secret_admits_nobody_and_debug_redacts_secret_bytes() {
    let empty = BearerCredential::new(SecretMaterial::new(Vec::new()));
    assert!(!empty.matches(b""));
    assert!(!empty.matches(b"anything"));

    let rendered = format!("{empty:?}");
    assert!(!rendered.contains("shared-secret"));
    assert!(!rendered.contains("BearerCredential(["));
}
