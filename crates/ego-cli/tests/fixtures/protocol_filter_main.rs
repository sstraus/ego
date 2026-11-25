//! Offline child fixture. It never links an alternative production backend.
use std::io::{BufRead, Write};

fn main() {
    let executable = std::env::args_os().next().unwrap();
    assert_eq!(std::env::args_os().count(), 1);
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(&executable).with_extension("json")).unwrap(),
    )
    .unwrap();
    assert!(std::env::var_os("ANTHROPIC_API_KEY").is_none());
    assert!(std::env::var_os("UNRELATED_SECRET").is_none());
    let mut stdin = std::io::stdin().lock();
    let mut input = Vec::new();
    stdin.read_until(b'\n', &mut input).unwrap();
    let hello: serde_json::Value = serde_json::from_slice(&input).unwrap();
    if matches!(
        hello["direction"].as_str(),
        Some("mcp_call" | "hook_payload")
    ) {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        if let Some(path) = config["egress_capture"].as_str() {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{hello}").unwrap();
        }
        let mode = config["egress_mode"].as_str().unwrap_or("reject");
        if mode == "timeout" {
            std::thread::sleep(std::time::Duration::from_secs(10));
            return;
        }
        if mode == "invalid" {
            println!("not JSON");
            return;
        }
        let secret = hello["state"]["values"]["restored_secret"]
            .as_str()
            .unwrap_or("");
        let body = String::from_utf8(
            STANDARD
                .decode(hello["body_base64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        if mode == "reject" && !secret.is_empty() && body.contains(secret) {
            println!(
                "{}",
                serde_json::json!({"version":1,"reject":true,"notices":["secret blocked at external dispatch"]})
            );
        } else {
            let body = if mode == "transform" && !secret.is_empty() {
                body.replace(secret, "MASKED")
            } else {
                body
            };
            println!(
                "{}",
                serde_json::json!({"version":1,"body_base64":STANDARD.encode(body),"identity_headers":if mode == "headers" {serde_json::json!([{"name":"x-leak","value":"no"}])} else {serde_json::json!([])},"expected_revision":hello["state"]["revision"],"state_writes":if mode == "writes" {serde_json::json!({"new":"forbidden"})} else {serde_json::json!({})}})
            );
        }
        std::io::stdout().flush().unwrap();
        input.clear();
        assert_eq!(stdin.read_until(b'\n', &mut input).unwrap(), 0);
        return;
    }
    assert_eq!(hello["version"], 1);
    assert_eq!(hello["direction"], "hello");
    assert_eq!(hello["provider"], "anthropic");
    assert_eq!(hello["model"], "claude-sonnet-5-5");
    assert_eq!(hello.as_object().unwrap().len(), 10);
    assert_eq!(hello["wire_format"], "AnthropicMessages");
    assert_eq!(
        hello["generation_role"],
        config["generation_role"].as_str().unwrap_or("main")
    );
    if let Some(path) = config["hello_capture"].as_str() {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{hello}").unwrap();
    }
    if let Some(path) = config["hello_marker"].as_str() {
        std::fs::write(path, b"hello received").unwrap();
    }
    match config["hello_mode"].as_str() {
        Some("reject") => {
            println!(
                "{}",
                serde_json::json!({"version":1,"reject":true,"notices":config["notices"].as_array().cloned().unwrap_or_default()})
            );
            return;
        }
        Some("invalid") => {
            println!("{{\"version\":2}}");
            return;
        }
        Some("missing") => {
            println!("{{\"version\":1,\"request\":true}}");
            return;
        }
        Some("malformed") => {
            println!("not json");
            return;
        }
        Some("sleep") => {
            std::thread::sleep(std::time::Duration::from_millis(200));
            return;
        }
        Some("delayed-pass") => {
            std::thread::sleep(std::time::Duration::from_millis(1_200));
        }
        _ => {}
    }
    let request_interest = config["request_interest"].as_bool().unwrap_or(true);
    let response_interest = config["response_interest"].as_bool().unwrap_or(true);
    println!(
        "{}",
        serde_json::json!({"version":1,"request":request_interest,"response":response_interest,"notices":config["hello_notices"].as_array().cloned().unwrap_or_default()})
    );
    std::io::stdout().flush().unwrap();
    if !request_interest && !response_interest {
        assert_eq!(stdin.read_until(b'\n', &mut input).unwrap(), 0);
        return;
    }
    input.clear();
    stdin.read_until(b'\n', &mut input).unwrap();
    let request: serde_json::Value = serde_json::from_slice(&input).unwrap();
    assert_eq!(request["version"], 1);
    assert_eq!(request["provider"], "anthropic");
    assert_eq!(request["model"], "claude-sonnet-5-5");
    if request["direction"] == "response" {
        let mut request = request;
        loop {
            assert_eq!(request["direction"], "response");
            if let Some(path) = config["response_marker"].as_str() {
                std::fs::write(path, b"response frame received").unwrap();
            }
            match config["response_mode"].as_str() {
                Some("reject") => {
                    println!(
                        "{}",
                        serde_json::json!({"version":1,"reject":true,"sequence":request["sequence"],"notices":config["notices"].as_array().cloned().unwrap_or_default()})
                    );
                    return;
                }
                Some("fail") => std::process::exit(9),
                Some("bad_sequence") => println!(
                    "{}",
                    serde_json::json!({
                        "version":1,"sequence":999,"body_base64":request["body_base64"]
                    })
                ),
                _ => println!(
                    "{}",
                    serde_json::json!({
                        "version":1,"sequence":request["sequence"],"body_base64":request["body_base64"],"notices":config["response_notices"].as_array().cloned().unwrap_or_default()
                    })
                ),
            }
            std::io::stdout().flush().unwrap();
            if request["eof"] == true {
                break;
            }
            input.clear();
            if stdin.read_until(b'\n', &mut input).unwrap() == 0 {
                panic!("missing EOF");
            }
            request = serde_json::from_slice(&input).unwrap();
        }
        input.clear();
        assert_eq!(stdin.read_until(b'\n', &mut input).unwrap(), 0);
        return;
    }
    assert!(request_interest);
    assert_eq!(request["direction"], "request");
    assert_eq!(request.as_object().unwrap().len(), 10);
    for key in ["session_id", "generation_id", "attempt_id"] {
        assert!(request[key].as_str().is_some());
    }
    if let Some(path) = config["capture"].as_str() {
        std::fs::write(path, &input).unwrap();
    }
    if let Some(path) = config["marker"].as_str() {
        let mut marker = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut context = request.clone();
        context.as_object_mut().unwrap().remove("body_base64");
        // Keep concurrent fixture processes from interleaving JSON tokens.
        let record = format!("{context}\n");
        assert_eq!(marker.write(record.as_bytes()).unwrap(), record.len());
    }
    match config["mode"].as_str().unwrap() {
        "reject" => {
            println!(
                "{}",
                serde_json::json!({"version":1,"reject":true,"notices":config["notices"].as_array().cloned().unwrap_or_default()})
            );
            return;
        }
        "egress" => {
            print!(
                "{}",
                serde_json::json!({"version":1,"body_base64":request["body_base64"],"identity_headers":[],"expected_revision":request["state"]["revision"],"state_writes":{"restored_secret":config["secret"]}})
            );
        }
        "variables" => {
            let captured = request["state"]["values"]["first_attempt"]
                .as_str()
                .unwrap_or(request["attempt_id"].as_str().unwrap());
            let value = config["replace"].as_str().unwrap_or(captured);
            print!(
                "{}",
                serde_json::json!({
                    "version":1,
                    "body_base64":request["body_base64"],
                    "identity_headers":[{"name":"x-ego-filter","value":captured}],
                    "expected_revision":request["state"]["revision"],
                    "state_writes":{"first_attempt":value}
                })
            );
        }
        "active" | "sleep" => std::thread::sleep(std::time::Duration::from_secs(10)),
        "exit" => {
            eprintln!("private diagnostic");
            std::process::exit(9);
        }
        "early eof" => {
            eprintln!("private stderr sentinel");
            return;
        }
        "raw" => print!("{}", config["raw"].as_str().unwrap()),
        "oversize envelope" => {
            eprintln!("private stderr sentinel");
            print!("{}", " ".repeat(22_435_161));
            std::io::stdout()
                .write_all(br#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#)
                .unwrap();
        }
        "oversize decoded body" => {
            print!(
                "{}",
                serde_json::json!({"version":1,"body_base64":"eHh4".repeat(5_592_406),"identity_headers":[],"expected_revision":request["state"]["revision"],"state_writes":{}})
            );
        }
        "pass" => print!(
            "{}",
            serde_json::json!({"version":1,"body_base64":request["body_base64"],"identity_headers":[],"expected_revision":request["state"]["revision"],"state_writes":{},"notices":config["notices"].as_array().cloned().unwrap_or_default()})
        ),
        other => panic!("unknown fixture mode {other}"),
    }
    println!();
    std::io::stdout().flush().unwrap();
    if response_interest {
        input.clear();
        assert!(stdin.read_until(b'\n', &mut input).unwrap() > 0);
        let mut response: serde_json::Value = serde_json::from_slice(&input).unwrap();
        loop {
            assert_eq!(response["direction"], "response");
            if let Some(path) = config["response_marker"].as_str() {
                std::fs::write(path, b"response frame received").unwrap();
            }
            match config["response_mode"].as_str() {
                Some("reject") => {
                    println!(
                        "{}",
                        serde_json::json!({"version":1,"reject":true,"sequence":response["sequence"],"notices":config["notices"].as_array().cloned().unwrap_or_default()})
                    );
                    return;
                }
                Some("fail") => std::process::exit(9),
                Some("bad_sequence") => println!(
                    "{}",
                    serde_json::json!({"version":1,"sequence":999,"body_base64":response["body_base64"]})
                ),
                _ if config["mode"] == "egress" => {
                    use base64::{Engine as _, engine::general_purpose::STANDARD};
                    let body = STANDARD
                        .decode(response["body_base64"].as_str().unwrap())
                        .unwrap();
                    let original = String::from_utf8(body).unwrap();
                    let body =
                        original.replace("RESTORE_SECRET", config["secret"].as_str().unwrap());
                    if body != original
                        && let Some(path) = config["restored_marker"].as_str()
                    {
                        std::fs::write(path, config["secret"].as_str().unwrap()).unwrap();
                    }
                    println!(
                        "{}",
                        serde_json::json!({"version":1,"sequence":response["sequence"],"body_base64":STANDARD.encode(body)})
                    );
                }
                _ => println!(
                    "{}",
                    serde_json::json!({"version":1,"sequence":response["sequence"],"body_base64":response["body_base64"],"notices":config["response_notices"].as_array().cloned().unwrap_or_default()})
                ),
            }
            std::io::stdout().flush().unwrap();
            if response["eof"] == true {
                break;
            }
            input.clear();
            assert!(stdin.read_until(b'\n', &mut input).unwrap() > 0);
            response = serde_json::from_slice(&input).unwrap();
        }
    }
    input.clear();
    assert_eq!(stdin.read_until(b'\n', &mut input).unwrap(), 0);
}
