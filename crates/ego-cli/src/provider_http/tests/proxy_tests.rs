use super::*;

// Catches: invocation proxy configuration being lost when binding a fresh run effect scope.
#[tokio::test]
async fn explicit_proxy_source_binding_sends_connect_through_run_resolver() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        String::from_utf8(bytes).unwrap()
    });
    let resolver = RecordingResolver::default();
    let recorded = resolver.deadlines.clone();
    let ports = run_ports(resolver).await;
    let source = crate::model::CliGenerationModelSource::new(
        ego_core::config::ModelRef("openai/test".into()),
        None,
        None,
        ego_core::ceiling::Ceilings::default(),
        Default::default(),
    )
    .with_proxy(Some(format!("http://proxy.test:{port}").parse().unwrap()));
    let row = ProviderRow::openai_at(Url::parse("https://provider.test/v1/").unwrap());
    let call = capture_prepared(row).await;
    let error = source
        .provider_http(ports.ports.clone())
        .begin_call()
        .execute(call)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error.failure(), TransportFailure::Connect(_)),
        "{error}"
    );
    assert!(
        server
            .join()
            .unwrap()
            .starts_with("CONNECT provider.test:443 HTTP/1.1\r\n")
    );
    let recorded = recorded.lock().unwrap();
    assert_eq!(
        recorded
            .iter()
            .map(|(target, _)| target.host())
            .collect::<Vec<_>>(),
        ["provider.test", "proxy.test"]
    );
    assert_eq!(
        recorded[0].1, recorded[1].1,
        "one deadline includes both resolutions"
    );
}
