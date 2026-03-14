use super::*;
use crate::CodexAuth;
use crate::auth::AuthCredentialsStoreMode;
use crate::config::ConfigBuilder;
use crate::model_provider_info::WireApi;
use chrono::Utc;
use codex_protocol::openai_models::ModelsResponse;
use core_test_support::responses::mount_models_once;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use tempfile::tempdir;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn remote_model(slug: &str, display: &str, priority: i32) -> ModelInfo {
    remote_model_with_visibility(slug, display, priority, "list")
}

fn remote_model_with_visibility(
    slug: &str,
    display: &str,
    priority: i32,
    visibility: &str,
) -> ModelInfo {
    serde_json::from_value(json!({
            "slug": slug,
            "display_name": display,
            "description": format!("{display} desc"),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}],
            "shell_type": "shell_command",
            "visibility": visibility,
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": priority,
            "upgrade": null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10_000},
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": 272_000,
            "experimental_supported_tools": [],
        }))
        .expect("valid model")
}

fn assert_models_contain(actual: &[ModelInfo], expected: &[ModelInfo]) {
    for model in expected {
        assert!(
            actual.iter().any(|candidate| candidate.slug == model.slug),
            "expected model {} in cached list",
            model.slug
        );
    }
}

fn provider_for(base_url: String) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "OpenAI".into(),
        base_url: Some(base_url),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    }
}

#[tokio::test]
async fn get_model_info_tracks_fallback_usage() {
    let codex_home = tempdir().expect("temp dir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load default test config");
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let manager = ModelsManager::new(
        codex_home.path().to_path_buf(),
        auth_manager,
        None,
        CollaborationModesConfig::default(),
    );
    let known_slug = manager
        .get_remote_models()
        .await
        .first()
        .expect("bundled models should include at least one model")
        .slug
        .clone();

    let known = manager.get_model_info(known_slug.as_str(), &config).await;
    assert!(!known.used_fallback_model_metadata);
    assert_eq!(known.slug, known_slug);

    let unknown = manager
        .get_model_info("model-that-does-not-exist", &config)
        .await;
    assert!(unknown.used_fallback_model_metadata);
    assert_eq!(unknown.slug, "model-that-does-not-exist");
}

#[tokio::test]
async fn get_model_info_uses_custom_catalog() {
    let codex_home = tempdir().expect("temp dir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load default test config");
    let mut overlay = remote_model("gpt-overlay", "Overlay", 0);
    overlay.supports_image_detail_original = true;

    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let manager = ModelsManager::new(
        codex_home.path().to_path_buf(),
        auth_manager,
        Some(ModelsResponse {
            models: vec![overlay],
        }),
        CollaborationModesConfig::default(),
    );

    let model_info = manager
        .get_model_info("gpt-overlay-experiment", &config)
        .await;

    assert_eq!(model_info.slug, "gpt-overlay-experiment");
    assert_eq!(model_info.display_name, "Overlay");
    assert_eq!(model_info.context_window, Some(272_000));
    assert!(model_info.supports_image_detail_original);
    assert!(!model_info.supports_parallel_tool_calls);
    assert!(!model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn get_model_info_matches_namespaced_suffix() {
    let codex_home = tempdir().expect("temp dir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load default test config");
    let mut remote = remote_model("gpt-image", "Image", 0);
    remote.supports_image_detail_original = true;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let manager = ModelsManager::new(
        codex_home.path().to_path_buf(),
        auth_manager,
        Some(ModelsResponse {
            models: vec![remote],
        }),
        CollaborationModesConfig::default(),
    );
    let namespaced_model = "custom/gpt-image".to_string();

    let model_info = manager.get_model_info(&namespaced_model, &config).await;

    assert_eq!(model_info.slug, namespaced_model);
    assert!(model_info.supports_image_detail_original);
    assert!(!model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn get_model_info_rejects_multi_segment_namespace_suffix_matching() {
    let codex_home = tempdir().expect("temp dir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load default test config");
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let manager = ModelsManager::new(
        codex_home.path().to_path_buf(),
        auth_manager,
        None,
        CollaborationModesConfig::default(),
    );
    let known_slug = manager
        .get_remote_models()
        .await
        .first()
        .expect("bundled models should include at least one model")
        .slug
        .clone();
    let namespaced_model = format!("ns1/ns2/{known_slug}");

    let model_info = manager.get_model_info(&namespaced_model, &config).await;

    assert_eq!(model_info.slug, namespaced_model);
    assert!(model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn refresh_available_models_sorts_by_priority() {
    let server = MockServer::start().await;
    let remote_models = vec![
        remote_model("priority-low", "Low", 1),
        remote_model("priority-high", "High", 0),
    ];
    let models_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: remote_models.clone(),
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let provider = provider_for(server.uri());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("refresh succeeds");
    let cached_remote = manager.get_remote_models().await;
    assert_models_contain(&cached_remote, &remote_models);

    let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
    let high_idx = available
        .iter()
        .position(|model| model.model == "priority-high")
        .expect("priority-high should be listed");
    let low_idx = available
        .iter()
        .position(|model| model.model == "priority-low")
        .expect("priority-low should be listed");
    assert!(
        high_idx < low_idx,
        "higher priority should be listed before lower priority"
    );
    assert_eq!(
        models_mock.requests().len(),
        1,
        "expected a single /models request"
    );
}

#[tokio::test]
async fn refresh_available_models_uses_cache_when_fresh() {
    let server = MockServer::start().await;
    let remote_models = vec![remote_model("cached", "Cached", 5)];
    let models_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: remote_models.clone(),
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let provider = provider_for(server.uri());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("first refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &remote_models);

    // Second call should read from cache and avoid the network.
    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("cached refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &remote_models);
    assert_eq!(
        models_mock.requests().len(),
        1,
        "cache hit should avoid a second /models request"
    );
}

#[tokio::test]
async fn refresh_available_models_refetches_when_cache_stale() {
    let server = MockServer::start().await;
    let initial_models = vec![remote_model("stale", "Stale", 1)];
    let initial_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: initial_models.clone(),
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let provider = provider_for(server.uri());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("initial refresh succeeds");

    // Rewrite cache with an old timestamp so it is treated as stale.
    manager
        .cache_manager
        .manipulate_cache_for_test(|fetched_at| {
            *fetched_at = Utc::now() - chrono::Duration::hours(1);
        })
        .await
        .expect("cache manipulation succeeds");

    let updated_models = vec![remote_model("fresh", "Fresh", 9)];
    server.reset().await;
    let refreshed_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: updated_models.clone(),
        },
    )
    .await;

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("second refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &updated_models);
    assert_eq!(
        initial_mock.requests().len(),
        1,
        "initial refresh should only hit /models once"
    );
    assert_eq!(
        refreshed_mock.requests().len(),
        1,
        "stale cache refresh should fetch /models once"
    );
}

#[tokio::test]
async fn refresh_available_models_refetches_when_version_mismatch() {
    let server = MockServer::start().await;
    let initial_models = vec![remote_model("old", "Old", 1)];
    let initial_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: initial_models.clone(),
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let provider = provider_for(server.uri());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("initial refresh succeeds");

    manager
        .cache_manager
        .mutate_cache_for_test(|cache| {
            let client_version = crate::models_manager::client_version_to_whole();
            cache.client_version = Some(format!("{client_version}-mismatch"));
        })
        .await
        .expect("cache mutation succeeds");

    let updated_models = vec![remote_model("new", "New", 2)];
    server.reset().await;
    let refreshed_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: updated_models.clone(),
        },
    )
    .await;

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("second refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &updated_models);
    assert_eq!(
        initial_mock.requests().len(),
        1,
        "initial refresh should only hit /models once"
    );
    assert_eq!(
        refreshed_mock.requests().len(),
        1,
        "version mismatch should fetch /models once"
    );
}

#[tokio::test]
async fn refresh_available_models_drops_removed_remote_models() {
    let server = MockServer::start().await;
    let initial_models = vec![remote_model("remote-old", "Remote Old", 1)];
    let initial_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: initial_models,
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let provider = provider_for(server.uri());
    let mut manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );
    manager.cache_manager.set_ttl(Duration::ZERO);

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("initial refresh succeeds");

    server.reset().await;
    let refreshed_models = vec![remote_model("remote-new", "Remote New", 1)];
    let refreshed_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: refreshed_models,
        },
    )
    .await;

    manager
        .refresh_available_models(RefreshStrategy::OnlineIfUncached)
        .await
        .expect("second refresh succeeds");

    let available = manager
        .try_list_models()
        .expect("models should be available");
    assert!(
        available.iter().any(|preset| preset.model == "remote-new"),
        "new remote model should be listed"
    );
    assert!(
        !available.iter().any(|preset| preset.model == "remote-old"),
        "removed remote model should not be listed"
    );
    assert_eq!(
        initial_mock.requests().len(),
        1,
        "initial refresh should only hit /models once"
    );
    assert_eq!(
        refreshed_mock.requests().len(),
        1,
        "second refresh should only hit /models once"
    );
}

#[tokio::test]
async fn refresh_available_models_skips_network_without_chatgpt_auth() {
    let server = MockServer::start().await;
    let dynamic_slug = "dynamic-model-only-for-test-noauth";
    let models_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: vec![remote_model(dynamic_slug, "No Auth", 1)],
        },
    )
    .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager = Arc::new(AuthManager::new(
        codex_home.path().to_path_buf(),
        false,
        AuthCredentialsStoreMode::File,
    ));
    let provider = provider_for(server.uri());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    manager
        .refresh_available_models(RefreshStrategy::Online)
        .await
        .expect("refresh should no-op without chatgpt auth");
    let cached_remote = manager.get_remote_models().await;
    assert!(
        !cached_remote
            .iter()
            .any(|candidate| candidate.slug == dynamic_slug),
        "remote refresh should be skipped without chatgpt auth"
    );
    assert_eq!(
        models_mock.requests().len(),
        0,
        "no auth should avoid /models requests"
    );
}

#[tokio::test]
async fn refresh_available_models_fetches_github_copilot_catalog_with_api_key_auth() {
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
                .contains("authorization: bearer copilot-test-token"),
            "expected bearer token in Authorization header, got:\n{request}"
        );

        let response_body = serde_json::json!({
            "data": [
                {"id": "copilot-only-model-123"},
                {"id": "copilot-only-model-123"},
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

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
    let mut provider = ModelProviderInfo::create_github_copilot_provider();
    provider.base_url = Some(format!("http://{addr}"));
    // Ensure auth comes from stored auth token, not process env.
    provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
    server.join().expect("models server should complete");

    assert!(
        available
            .iter()
            .any(|preset| preset.model == "copilot-only-model-123"),
        "expected Copilot-only model in available list"
    );
    assert_eq!(
        available
            .iter()
            .filter(|preset| preset.model == "copilot-only-model-123")
            .count(),
        1,
        "duplicate Copilot catalog entries should be deduplicated"
    );
}

#[test]
fn build_available_models_picks_default_after_hiding_hidden_models() {
    let codex_home = tempdir().expect("temp dir");
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let provider = provider_for("http://example.test".to_string());
    let manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
    );

    let hidden_model = remote_model_with_visibility("hidden", "Hidden", 0, "hide");
    let visible_model = remote_model_with_visibility("visible", "Visible", 1, "list");

    let expected_hidden = ModelPreset::from(hidden_model.clone());
    let mut expected_visible = ModelPreset::from(visible_model.clone());
    expected_visible.is_default = true;

    let available = manager.build_available_models(vec![hidden_model, visible_model]);

    assert_eq!(available, vec![expected_hidden, expected_visible]);
}

#[test]
fn bundled_models_json_roundtrips() {
    let file_contents = include_str!("../../models.json");
    let response: ModelsResponse =
        serde_json::from_str(file_contents).expect("bundled models.json should deserialize");

    let serialized =
        serde_json::to_string(&response).expect("bundled models.json should serialize");
    let roundtripped: ModelsResponse =
        serde_json::from_str(&serialized).expect("serialized models.json should deserialize");

    assert_eq!(
        response, roundtripped,
        "bundled models.json should round trip through serde"
    );
    assert!(
        !response.models.is_empty(),
        "bundled models.json should contain at least one model"
    );
}

#[tokio::test]
async fn non_openai_provider_ignores_cross_provider_cache_and_uses_models_dev_catalog() {
    let codex_home = tempdir().expect("temp dir");
    let copilot_auth =
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
    let mut copilot_provider = ModelProviderInfo::create_github_copilot_provider();
    copilot_provider.base_url = Some("https://api.githubcopilot.com".to_string());
    let copilot_manager = ModelsManager::with_provider_for_tests(
        codex_home.path().to_path_buf(),
        copilot_auth,
        copilot_provider,
    );

    let leaked_slug = "copilot-cache-only-model";
    copilot_manager
        .cache_manager
        .persist_cache(
            &[remote_model(leaked_slug, "Leaked", 0)],
            None,
            crate::models_manager::client_version_to_whole(),
            copilot_manager.cache_scope_key(),
        )
        .await;

    let models_dev_server = MockServer::start().await;
    let _models_dev = wiremock::Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "azure": {
                "id": "azure",
                "name": "Azure",
                "api": "https://azure.example.com/openai",
                "env": ["AZURE_OPENAI_API_KEY"],
                "models": {
                    "azure-model-a": {
                        "id": "azure-model-a",
                        "name": "Azure Model A",
                        "release_date": "2026-01-01",
                        "attachment": false,
                        "reasoning": true,
                        "temperature": true,
                        "tool_call": true,
                        "limit": {"context": 128000, "output": 4096},
                        "options": {}
                    }
                }
            }
        })))
        .expect(1)
        .mount_as_scoped(&models_dev_server)
        .await;

    let azure_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("azure-key"));
    let azure_provider = ModelProviderInfo {
        name: "Azure".to_string(),
        base_url: Some("https://azure.example.com/openai".to_string()),
        env_key: Some("AZURE_OPENAI_API_KEY".to_string()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: Some(
            [("api-version".to_string(), "2025-04-01-preview".to_string())]
                .into_iter()
                .collect(),
        ),
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };
    let azure_manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
        codex_home.path().to_path_buf(),
        azure_auth,
        azure_provider,
        format!("{}/api.json", models_dev_server.uri()),
    );

    let available = azure_manager
        .list_models(RefreshStrategy::OnlineIfUncached)
        .await;
    assert!(
        !available.iter().any(|preset| preset.model == leaked_slug),
        "non-openai providers must ignore cache entries from github-copilot scope"
    );
    assert!(
        available
            .iter()
            .any(|preset| preset.model == "azure-model-a"),
        "expected models.dev-discovered model to be listed"
    );
    assert!(
        !available
            .iter()
            .any(|preset| preset.model.starts_with("gpt-")),
        "non-openai provider should use authoritative models.dev catalog, not bundled OpenAI catalog"
    );
}

#[tokio::test]
async fn models_dev_provider_match_uses_base_url_host_when_name_is_custom() {
    let models_dev_server = MockServer::start().await;
    let _models_dev = wiremock::Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "anthropic": {
                "id": "anthropic",
                "name": "Anthropic",
                "api": "https://api.anthropic.com/v1",
                "env": ["ANTHROPIC_API_KEY"],
                "models": {
                    "claude-host-match": {
                        "id": "claude-host-match",
                        "name": "Claude Host Match",
                        "release_date": "2026-01-01",
                        "attachment": false,
                        "reasoning": true,
                        "temperature": true,
                        "tool_call": true,
                        "limit": {"context": 200000, "output": 4096},
                        "options": {}
                    }
                }
            }
        })))
        .expect(1)
        .mount_as_scoped(&models_dev_server)
        .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
    let provider = ModelProviderInfo {
        name: "Internal Anthropic Proxy".to_string(),
        base_url: Some("https://api.anthropic.com/v1".to_string()),
        env_key: Some("ANTHROPIC_API_KEY".to_string()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };
    let manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
        format!("{}/api.json", models_dev_server.uri()),
    );

    let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
    assert!(
        available
            .iter()
            .any(|preset| preset.model == "claude-host-match"),
        "expected provider host fallback to match models.dev provider"
    );
}

#[tokio::test]
async fn non_openai_provider_falls_back_to_provider_models_when_models_dev_has_no_match() {
    let models_dev_server = MockServer::start().await;
    let _models_dev = wiremock::Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "unrelated": {
                "id": "unrelated",
                "name": "Unrelated Provider",
                "api": "https://unrelated.example.com/v1",
                "env": ["UNRELATED_KEY"],
                "models": {}
            }
        })))
        .expect(1)
        .mount_as_scoped(&models_dev_server)
        .await;

    let provider_server = MockServer::start().await;
    let _provider_models = wiremock::Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "provider-fallback-a"},
                {"id": "provider-fallback-b"}
            ]
        })))
        .expect(1)
        .mount_as_scoped(&provider_server)
        .await;

    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("provider-token"));
    let provider = ModelProviderInfo {
        name: "Custom Provider".to_string(),
        base_url: Some(format!("{}/v1", provider_server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };
    let manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
        codex_home.path().to_path_buf(),
        auth_manager,
        provider,
        format!("{}/api.json", models_dev_server.uri()),
    );

    let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
    assert!(
        available
            .iter()
            .any(|preset| preset.model == "provider-fallback-a"),
        "expected fallback to provider /models endpoint when models.dev has no match"
    );
    assert!(
        !available
            .iter()
            .any(|preset| preset.model.starts_with("gpt-")),
        "provider-authoritative fallback should still avoid bundled OpenAI model bleed"
    );
}
