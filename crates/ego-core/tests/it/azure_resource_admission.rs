//! Attack table for the Azure resource boundary that a credential is bound to.

use ego_core::config::{ConnectionConfig, ProviderEndpoint};

fn admitted(url: &str) -> bool {
    url.parse::<ProviderEndpoint>()
        .is_ok_and(|endpoint| endpoint.is_azure_openai_v1())
}

// Catches: a suffix check without the dot, nested labels, another port, path or zone that
// would send one resource's bearer to a host the operator never named.
#[test]
fn only_a_single_label_public_azure_resource_v1_base_is_admitted() {
    for accepted in [
        "https://tenant.openai.azure.com/openai/v1/",
        "https://tenant.openai.azure.com/openai/v1",
        "https://TENANT.OpenAI.Azure.COM/openai/v1/",
        "https://tenant-1.services.ai.azure.com/openai/v1/",
    ] {
        assert!(admitted(accepted), "must admit {accepted}");
    }
    for refused in [
        "https://evilopenai.azure.com/openai/v1/",
        "https://openai.azure.com/openai/v1/",
        "https://a.b.openai.azure.com/openai/v1/",
        "https://-x.openai.azure.com/openai/v1/",
        "https://x-.openai.azure.com/openai/v1/",
        "https://x_y.openai.azure.com/openai/v1/",
        "https://x.openai.azure.com:8443/openai/v1/",
        "https://x.openai.azure.com/openai/v1/extra/",
        "https://x.openai.azure.com/openai/v1/%2e%2e/",
        "https://x.openai.azure.com/",
        "https://x.openai.azure.com.evil.test/openai/v1/",
        "https://x.openai.azure.us/openai/v1/",
        "https://x.openai.azure.com@evil.test/openai/v1/",
        "https://evil.test\\@x.openai.azure.com/openai/v1/",
        "http://x.openai.azure.com/openai/v1/",
    ] {
        assert!(!admitted(refused), "must refuse {refused}");
    }
}

fn connection(json: serde_json::Value) -> ConnectionConfig {
    serde_json::from_value(json).expect("fixture parses")
}

// Catches: an Azure account without a bound resource, a foreign vendor carrying the resource
// field, and a key account falling back to the shared auth-store slot.
#[test]
fn azure_connection_requires_its_resource_and_a_scoped_key_source() {
    let base = "https://tenant.openai.azure.com/openai/v1/";
    let ok = connection(serde_json::json!({
        "name": "A", "vendor": "azure-openai", "access": "oauth", "azure_resource": base
    }));
    assert!(ok.validate("a").is_ok());
    for bad in [
        serde_json::json!({"name": "A", "vendor": "azure-openai", "access": "oauth"}),
        serde_json::json!({"name": "A", "vendor": "azure-openai", "access": "key", "azure_resource": base}),
        serde_json::json!({"name": "A", "vendor": "azure-openai", "access": "oauth",
            "key_env": "TENANT_API_KEY", "azure_resource": base}),
        serde_json::json!({"name": "A", "vendor": "openai", "access": "oauth", "azure_resource": base}),
        serde_json::json!({"name": "A", "vendor": "azure-openai", "access": "oauth",
            "azure_resource": "https://x.openai.azure.com/v1/"}),
    ] {
        assert!(connection(bad.clone()).validate("a").is_err(), "{bad}");
    }
}
