//! What the test binaries in this directory share.
//!
//! One file rather than a copy per binary: `Credentials` is the seam every
//! test that assembles a request has to stand in for, and two doubles that
//! drifted apart would let one binary prove something the other disproved.

// Each integration test is its own crate and compiles this module separately,
// so an item only one binary needs is dead code in the others. The alternative
// is an API contorted to be fully used everywhere, which is a worse trade than
// one scoped allow on a test-only module.
#![allow(dead_code)]

use ego_ai::provider::{Credentials, MissingCredential, Secret};

/// A [`Credentials`] that answers with what it was built with, whatever
/// variable is asked for.
///
/// The variable name is not matched on purpose. A test double that checked it
/// would be re-implementing the lookup [`EnvCredentials`](ego_ai::provider::EnvCredentials)
/// owns, and the thing under test here is never *which* variable was consulted
/// — it is what the row does with the answer.
pub struct FakeCredentials(Result<&'static str, ()>);

impl FakeCredentials {
    /// Every variable is set, to `value`.
    pub fn holding(value: &'static str) -> Self {
        FakeCredentials(Ok(value))
    }

    /// No variable is set.
    pub fn empty() -> Self {
        FakeCredentials(Err(()))
    }
}

impl Credentials for FakeCredentials {
    fn bearer(&self, var: &str) -> Result<Secret, MissingCredential> {
        match self.0 {
            Ok(value) => Ok(value.into()),
            Err(()) => Err(MissingCredential {
                var: var.to_string(),
            }),
        }
    }
}
