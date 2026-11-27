//! Independent attacks on `ego resume --model <other provider>` account binding (#1193-6939).

use crate::support::{Ego, Places, Plan, Reply, Stub};

const SEED: &str = "00000000-0000-0000-0000-000000000901";
const KEY_ANTHROPIC_A: &str = "sk-ant-TestOnly0614";
const KEY_ANTHROPIC_B: &str = "sk-ant-TestOnly0615";
const AMBIENT_KEY: &str = "sk-ant-TestOnly0616";
const OPENAI_ONE: &str = "synthetic-openai-attack-one";
const OPENAI_TWO: &str = "synthetic-openai-attack-two";
const MODEL: &str = "openai/gpt-6.1-sol";

fn configure(places: &Places, openai_accounts: &[(&str, &str)]) {
    let mut text = String::from(
        "connection = \"account_a\"\nmodel = \"anthropic/claude-opus-5-5\"\nthinking = \"low\"\nsandbox = \"off\"\nnetwork = \"on\"\n\n[connections.account_a]\nname = \"Personal\"\nvendor = \"anthropic\"\naccess = \"key\"\nkey_env = \"ATTACK_A_KEY\"\n\n[connections.account_b]\nname = \"Work\"\nvendor = \"anthropic\"\naccess = \"key\"\nkey_env = \"ATTACK_B_KEY\"\n",
    );
    if !openai_accounts.is_empty() {
        text.push_str("\n[account_numbers.openai]\n");
        for (number, (id, _)) in openai_accounts.iter().enumerate() {
            text.push_str(&format!("{id} = {}\n", number + 1));
        }
    }
    for (id, env) in openai_accounts {
        text.push_str(&format!(
            "\n[connections.{id}]\nname = '{id}'\nvendor = 'openai'\naccess = 'key'\nkey_env = '{env}'\n"
        ));
    }
    std::fs::write(places.home_path().join("config.toml"), text).unwrap();
}

fn ego(places: &Places, stub: &Stub) -> Ego {
    Ego::at(places.home_path(), places.workspace_path(), Some(stub))
        .platform_sandbox()
        .env("ATTACK_A_KEY", KEY_ANTHROPIC_A)
        .env("ATTACK_B_KEY", KEY_ANTHROPIC_B)
        .env("ANTHROPIC_API_KEY", AMBIENT_KEY)
        .env("ATTACK_OPENAI_ONE_API_KEY", OPENAI_ONE)
        .env("ATTACK_OPENAI_TWO_API_KEY", OPENAI_TWO)
        .env("OPENAI_API_KEY", "")
}

fn seed(places: &Places, stub: &Stub, session: &str) {
    let seeded = ego(places, stub)
        .args([
            "--connection",
            "account_b",
            "--session-id",
            session,
            "First.",
        ])
        .run();
    assert!(
        seeded.stderr.contains("the provider returned HTTP 400"),
        "{}",
        seeded.stderr
    );
}

fn requests_to(stub: &Stub, suffix: &str) -> Vec<crate::support::Recorded> {
    stub.requests()
        .into_iter()
        .filter(|request| request.path.ends_with(suffix))
        .collect()
}

fn header_values(request: &crate::support::Recorded) -> String {
    request
        .headers
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Catches: the journal keeps the previous provider's account and model after a provider
/// switch, so the next plain resume reverts to the old model/account or crosses credentials.
#[test]
fn plain_resume_after_a_provider_switch_continues_on_the_new_provider_account() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400); 6],
        ..Plan::default()
    });
    configure(&places, &[("openai_one", "ATTACK_OPENAI_ONE_API_KEY")]);
    seed(&places, &stub, SEED);

    let switched = ego(&places, &stub)
        .subcommand("resume")
        .args([SEED, "--model", MODEL, "Switch."])
        .run();
    assert!(
        switched.stderr.contains("the provider returned HTTP 400"),
        "{}",
        switched.stderr
    );
    let plain = ego(&places, &stub)
        .subcommand("resume")
        .args([SEED, "Again."])
        .run();
    assert!(
        plain.stderr.contains("the provider returned HTTP 400"),
        "{}",
        plain.stderr
    );

    let anthropic = requests_to(&stub, "/messages");
    let openai = requests_to(&stub, "/responses");
    assert_eq!(
        anthropic.len(),
        1,
        "plain resume went back to anthropic: {:?}",
        stub.requests()
    );
    assert_eq!(openai.len(), 2, "{:?}", stub.requests());
    for request in &openai {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {OPENAI_ONE}").as_str())
        );
        let values = header_values(request);
        for forbidden in [KEY_ANTHROPIC_A, KEY_ANTHROPIC_B, AMBIENT_KEY] {
            assert!(
                !values.contains(forbidden),
                "anthropic credential crossed to openai"
            );
        }
    }
    assert!(!header_values(&anthropic[0]).contains(OPENAI_ONE));
}

/// Catches: the advertised `--account N` retry is rejected or ignored for a provider change,
/// or selects an account that is not the numbered one.
#[test]
fn the_account_retry_in_the_refusal_selects_the_numbered_account() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400); 8],
        ..Plan::default()
    });
    configure(
        &places,
        &[
            ("openai_one", "ATTACK_OPENAI_ONE_API_KEY"),
            ("openai_two", "ATTACK_OPENAI_TWO_API_KEY"),
        ],
    );
    let sessions = [
        "00000000-0000-0000-0000-000000000902",
        "00000000-0000-0000-0000-000000000903",
    ];
    for session in sessions {
        seed(&places, &stub, session);
    }
    let before = stub.requests().len();
    let refused = ego(&places, &stub)
        .subcommand("resume")
        .args([sessions[0], "--model", MODEL, "Retry."])
        .run();
    assert_eq!(refused.code, Some(2), "{}", refused.stderr);
    assert_eq!(stub.requests().len(), before, "refusal reached HTTP");

    let mut keys = Vec::new();
    for (session, number) in sessions.iter().zip(["1", "2"]) {
        let before = requests_to(&stub, "/responses").len();
        let run = ego(&places, &stub)
            .subcommand("resume")
            .args([session, "--model", MODEL, "--account", number, "Retry."])
            .run();
        assert!(
            run.stderr.contains("the provider returned HTTP 400"),
            "--account {number}: {}",
            run.stderr
        );
        let openai = requests_to(&stub, "/responses");
        assert_eq!(openai.len(), before + 1, "{:?}", stub.requests());
        keys.push(
            openai
                .last()
                .unwrap()
                .headers
                .get("authorization")
                .cloned()
                .unwrap(),
        );
    }
    keys.sort();
    assert_eq!(
        keys,
        vec![
            format!("Bearer {OPENAI_ONE}"),
            format!("Bearer {OPENAI_TWO}")
        ],
        "accounts 1 and 2 must select the two distinct openai accounts"
    );
}

/// Catches: a destination with no saved account of the new provider still carries the old
/// provider's account (or an ambient key of the old provider) to the new endpoint.
#[test]
fn provider_change_without_a_saved_destination_account_never_sends_the_old_key() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400); 4],
        ..Plan::default()
    });
    configure(&places, &[]);
    seed(&places, &stub, SEED);
    let run = ego(&places, &stub)
        .env("OPENAI_API_KEY", "synthetic-openai-env-key")
        .subcommand("resume")
        .args([SEED, "--model", MODEL, "Switch."])
        .run();
    let sent = requests_to(&stub, "/responses");
    assert_eq!(
        sent.len(),
        1,
        "the switch must reach openai: {}",
        run.stderr
    );
    assert_eq!(
        sent[0].headers.get("authorization").map(String::as_str),
        Some("Bearer synthetic-openai-env-key")
    );
    for request in requests_to(&stub, "/responses") {
        let values = header_values(&request);
        for forbidden in [KEY_ANTHROPIC_A, KEY_ANTHROPIC_B, AMBIENT_KEY] {
            assert!(!values.contains(forbidden), "{}", run.stderr);
        }
    }
    let plain = ego(&places, &stub)
        .env("OPENAI_API_KEY", "synthetic-openai-env-key")
        .subcommand("resume")
        .args([SEED, "Again."])
        .run();
    for request in requests_to(&stub, "/responses") {
        let values = header_values(&request);
        for forbidden in [KEY_ANTHROPIC_A, KEY_ANTHROPIC_B, AMBIENT_KEY] {
            assert!(!values.contains(forbidden), "{}", plain.stderr);
        }
    }
    assert_eq!(
        requests_to(&stub, "/messages").len(),
        1,
        "plain resume after the switch went back to the saved anthropic account: {}",
        plain.stderr
    );
}
