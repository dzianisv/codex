use super::*;
use crate::codex::make_session_and_context;
use crate::config::test_config;
use crate::model_provider_info::GITHUB_COPILOT_PROVIDER_ID;
use crate::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use crate::models_manager::manager::RefreshStrategy;
use assert_matches::assert_matches;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelsResponse;
use core_test_support::responses::mount_models_once;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;
use tempfile::tempdir;
use wiremock::MockServer;

fn user_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        end_turn: None,
        phase: None,
    }
}
fn assistant_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        end_turn: None,
        phase: None,
    }
}

#[test]
fn drops_from_last_user_only() {
    let items = [
        user_msg("u1"),
        assistant_msg("a1"),
        assistant_msg("a2"),
        user_msg("u2"),
        assistant_msg("a3"),
        ResponseItem::Reasoning {
            id: "r1".to_string(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "s".to_string(),
            }],
            content: None,
            encrypted_content: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            call_id: "c1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
        },
        assistant_msg("a4"),
    ];

    let initial: Vec<RolloutItem> = items
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect();
    let truncated = truncate_before_nth_user_message(InitialHistory::Forked(initial), 1);
    let got_items = truncated.get_rollout_items();
    let expected_items = vec![
        RolloutItem::ResponseItem(items[0].clone()),
        RolloutItem::ResponseItem(items[1].clone()),
        RolloutItem::ResponseItem(items[2].clone()),
    ];
    assert_eq!(
        serde_json::to_value(&got_items).unwrap(),
        serde_json::to_value(&expected_items).unwrap()
    );

    let initial2: Vec<RolloutItem> = items
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect();
    let truncated2 = truncate_before_nth_user_message(InitialHistory::Forked(initial2), 2);
    assert_matches!(truncated2, InitialHistory::New);
}

#[tokio::test]
async fn ignores_session_prefix_messages_when_truncating() {
    let (session, turn_context) = make_session_and_context().await;
    let mut items = session.build_initial_context(&turn_context).await;
    items.push(user_msg("feature request"));
    items.push(assistant_msg("ack"));
    items.push(user_msg("second question"));
    items.push(assistant_msg("answer"));

    let rollout_items: Vec<RolloutItem> = items
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect();

    let truncated = truncate_before_nth_user_message(InitialHistory::Forked(rollout_items), 1);
    let got_items = truncated.get_rollout_items();

    let expected: Vec<RolloutItem> = vec![
        RolloutItem::ResponseItem(items[0].clone()),
        RolloutItem::ResponseItem(items[1].clone()),
        RolloutItem::ResponseItem(items[2].clone()),
        RolloutItem::ResponseItem(items[3].clone()),
    ];

    assert_eq!(
        serde_json::to_value(&got_items).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[tokio::test]
async fn shutdown_all_threads_bounded_submits_shutdown_to_every_thread() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config();
    config.codex_home = temp_dir.path().join("codex-home");
    config.cwd = config.codex_home.clone();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");

    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.clone(),
    );
    let thread_1 = manager
        .start_thread(config.clone())
        .await
        .expect("start first thread")
        .thread_id;
    let thread_2 = manager
        .start_thread(config)
        .await
        .expect("start second thread")
        .thread_id;

    let report = manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;

    let mut expected_completed = vec![thread_1, thread_2];
    expected_completed.sort_by_key(std::string::ToString::to_string);
    assert_eq!(report.completed, expected_completed);
    assert!(report.submit_failed.is_empty());
    assert!(report.timed_out.is_empty());
    assert!(manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn new_uses_configured_openai_provider_for_model_refresh() {
    let server = MockServer::start().await;
    let models_mock = mount_models_once(&server, ModelsResponse { models: vec![] }).await;

    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config();
    config.codex_home = temp_dir.path().join("codex-home");
    config.cwd = config.codex_home.clone();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    config.model_catalog = None;
    config
        .model_providers
        .get_mut("openai")
        .expect("openai provider should exist")
        .base_url = Some(server.uri());

    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let manager = ThreadManager::new(
        &config,
        auth_manager,
        SessionSource::Exec,
        CollaborationModesConfig::default(),
    );

    let _ = manager.list_models(RefreshStrategy::Online).await;
    assert_eq!(models_mock.requests().len(), 1);
}

#[tokio::test]
async fn new_uses_active_github_copilot_provider_for_model_refresh() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept /models connection");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let bytes_read = stream.read(&mut chunk).expect("read request bytes");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..bytes_read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&request).to_string();
        assert!(
            request.starts_with("GET /models?"),
            "expected GET /models request, got:\n{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer copilot-thread-token"),
            "expected bearer token in Authorization header, got:\n{request}"
        );

        let response_body = serde_json::json!({
            "data": [
                {"id": "copilot-thread-model"},
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write /models response");
    });

    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config();
    config.codex_home = temp_dir.path().join("codex-home");
    config.cwd = config.codex_home.clone();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    config.model_catalog = None;

    let mut provider = ModelProviderInfo::create_github_copilot_provider();
    provider.base_url = Some(format!("http://{addr}"));
    // Ensure auth comes from stored auth token, not process env.
    provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
    config.model_provider_id = GITHUB_COPILOT_PROVIDER_ID.to_string();
    config.model_provider = provider.clone();
    config
        .model_providers
        .insert(GITHUB_COPILOT_PROVIDER_ID.to_string(), provider);

    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-thread-token"));
    let manager = ThreadManager::new(
        &config,
        auth_manager,
        SessionSource::Exec,
        CollaborationModesConfig::default(),
    );

    let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
    server.join().expect("models server should complete");

    assert!(
        available
            .iter()
            .any(|preset| preset.model == "copilot-thread-model"),
        "expected Copilot provider refresh to populate the selected model"
    );
}

#[tokio::test]
async fn reload_mcp_servers_submits_reload_op_to_each_thread() {
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        crate::built_in_model_providers(None)["openai"].clone(),
    );
    let config = crate::config::test_config();
    let new_thread = manager
        .start_thread(config)
        .await
        .expect("start test thread");

    manager.reload_mcp_servers().await;

    assert_eq!(
        manager.captured_ops(),
        vec![(new_thread.thread_id, Op::ReloadMcpServers)]
    );
}
