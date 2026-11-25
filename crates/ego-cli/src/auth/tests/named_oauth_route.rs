use super::{UnexpectedHttp, account_entry};
use crate::auth::{
    AuthEntry, AuthStore, CredentialCommandFailure, CredentialCommandPort,
    CredentialCommandRequest, StoredSecret, connection_credentials,
};
use ego_ai::provider::CredentialStrategy;
use ego_core::{config::ConnectionConfig, turn::CancelToken};
use std::{path::PathBuf, time::Duration};

fn isolated_home(test: &str) -> Option<PathBuf> {
    let (_, test) = test.split_once("::").unwrap();
    const CHILD: &str = "EGO_NAMED_OAUTH_ROUTE_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return Some(crate::auth::ego_home().unwrap());
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[connections.work]\nname='Work'\nvendor='anthropic'\naccess='oauth'\n\
         [auth.anthropic]\ncredential_command='route-program'\ncredential_timeout_secs=19\n\
         [auth.selection]\ncredential_command='selection-program'\ncredential_timeout_secs=27\n",
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env("EGO_HOME", home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

fn account(home: &std::path::Path) -> ConnectionConfig {
    crate::auth::connection_layer_at(home).unwrap().connections["work"].clone()
}

struct RouteRefresh;

impl CredentialCommandPort for RouteRefresh {
    fn run(
        &self,
        request: CredentialCommandRequest,
    ) -> futures::future::BoxFuture<'_, Result<StoredSecret, CredentialCommandFailure>> {
        Box::pin(async move {
            assert_eq!(request.argv, ["route-program", "refresh"]);
            assert_eq!(request.timeout, Duration::from_secs(19));
            let input: serde_json::Value =
                serde_json::from_slice(request.stdin.unwrap().expose_secret_bytes()).unwrap();
            assert_eq!(input["refresh"], "work-refresh");
            Ok(StoredSecret::from(
                r#"{"access":"work-updated","expires":4102444800000}"#,
            ))
        })
    }
}

// Catches: a selection-owned program receives the captured OAuth account's refresh token.
#[tokio::test]
async fn oauth_connection_refresh_source_follows_connection_route_not_selection_id() {
    let Some(home) = isolated_home(concat!(
        module_path!(),
        "::oauth_connection_refresh_source_follows_connection_route_not_selection_id"
    )) else {
        return;
    };
    let store = AuthStore::new(&home);
    store
        .put(
            "connection:work",
            AuthEntry::OAuth {
                access: StoredSecret::from("work-expired"),
                refresh: Some(StoredSecret::from("work-refresh")),
                expires: Some(1),
                scope_binding: None,
            },
        )
        .unwrap();
    let strategy = CredentialStrategy::OAuth {
        provider: "anthropic".into(),
    };
    let credentials = connection_credentials(
        "work",
        &account(&home),
        "selection",
        &strategy,
        &crate::config::auth().unwrap(),
        None,
        &UnexpectedHttp,
        &RouteRefresh,
        &CancelToken::new(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        credentials
            .secret("oauth:anthropic")
            .unwrap()
            .expose_secret_bytes(),
        b"work-updated"
    );
    assert_eq!(
        store
            .read_secret_material("connection:work")
            .unwrap()
            .expose_secret_bytes(),
        b"work-updated"
    );
}

// Catches: a mismatched presentation strategy reaches named OAuth preparation.
#[tokio::test]
async fn oauth_connection_rejects_strategy_provider_not_matching_connection_route() {
    let Some(home) = isolated_home(concat!(
        module_path!(),
        "::oauth_connection_rejects_strategy_provider_not_matching_connection_route"
    )) else {
        return;
    };
    let store = AuthStore::new(&home);
    store
        .put("connection:work", account_entry("work-access"))
        .unwrap();
    let strategy = CredentialStrategy::OAuth {
        provider: "other".into(),
    };
    let result = connection_credentials(
        "work",
        &account(&home),
        "anthropic",
        &strategy,
        &crate::config::auth().unwrap(),
        None,
        &UnexpectedHttp,
        &RouteRefresh,
        &CancelToken::new(),
        None,
    )
    .await;
    let Err(error) = result else {
        panic!("a mismatched OAuth strategy was admitted");
    };
    assert!(
        error.to_string().contains("requires OAuth route anthropic"),
        "{error}"
    );
}

// Catches: a hand-edited API key is presented as a named OAuth bearer.
#[tokio::test]
async fn named_oauth_connection_refuses_api_key_shaped_entry() {
    let Some(home) = isolated_home(concat!(
        module_path!(),
        "::named_oauth_connection_refuses_api_key_shaped_entry"
    )) else {
        return;
    };
    let store = AuthStore::new(&home);
    store
        .put(
            "connection:work",
            AuthEntry::ApiKey {
                key: StoredSecret::from("work-api-key"),
            },
        )
        .unwrap();
    let strategy = CredentialStrategy::OAuth {
        provider: "anthropic".into(),
    };
    let credentials = connection_credentials(
        "work",
        &account(&home),
        "anthropic",
        &strategy,
        &crate::config::auth().unwrap(),
        None,
        &UnexpectedHttp,
        &RouteRefresh,
        &CancelToken::new(),
        None,
    )
    .await
    .unwrap();
    assert!(strategy.resolve(credentials.as_ref()).is_err());
    assert_eq!(
        store
            .read_secret_material("connection:work")
            .unwrap()
            .expose_secret_bytes(),
        b"work-api-key"
    );
}
