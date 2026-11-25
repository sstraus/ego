use ego_cli::cli::{self, Invocation};

fn parse(words: &[&str]) -> Result<Invocation, cli::UsageError> {
    cli::parse(words.iter().map(|word| (*word).to_owned()).collect())
}

// Catches: accepting the option for one command but dropping it for resume or interactive.
#[test]
fn explicit_proxy_is_preserved_for_run_resume_and_interactive() {
    for words in [
        vec!["ego", "run", "--proxy", "http://proxy.test:8888", "hello"],
        vec![
            "ego",
            "resume",
            "--proxy",
            "http://proxy.test:8888",
            "hello",
        ],
        vec!["ego", "--proxy", "http://proxy.test:8888", "hello"],
    ] {
        let args = match parse(&words).unwrap() {
            Invocation::Run(args) | Invocation::Resume(args) | Invocation::Interactive(args) => {
                args
            }
            other => panic!("unexpected invocation: {other:?}"),
        };
        assert_eq!(args.proxy, Some("http://proxy.test:8888".parse().unwrap()));
    }
}

// Catches: unsupported/authenticated endpoints, CA without proxy, and leaking proxy credentials in errors.
#[test]
fn explicit_proxy_rejects_invalid_endpoints_and_unscoped_ca() {
    for endpoint in [
        "socks5://host:8888",
        "https://host:8888",
        "http://user:secret-value@host:8888",
        "http://host:8888/path",
        "http://host:8888?secret-value",
        "http://host:8888#fragment",
        "broken",
    ] {
        let error = parse(&["ego", "run", "--proxy", endpoint, "hello"]).unwrap_err();
        assert!(error.to_string().contains("--proxy"), "{error}");
        assert!(!error.to_string().contains("secret-value"), "{error}");
    }
    assert!(parse(&["ego", "run", "--proxy-ca", "ca.pem", "hello"]).is_err());
    assert!(
        parse(&[
            "ego",
            "run",
            "--proxy",
            "http://host:8888",
            "--proxy-ca",
            "missing-ego-proxy-ca.pem",
            "hello"
        ])
        .unwrap_err()
        .to_string()
        .contains("could not read --proxy-ca")
    );
}

// Catches: ignoring --proxy-ca or accepting an arbitrary file as trust material.
#[test]
fn explicit_proxy_ca_is_validated_and_scoped_to_this_invocation() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        include_bytes!("../../../ego-tools/src/http/proxy/fixtures/ca.pem"),
    )
    .unwrap();
    let Invocation::Run(args) = parse(&[
        "ego",
        "run",
        "--proxy",
        "http://proxy.test:8888",
        "--proxy-ca",
        file.path().to_str().unwrap(),
        "hello",
    ])
    .unwrap() else {
        panic!("run")
    };
    assert_ne!(args.proxy, Some("http://proxy.test:8888".parse().unwrap()));
    for invalid in [
        b"not a certificate".as_slice(),
        b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".as_slice(),
    ] {
        std::fs::write(file.path(), invalid).unwrap();
        assert!(
            parse(&[
                "ego",
                "run",
                "--proxy",
                "http://proxy.test:8888",
                "--proxy-ca",
                file.path().to_str().unwrap(),
                "hello"
            ])
            .unwrap_err()
            .to_string()
            .contains("valid PEM certificate")
        );
    }
}

// Catches: loading an unbounded CA file before validating the certificate.
#[test]
fn explicit_proxy_ca_refuses_oversized_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len(65537).unwrap();
    let error = parse(&[
        "ego",
        "run",
        "--proxy",
        "http://proxy.test",
        "--proxy-ca",
        file.path().to_str().unwrap(),
        "hello",
    ])
    .unwrap_err();
    assert_eq!(error.to_string(), "--proxy-ca certificate exceeds 64 KiB");
}
