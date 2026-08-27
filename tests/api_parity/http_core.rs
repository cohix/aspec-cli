//! Part 0.5 generic HTTP transport tests.

use std::time::Duration;

use awman::command::commands::HttpCore;
use awman::command::error::CommandError;
use awman::engine::auth::ApiKey;

#[tokio::test]
async fn http_core_trims_base_url_honours_prefix_and_applies_bearer_header() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/custom/status"))
        .and(matchers::header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;

    let key = ApiKey::from_string("test-key");
    let core = HttpCore::new(&format!("{}/", server.uri()), "custom", Some(&key)).unwrap();
    assert_eq!(core.base_url(), server.uri());
    assert_eq!(core.prefix(), "custom");
    assert_eq!(
        core.url(&["status"]),
        format!("{}/custom/status", server.uri())
    );

    let response = core.get(&["status"]).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body["ok"], true);
}

#[tokio::test]
async fn http_core_maps_status_errors_and_tolerates_non_json_delete_body() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v9/commands"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "invalid condition"
        })))
        .mount(&server)
        .await;
    Mock::given(matchers::method("DELETE"))
        .and(matchers::path("/v9/condition"))
        .respond_with(ResponseTemplate::new(204).set_body_string("deleted"))
        .mount(&server)
        .await;

    let core = HttpCore::new(&server.uri(), "v9", None).unwrap();
    let status = core
        .post_command(
            &["commands"],
            &[("name", serde_json::json!("condition"))],
            &[("x-test", "yes")],
        )
        .await
        .expect_err("HTTP status >= 400 must be surfaced");
    match status {
        CommandError::RemoteHttpStatus { status: 422, body } => {
            assert!(body.contains("invalid condition"));
        }
        other => panic!("expected RemoteHttpStatus, got {other:?}"),
    }

    let deleted = core.delete(&["condition"]).await.unwrap();
    assert_eq!(deleted.status, 204);
    assert_eq!(deleted.body, serde_json::json!({}));
}

#[tokio::test]
async fn http_core_classifies_connect_and_timeout_errors_like_remote_client() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unused = listener.local_addr().unwrap();
    drop(listener);
    let core = HttpCore::new(&format!("http://{unused}"), "v1", None).unwrap();
    let connect_error = core.get(&["status"]).await.expect_err("port is unused");
    assert!(
        matches!(connect_error, CommandError::RemoteConnectionRefused(_)),
        "expected connection classification, got {connect_error:?}"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _accepted = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let short_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1))
        .build()
        .unwrap();
    let timeout = short_client
        .get(format!("http://{addr}"))
        .send()
        .await
        .expect_err("the deliberately short timeout must expire");
    assert!(
        matches!(
            HttpCore::map_reqwest_error(timeout),
            CommandError::RemoteTimeout
        ),
        "timeout must map to RemoteTimeout"
    );
    server.await.unwrap();
}

#[test]
fn http_core_exposes_remote_client_timeout_contract() {
    assert_eq!(HttpCore::CONNECT_TIMEOUT, Duration::from_secs(10));
    assert_eq!(HttpCore::READ_TIMEOUT, Duration::from_secs(600));
}
