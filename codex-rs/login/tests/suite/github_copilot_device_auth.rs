#![allow(clippy::unwrap_used)]

use anyhow::Context;
use codex_login::GithubCopilotDeviceAuthOptions;
use codex_login::run_github_copilot_login_with_options;
use core_test_support::skip_if_no_network;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn test_options(server: &MockServer) -> GithubCopilotDeviceAuthOptions {
    GithubCopilotDeviceAuthOptions {
        client_id: "test-client-id".to_string(),
        device_code_url: format!("{}/login/device/code", server.uri()),
        access_token_url: format!("{}/login/oauth/access_token", server.uri()),
        user_agent: "codex/test".to_string(),
    }
}

async fn mock_device_code_success(server: &MockServer, interval: u64) {
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "device-code-123",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 120,
            "interval": interval
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn github_copilot_device_auth_succeeds_after_authorization_pending() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    mock_device_code_success(&server, 0).await;

    let poll_counter = Arc::new(AtomicUsize::new(0));
    let poll_counter_for_mock = poll_counter.clone();

    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(move |_: &Request| {
            let attempt = poll_counter_for_mock.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                ResponseTemplate::new(200).set_body_json(json!({
                    "error": "authorization_pending"
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "gho_test_copilot_token",
                    "token_type": "bearer",
                    "scope": "read:user"
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let token = run_github_copilot_login_with_options(test_options(&server))
        .await
        .context("github copilot login should succeed")?;
    assert_eq!(token, "gho_test_copilot_token");
    Ok(())
}

#[tokio::test]
async fn github_copilot_device_auth_returns_permission_denied_on_access_denied()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    mock_device_code_success(&server, 0).await;

    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": "access_denied",
            "error_description": "The user denied this request"
        })))
        .mount(&server)
        .await;

    let err = run_github_copilot_login_with_options(test_options(&server))
        .await
        .expect_err("access_denied should fail login");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        err.to_string().contains("access_denied"),
        "unexpected error message: {err:?}"
    );
    Ok(())
}

#[tokio::test]
async fn github_copilot_device_auth_surfaces_device_code_http_failure() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
        .mount(&server)
        .await;

    let err = run_github_copilot_login_with_options(test_options(&server))
        .await
        .expect_err("503 should fail login");
    assert!(
        err.to_string()
            .contains("GitHub device code request failed with status 503"),
        "unexpected error message: {err:?}"
    );
    Ok(())
}
