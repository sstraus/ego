//! Lowercase hex SHA-256, in one place.
//!
//! Extracted rather than written a third time. `config/discovery.rs` hashes
//! an instruction file and `store::pricing` hashes a fetched price table, and
//! two private copies of the same six lines are two places for the encoding
//! to drift — an uppercase digest and a lowercase one are different strings
//! naming the same bytes, and both of these values end up in a filename or a
//! recorded revision where the spelling is the identity.
//!
//! `sha2` and not a second hash crate, for the reason
//! [`RuleId`](crate::permit::rule::RuleId) gives: nothing is *authorised* by
//! one of these, so the properties that would justify another dependency buy
//! nothing, and `sha2` is already here.

use sha2::{Digest, Sha256};

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published vector for the empty input, so a dependency bump that
    /// changed the algorithm could not pass.
    #[test]
    fn the_empty_digest_is_the_published_one() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Lowercase, always: the spelling is the identity wherever this is used.
    #[test]
    fn the_spelling_is_lowercase_hex_and_sixty_four_characters() {
        let digest = sha256_hex(b"ego");
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
            "{digest}"
        );
    }
}
