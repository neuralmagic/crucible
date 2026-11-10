//! The client against the router a crucible-kueue deployment serves, with a kube client whose API
//! server never answers: a call the server refuses fails with its refusal, one it lets through
//! fails only when it reaches Kubernetes.

mod common;

use common::{serve, task_token};
use crucible_kueue_client::{Client, Error, Token};
use serde_json::{Map, Value, json};
use std::time::Duration;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_carries_the_token_the_file_holds_at_that_moment() {
    let url = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kueue");
    std::fs::write(&file, "not-a-token").unwrap();
    let client = Client::new(&url, Token::File(file.clone())).unwrap();

    let refused = client.call("whoami", Map::new()).await.unwrap_err();
    assert!(
        matches!(refused, Error::Refused { status: 401, .. }),
        "{refused}"
    );

    std::fs::write(&file, format!("{}\n", task_token())).unwrap();
    let who = client.call("whoami", Map::new()).await.unwrap();
    assert_eq!(who["status"], "ok");
    assert_eq!(who["kube_user"], "crucible:run:crucible-run-brave-otter");
    assert_eq!(who["launched_by"], "alice");
    assert_eq!(who["namespaces"], json!(["team-e2e"]));

    std::fs::remove_file(&file).unwrap();
    assert!(matches!(
        client.call("whoami", Map::new()).await,
        Err(Error::Token { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_refusal_and_a_kubernetes_failure_are_tool_errors() {
    let url = serve().await;
    let client = Client::new(&url, Token::Static(task_token())).unwrap();

    let elsewhere = client
        .call(
            "queue_describe",
            args(json!({"namespace": "team-b", "queue": "gpu"})),
        )
        .await
        .unwrap_err();
    let Error::Tool {
        status, retryable, ..
    } = &elsewhere
    else {
        panic!("{elsewhere}");
    };
    assert_eq!((status.as_str(), *retryable), ("refused", false));
    assert!(
        elsewhere.to_string().contains("access check queue-get"),
        "{elsewhere}"
    );

    let unreachable = client
        .call("queue_describe", args(json!({"queue": "gpu"})))
        .await
        .unwrap_err();
    let Error::Tool {
        status, retryable, ..
    } = &unreachable
    else {
        panic!("{unreachable}");
    };
    assert_eq!((status.as_str(), *retryable), ("error", true));

    let waited = client
        .wait("Job", "train", "finished", None, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&waited, Error::Tool { status, retryable: true, .. } if status == "error"),
        "{waited}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_the_server_does_not_declare_is_refused() {
    let url = serve().await;
    let client = Client::new(&url, Token::Static(task_token())).unwrap();
    let unknown = client.call("no_such_tool", Map::new()).await.unwrap_err();
    let Error::Tool {
        status, retryable, ..
    } = &unknown
    else {
        panic!("{unknown}");
    };
    assert_eq!((status.as_str(), *retryable), ("refused", false));
}
