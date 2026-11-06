use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use url::Url;

use super::{Layer, probe};

const BUDGET: Duration = Duration::from_secs(5);

/// A server that reads one request, then answers with `reply` (`None` closes).
/// With `hang` it sleeps before answering.
fn serve(reply: Option<&'static str>, hang: bool) -> (Url, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        // The TCP layer's own connection arrives first and sends nothing.
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                continue;
            }
            let _ = sent.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
            if hang {
                std::thread::sleep(Duration::from_secs(10));
            }
            if let Some(reply) = reply {
                let _ = stream.write_all(reply.as_bytes());
            }
            return;
        }
    });
    (
        Url::parse(&format!("http://127.0.0.1:{port}/v1/")).unwrap(),
        received,
    )
}

#[test]
fn an_unauthenticated_refusal_still_proves_the_endpoint_answered() {
    let (url, request) = serve(
        Some("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
        false,
    );
    let reached = probe(&url, BUDGET).expect("a 401 is an answer");
    assert_eq!(reached.status, 401);
    let request = request.recv().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /v1/ "), "{request}");
    assert!(!request.contains("authorization"), "{request}");
    assert!(!request.contains("x-api-key"), "{request}");
}

#[test]
fn a_host_that_does_not_resolve_fails_at_dns() {
    let url = Url::parse("http://nonexistent.invalid/v1/").unwrap();
    assert_eq!(probe(&url, BUDGET).unwrap_err().layer, Layer::Dns);
}

#[test]
fn a_closed_port_fails_at_tcp() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    assert_eq!(probe(&url, BUDGET).unwrap_err().layer, Layer::Tcp);
}

#[test]
fn a_server_that_is_not_tls_fails_at_tls() {
    let (http, _) = serve(Some("HTTP/1.1 400 Bad Request\r\n\r\n"), false);
    let url = Url::parse(&http.as_str().replacen("http://", "https://", 1)).unwrap();
    assert_eq!(probe(&url, BUDGET).unwrap_err().layer, Layer::Tls);
}

#[test]
fn a_server_that_closes_without_answering_fails_at_http() {
    let (url, _) = serve(None, false);
    assert_eq!(probe(&url, BUDGET).unwrap_err().layer, Layer::Http);
}

#[test]
fn a_server_that_never_answers_is_cut_off_by_the_budget() {
    let (url, _) = serve(None, true);
    let started = Instant::now();
    let failure = probe(&url, Duration::from_millis(500)).unwrap_err();
    assert_eq!(failure.layer, Layer::Http);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}
