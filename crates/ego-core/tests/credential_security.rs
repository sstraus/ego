//! RED boundary tests for provider credential ownership.

use ego_core::credentials::SecretMaterial;
use ego_core::permit::HttpHeader;

const TOKEN: &[u8] = b"credential-bytes-must-not-be-formatted";

#[test]
fn secret_material_has_redacted_diagnostics_and_a_typed_secret_header() {
    let material = SecretMaterial::new(TOKEN.to_vec());
    assert!(!format!("{material:?}").contains("credential-bytes"));
    let header = HttpHeader::secret("authorization", material).unwrap();
    assert!(header.is_secret());
    assert!(!format!("{header:?}").contains("credential-bytes"));
}

#[test]
fn provider_credential_owner_can_be_moved_to_the_transport_without_a_string_escape() {
    let owner = ego_core::credentials::ProviderSecret::new(TOKEN.to_vec());
    let header = owner.into_secret_header("authorization").unwrap();
    assert!(header.is_secret());
    assert_eq!(header.value(), TOKEN);
}
