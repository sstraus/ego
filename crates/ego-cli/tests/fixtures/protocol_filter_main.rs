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
    let request: serde_json::Value = serde_json::from_slice(&input).unwrap();
    assert_eq!(request["version"], 1);
    assert_eq!(request["provider"], "anthropic");
    assert_eq!(request["model"], "claude-sonnet-5-5");
    if request["direction"] == "response" {
        let mut request = request;
        loop {
            assert_eq!(request["direction"], "response");
            match config["response_mode"].as_str() {
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
                        "version":1,"sequence":request["sequence"],"body_base64":request["body_base64"]
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
    assert_eq!(request["direction"], "request");
    assert_eq!(request.as_object().unwrap().len(), 9);
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
        "raw" => print!("{}", config["raw"].as_str().unwrap()),
        "oversize envelope" => {
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
            serde_json::json!({"version":1,"body_base64":request["body_base64"],"identity_headers":[],"expected_revision":request["state"]["revision"],"state_writes":{}})
        ),
        other => panic!("unknown fixture mode {other}"),
    }
}
