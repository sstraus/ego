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

/// Whether `value` is the one canonical spelling of a SHA-256 digest.
pub(crate) fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// HMAC-SHA256 for suspension authentication and provider-domain routing.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
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

    #[test]
    fn hmac_domain_routing_keeps_the_rfc4231_sha256_vector() {
        // Catches: moving suspension's HMAC primitive into the digest module changes its bytes.
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        let hex: String = mac.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
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
