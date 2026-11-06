//! The inference-only authority uses provider HTTP without a run effect scope.

use std::io::{Read, Write};
use std::sync::Arc;

use ego_ai::http::Http;
use ego_core::permit::{CanonPath, Confinement, RunConfinement};
use ego_core::turn::CancelToken;
use ego_tools::exec::HttpCallTimeout;
use ego_tools::http::InferenceHttpPorts;

// Catches: setup inference reuses the ordinary scope and creates evidence or
// launches confinement probes before contacting a scripted provider.
#[test]
fn setup_inference_http_creates_no_files_or_process_scope() {
    let root = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        let bytes = socket.read(&mut buffer).unwrap();
        assert!(String::from_utf8_lossy(&buffer[..bytes]).starts_with("GET /api/tags "));
        let body = r#"{"models":[]}"#;
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let policy = RunConfinement::new(Confinement::Ro, false, CanonPath::new(root.path())).unwrap();
    let ports = Arc::new(InferenceHttpPorts::new(
        policy,
        CancelToken::new(),
        HttpCallTimeout::default(),
    ));
    let http: Arc<dyn Http> = Arc::new(ego_cli::provider_http::ProviderHttpAdapter::inference(
        ports,
    ));
    let row =
        ego_ai::provider::ProviderRow::ollama(format!("http://{address}/v1/").parse().unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(ego_ai::ollama::enumerate(&row, &http))
        .unwrap();
    assert!(report.models().is_empty());
    provider.join().unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
