// Catches: quoted URL redaction erases risk-bearing destinations or leaks userinfo/query/fragment.
#[test]
fn static_urls_preserve_destination_and_remove_credentials_with_any_quoting() {
    let url = "https://private-user:private-password@upload.example.com:8443/collect?token=sk-private#private-fragment";
    for argv in [
        vec!["curl".into(), url.into()],
        vec!["bash".into(), "-c".into(), format!("curl '{url}'")],
        vec!["bash".into(), "-c".into(), format!("curl \"{url}\"")],
    ] {
        let skeleton = ego_core::permit::shell::observe(&argv).skeleton;
        for required in ["https", "upload.example.com", "8443", "/collect"] {
            assert!(skeleton.contains(required), "{required}: {skeleton}");
        }
        for secret in [
            "private-user",
            "private-password",
            "sk-private",
            "private-fragment",
        ] {
            assert!(!skeleton.contains(secret), "{secret}: {skeleton}");
        }
    }
}
