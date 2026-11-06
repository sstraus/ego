//! Catches over-broad or under-broad environment secret inventory.
use std::ffi::OsString;

use ego_tools::secrets::from_vars;

fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(n, v)| ((*n).into(), (*v).into()))
        .collect()
}

// Catches: MY_KEYBOARD=it causes ordinary owner output containing "it" to be redacted.
#[test]
fn keyboard_and_short_environment_values_do_not_corrupt_output() {
    let values = from_vars(vars(&[
        ("MY_KEYBOARD", "it"),
        ("TOKENIZER", "ordinary-text"),
        ("API_KEY", "abc"),
        ("DB_PASSWORD", "1234567"),
    ]));
    let text = "it is ordinary-text abc 1234567";
    assert_eq!(values.mask(text), text);
}

// Catches: the minimum-length boundary moves, or segment matching drops real names.
#[test]
fn qualifying_names_and_boundary_length_are_masked_case_insensitively() {
    let values = from_vars(vars(&[
        ("api_key", "12345678"),
        ("GITHUB_TOKEN", "abcdefgh1"),
        ("client_secret_value", "zzzzzzzz2"),
        ("PASSWORD", "yyyyyyyy3"),
        ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
        ("GOOGLE_APPLICATION_CREDENTIALS", "/p/cred.json"),
    ]));
    for secret in [
        "12345678",
        "abcdefgh1",
        "zzzzzzzz2",
        "yyyyyyyy3",
        "AKIAEXAMPLE",
        "/p/cred.json",
    ] {
        assert_eq!(values.mask(secret), "[REDACTED]", "{secret}");
    }
    assert_eq!(
        from_vars(vars(&[("API_KEY", "1234567")])).mask("1234567"),
        "1234567"
    );
}

// Catches: a compromised tool splits a known qualifying secret across chunks and exposes it.
#[test]
fn known_api_key_is_masked_across_output_chunk_boundaries() {
    let values = from_vars(vars(&[("MY_API_KEY", "sk-live-0123456789")]));
    let input = b"a sk-live-0123456789 b";
    for split in 0..=input.len() {
        let mut stream = values.stream();
        let mut out = stream.push(&input[..split], false);
        out.extend(stream.push(&input[split..], true));
        assert_eq!(out, b"a [REDACTED] b", "split {split}");
    }
}

#[test]
fn credential_names_without_an_inner_underscore_stay_masked() {
    // Catches: exact segment equality drops PGPASSWORD (libpq), NGROK_AUTHTOKEN and
    // OPENAI_APIKEY, which the substring policy masked, so their values reach the model.
    for name in [
        "PGPASSWORD",
        "NGROK_AUTHTOKEN",
        "OPENAI_APIKEY",
        "app_clientsecret",
    ] {
        let values = from_vars(vars(&[(name, "long-enough-credential")]));
        assert_eq!(
            values.mask("x long-enough-credential y"),
            "x [REDACTED] y",
            "{name} must stay masked"
        );
    }
}
