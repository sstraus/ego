//! The bearer credential every A2A request must carry, checked before routing.
//!
//! A2A puts authentication on the client and validation on the server: the
//! Agent Card declares the scheme, and a request that does not satisfy it is
//! answered `401` without the server saying anything further about what was
//! expected. There is no anonymous mode here — an unauthenticated A2A listener
//! is a remote-run facility for whoever finds the port, and a default that
//! insecure is not a default, it is a defect waiting for an operator who did
//! not read the flag list.

use std::fmt;

use ego_core::credentials::SecretMaterial;

/// The `Authorization` scheme ego declares and enforces.
///
/// Lowercase because header values are compared case-insensitively on the
/// scheme token, and this is the spelling used for the comparison.
pub const BEARER: &str = "bearer";

/// The variable the token is read from.
///
/// An environment variable rather than a flag, because a token on a command
/// line is in the process table and in the operator's shell history, and this
/// one is the whole of the authority to run code in their workspace.
pub const TOKEN_VAR: &str = "EGO_A2A_TOKEN";

/// The token this listener accepts.
///
/// Held as `SecretMaterial` so it is zeroized on drop and cannot be rendered
/// into a diagnostic by accident — the same owner `ego-cli`'s credential store
/// uses.
pub struct Credential {
    token: SecretMaterial,
}

/// `Debug` is implemented by hand and prints nothing of the token: a listener
/// is the sort of value that ends up in a `tracing` field or a panic message.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential").finish_non_exhaustive()
    }
}

/// Why no credential could be established at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRefusal {
    Unset,
    Empty,
}

impl fmt::Display for CredentialRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unset => write!(
                f,
                "'ego a2a' needs a secret token to check who can connect; set \
                 {TOKEN_VAR} to a secret shared only with trusted callers"
            ),
            Self::Empty => write!(
                f,
                "{TOKEN_VAR} is empty, so the server cannot check who can connect; \
                 set it to a secret shared only with trusted callers"
            ),
        }
    }
}

impl std::error::Error for CredentialRefusal {}

impl Credential {
    /// Read the token from the environment, refusing to serve without one.
    ///
    /// The empty case is separate from the unset one because they are different
    /// operator mistakes with the same consequence: `EGO_A2A_TOKEN=` in a unit
    /// file reads as "configured" to the person who wrote it.
    pub fn from_env(read: impl FnOnce(&str) -> Option<String>) -> Result<Self, CredentialRefusal> {
        let token = read(TOKEN_VAR).ok_or(CredentialRefusal::Unset)?;
        if token.is_empty() {
            return Err(CredentialRefusal::Empty);
        }
        Ok(Self {
            token: SecretMaterial::new(token.into_bytes()),
        })
    }

    /// Whether an `Authorization` header value presents this token.
    ///
    /// The scheme is matched case-insensitively, as HTTP requires. The token
    /// itself is compared in constant time: a byte-by-byte early return leaks
    /// the length of the shared prefix, and this endpoint can be asked the same
    /// question as often as an attacker likes.
    pub fn admits(&self, authorization: &[u8]) -> bool {
        let Some(presented) = strip_bearer(authorization) else {
            return false;
        };
        constant_time_eq(presented, self.token.expose_secret_bytes())
    }
}

/// The token bytes of a `Bearer <token>` header value, if that is its shape.
fn strip_bearer(authorization: &[u8]) -> Option<&[u8]> {
    let (scheme, rest) = split_first_space(authorization)?;
    if !scheme.eq_ignore_ascii_case(BEARER.as_bytes()) {
        return None;
    }
    // One optional run of spaces after the scheme, per RFC 9110's credentials
    // grammar. Trailing whitespace is *not* trimmed: it is part of the token as
    // far as this comparison is concerned, so a value that differs only by a
    // trailing space is a different value rather than a silent match.
    Some(trim_leading_spaces(rest))
}

fn split_first_space(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == b' ')?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

fn trim_leading_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// Compare without a data-dependent exit, mirroring
/// `ego_core::suspension::constant_time_eq`.
///
/// The length check ahead of it is not a leak worth avoiding: a bearer token's
/// length is not the secret, and comparing to a fixed width would require
/// choosing one.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(token: &str) -> Credential {
        Credential::from_env(|_| Some(token.to_string())).expect("a non-empty token is admissible")
    }

    #[test]
    fn a_listener_refuses_to_start_without_a_token_and_says_which_variable() {
        let unset = Credential::from_env(|_| None).expect_err("an absent token was accepted");
        assert_eq!(unset, CredentialRefusal::Unset);
        assert!(unset.to_string().contains(TOKEN_VAR));

        let empty =
            Credential::from_env(|_| Some(String::new())).expect_err("an empty token was accepted");
        assert_eq!(empty, CredentialRefusal::Empty);
        assert!(empty.to_string().contains(TOKEN_VAR));
    }

    #[test]
    fn the_matching_token_is_admitted_in_either_spelling_of_the_scheme() {
        let credential = credential("s3cret-token");
        for header in [
            "Bearer s3cret-token",
            "bearer s3cret-token",
            "BEARER s3cret-token",
            "Bearer   s3cret-token",
        ] {
            assert!(credential.admits(header.as_bytes()), "{header}");
        }
    }

    #[test]
    fn everything_that_is_not_exactly_the_token_is_refused() {
        let credential = credential("s3cret-token");
        for header in [
            "Bearer s3cret-toke",
            "Bearer s3cret-tokenn",
            "Bearer s3cret-token ",
            "Bearer S3CRET-TOKEN",
            "Bearer ",
            "Bearer",
            "Basic s3cret-token",
            "s3cret-token",
            "",
        ] {
            assert!(
                !credential.admits(header.as_bytes()),
                "admitted {header:?}, which is not the token"
            );
        }
    }

    /// The token must not be recoverable from a diagnostic, which is where a
    /// listener value is most likely to end up.
    #[test]
    fn the_debug_rendering_carries_nothing_of_the_token() {
        let rendered = format!("{:?}", credential("s3cret-token"));
        assert!(!rendered.contains("s3cret-token"), "{rendered}");
    }

    #[test]
    fn a_length_mismatch_is_refused_without_indexing_past_either_end() {
        assert!(!constant_time_eq(b"short", b"much longer value"));
        assert!(!constant_time_eq(b"much longer value", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"equal", b"equal"));
    }
}
