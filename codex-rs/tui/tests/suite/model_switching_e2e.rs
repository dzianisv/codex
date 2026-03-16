use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::ConfigBuilder;
use serde_json::Value as JsonValue;
use tempfile::TempDir;
use tokio::select;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio::time::timeout;

#[tokio::test]
async fn model_picker_search_switches_and_persists_across_restarts() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let codex_home = tempdir_with_ollama_config(&repo_root, "gpt-5.3-codex")?;
    let (first_base_url, first_server) = spawn_models_server(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "gpt-5.3-codex", "object": "model"},
            {"id": "claude-opus-4.6", "object": "model"}
        ]
    }))?;
    let (first_models_dev_url, first_models_dev_server) =
        spawn_models_dev_catalog_server(serde_json::json!({
            "github-copilot": {
                "id": "github-copilot",
                "name": "GitHub Copilot",
                "api": "https://api.githubcopilot.com/v1",
                "models": {
                    "gpt-5.3-codex": {"id": "gpt-5.3-codex", "name": "gpt-5.3-codex"}
                }
            },
            "lmstudio": {
                "id": "lmstudio",
                "name": "LM Studio",
                "api": "http://127.0.0.1:1234/v1",
                "models": {
                    "qwen2.5-coder:7b": {"id": "qwen2.5-coder:7b", "name": "qwen2.5-coder:7b"}
                }
            }
        }))?;
    let mut first_env = HashMap::new();
    first_env.insert("CODEX_OSS_BASE_URL".to_string(), first_base_url);
    first_env.insert("CODEX_MODELS_DEV_URL".to_string(), first_models_dev_url);

    let first_output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "gpt-5.3-codex",
        "claude",
        None,
        first_env,
    )
    .await
    .context("switch from gpt-5.3-codex to claude-opus-4.6")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        "claude-opus-4.6",
        "ollama",
        &first_output,
    )?;
    let first_requests = first_server
        .join()
        .expect("first model server should join")?;
    anyhow::ensure!(
        first_requests
            .iter()
            .any(|line| is_models_endpoint_request(line)),
        "expected GET /v1/models in first run, got: {first_requests:?}"
    );
    let _first_models_dev_requests = first_models_dev_server
        .join()
        .expect("first models.dev server should join")?;

    let (second_base_url, second_server) = spawn_models_server(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "gpt-5.3-codex", "object": "model"},
            {"id": "claude-opus-4.6", "object": "model"}
        ]
    }))?;
    let (second_models_dev_url, second_models_dev_server) =
        spawn_models_dev_catalog_server(serde_json::json!({
            "github-copilot": {
                "id": "github-copilot",
                "name": "GitHub Copilot",
                "api": "https://api.githubcopilot.com/v1",
                "models": {
                    "gpt-5.3-codex": {"id": "gpt-5.3-codex", "name": "gpt-5.3-codex"}
                }
            },
            "lmstudio": {
                "id": "lmstudio",
                "name": "LM Studio",
                "api": "http://127.0.0.1:1234/v1",
                "models": {
                    "qwen2.5-coder:7b": {"id": "qwen2.5-coder:7b", "name": "qwen2.5-coder:7b"}
                }
            }
        }))?;
    let mut second_env = HashMap::new();
    second_env.insert("CODEX_OSS_BASE_URL".to_string(), second_base_url);
    second_env.insert("CODEX_MODELS_DEV_URL".to_string(), second_models_dev_url);

    let second_output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "claude-opus-4.6",
        "gpt-5.3-codex",
        None,
        second_env,
    )
    .await
    .context("switch back from claude-opus-4.6 to gpt-5.3-codex")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        "gpt-5.3-codex",
        "ollama",
        &second_output,
    )?;
    let second_requests = second_server
        .join()
        .expect("second model server should join")?;
    anyhow::ensure!(
        second_requests
            .iter()
            .any(|line| is_models_endpoint_request(line)),
        "expected GET /v1/models in second run, got: {second_requests:?}"
    );
    let _second_models_dev_requests = second_models_dev_server
        .join()
        .expect("second models.dev server should join")?;

    Ok(())
}

/// Regression for https://github.com/dzianisv/codex/issues/11:
/// if a model is returned with `model_picker_enabled=false`, `/model` must
/// not let users switch to it.
#[tokio::test]
async fn model_picker_ignores_openai_compat_models_disabled_for_picker() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let codex_home = tempdir_with_ollama_config(&repo_root, "llama3.2:latest")?;
    let (base_url, server) = spawn_models_server(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "llama3.2:latest", "object": "model", "model_picker_enabled": true},
            {"id": "claude-opus-4.6", "object": "model", "model_picker_enabled": false}
        ]
    }))?;

    let mut env = HashMap::new();
    env.insert("CODEX_OSS_BASE_URL".to_string(), base_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "llama3.2:latest",
        "claude-opus-4.6",
        None,
        env,
    )
    .await
    .context("attempt to switch to model_picker_enabled=false model")?;
    assert_model_and_provider_in_config(codex_home.path(), "llama3.2:latest", "ollama", &output)?;

    let requests = server.join().expect("model listing server should join")?;
    assert!(
        requests.iter().any(|line| is_models_endpoint_request(line)),
        "expected at least one GET /v1/models request, got: {requests:?}"
    );

    Ok(())
}

#[tokio::test]
async fn ollama_model_picker_uses_local_models_endpoint_and_switches() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let codex_home = tempdir_with_ollama_config(&repo_root, "llama3.2:latest")?;
    let (base_url, server) = spawn_models_server(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "llama3.2:latest", "object": "model"},
            {"id": "qwen2.5-coder:7b", "object": "model"}
        ]
    }))?;

    let mut env = HashMap::new();
    env.insert("CODEX_OSS_BASE_URL".to_string(), base_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "llama3.2:latest",
        "qwen2.5-coder:7b",
        None,
        env,
    )
    .await
    .context("switch model for ollama provider")?;
    assert_model_and_provider_in_config(codex_home.path(), "qwen2.5-coder:7b", "ollama", &output)?;

    let requests = server.join().expect("model listing server should join")?;
    assert!(
        requests.iter().any(|line| is_models_endpoint_request(line)),
        "expected at least one GET /v1/models request, got: {requests:?}"
    );

    Ok(())
}

#[tokio::test]
async fn ollama_model_picker_does_not_switch_to_bundled_gpt_when_discovery_fails() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let codex_home = tempdir_with_ollama_config(&repo_root, "llama3.2:latest")?;
    let (base_url, server) = spawn_failing_ollama_discovery_server()?;

    let mut env = HashMap::new();
    env.insert("CODEX_OSS_BASE_URL".to_string(), base_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "llama3.2:latest",
        "github-copilot/gpt-5",
        None,
        env,
    )
    .await
    .context("attempt to switch Ollama to bundled GPT slug when model discovery fails")?;
    assert_model_and_provider_in_config(codex_home.path(), "llama3.2:latest", "ollama", &output)?;

    let requests = server.join().expect("discovery server should join")?;
    assert!(
        requests.iter().any(|line| is_models_endpoint_request(line)),
        "expected GET /v1/models request, got: {requests:?}"
    );

    Ok(())
}

#[tokio::test]
async fn ollama_model_switch_then_prompt_uses_responses_api() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let codex_home = tempdir_with_ollama_config(&repo_root, "llama3.2:latest")?;
    let prompt = "When the first gpt model was released?";
    let answer_text = "The first GPT model (GPT-1) was released in 2018.";
    let (base_url, server) = spawn_openai_compat_models_and_responses_server(
        serde_json::json!({
            "object": "list",
            "data": [
                {"id": "llama3.2:latest", "object": "model"},
                {"id": "qwen2.5-coder:7b", "object": "model"}
            ]
        }),
        "qwen2.5-coder:7b",
        answer_text,
    )?;

    let mut env = HashMap::new();
    env.insert("CODEX_OSS_BASE_URL".to_string(), base_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "llama3.2:latest",
        "qwen2.5-coder:7b",
        Some(prompt),
        env,
    )
    .await
    .context("switch model and run simple prompt against local responses API")?;
    assert_model_and_provider_in_config(codex_home.path(), "qwen2.5-coder:7b", "ollama", &output)?;
    anyhow::ensure!(
        output.contains("2018"),
        "expected answer to contain 2018, got output: {output}"
    );

    let requests = server.join().expect("model/responses server should join")?;
    anyhow::ensure!(
        requests
            .iter()
            .any(|request| is_models_endpoint_request(request)),
        "expected GET /v1/models request; got requests: {requests:?}"
    );
    let responses_request = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/responses "))
        .context("missing POST /v1/responses request")?;
    anyhow::ensure!(
        responses_request.contains("\"model\":\"qwen2.5-coder:7b\""),
        "expected switched model in /responses request body; request: {responses_request}"
    );
    anyhow::ensure!(
        responses_request.contains(prompt),
        "expected prompt text in /responses request body; request: {responses_request}"
    );

    Ok(())
}

#[tokio::test]
async fn copilot_model_switch_then_prompt_uses_responses_api_without_cli_provider_override()
-> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };
    let prompt = "When the first gpt model was released?";
    let answer_text = "The first GPT model (GPT-1) was released in June 2018.";
    let startup_model = "gpt-5";
    let selected_model = "claude-4.6-opus";
    let (base_url, server) = spawn_openai_compat_models_and_responses_server(
        serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": startup_model,
                    "object": "model",
                    "model_picker_enabled": true,
                    "supported_endpoints": ["/responses"]
                },
                {
                    "id": selected_model,
                    "object": "model",
                    "model_picker_enabled": true,
                    "supported_endpoints": ["/responses"]
                }
            ]
        }),
        selected_model,
        answer_text,
    )?;
    let codex_home = tempdir_with_github_copilot_config(
        &repo_root,
        startup_model,
        base_url.as_str(),
        &[startup_model, selected_model],
    )?;

    let mut env = HashMap::new();
    env.insert(
        "GITHUB_COPILOT_TOKEN".to_string(),
        "test-copilot-token".to_string(),
    );
    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        startup_model,
        selected_model,
        Some(prompt),
        env,
    )
    .await
    .context("switch model via /model and run prompt against local Copilot responses API")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        selected_model,
        "github-copilot",
        &output,
    )?;

    let requests = server.join().expect("model/responses server should join")?;
    anyhow::ensure!(
        requests
            .iter()
            .any(|request| is_models_endpoint_request(request)),
        "expected GET /v1/models request against localhost Copilot provider; got requests: {requests:?}"
    );
    let responses_request = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/responses "))
        .context("missing POST /v1/responses request")?;
    anyhow::ensure!(
        responses_request.contains(format!("\"model\":\"{selected_model}\"").as_str()),
        "expected switched model in /responses request body; request: {responses_request}"
    );
    anyhow::ensure!(
        responses_request.contains(prompt),
        "expected prompt text in /responses request body; request: {responses_request}"
    );

    Ok(())
}

#[tokio::test]
async fn copilot_chat_completions_only_model_switch_then_prompt_preserves_model() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };

    let prompt = "When the first gpt model was released?";
    let answer_text = "The first GPT model (GPT-1) was released in June 2018.";
    let startup_model = "gpt-5";
    let selected_model = "claude-4.6-opus";
    let (base_url, server) = spawn_openai_compat_models_with_chat_completions_fallback_server(
        serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": startup_model,
                    "object": "model",
                    "model_picker_enabled": true,
                    "supported_endpoints": ["/responses"]
                },
                {
                    "id": selected_model,
                    "object": "model",
                    "model_picker_enabled": true,
                    "supported_endpoints": ["/v1/messages", "/chat/completions"]
                }
            ]
        }),
        selected_model,
        answer_text,
    )?;
    let codex_home = tempdir_with_github_copilot_config(
        &repo_root,
        startup_model,
        base_url.as_str(),
        &[startup_model, selected_model],
    )?;

    let mut env = HashMap::new();
    env.insert(
        "GITHUB_COPILOT_TOKEN".to_string(),
        "test-copilot-token".to_string(),
    );
    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        startup_model,
        selected_model,
        Some(prompt),
        env,
    )
    .await
    .context("switch to chat-completions-only Copilot model and run prompt")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        selected_model,
        "github-copilot",
        &output,
    )?;
    anyhow::ensure!(
        output.contains("2018"),
        "expected fallback answer to include 2018, got output: {output}"
    );

    let requests = server.join().expect("model/responses server should join")?;
    anyhow::ensure!(
        requests
            .iter()
            .any(|request| is_models_endpoint_request(request)),
        "expected GET /v1/models request against localhost Copilot provider; got requests: {requests:?}"
    );
    let responses_request = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/responses "))
        .context("missing POST /v1/responses request")?;
    anyhow::ensure!(
        responses_request.contains(format!("\"model\":\"{selected_model}\"").as_str()),
        "expected switched model in /responses request body; request: {responses_request}"
    );

    let chat_request = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/chat/completions "))
        .context("missing POST /v1/chat/completions request")?;
    anyhow::ensure!(
        chat_request.contains(format!("\"model\":\"{selected_model}\"").as_str()),
        "expected switched model in /chat/completions request body; request: {chat_request}"
    );
    anyhow::ensure!(
        chat_request.contains(prompt),
        "expected prompt text in /chat/completions request body; request: {chat_request}"
    );

    Ok(())
}

#[tokio::test]
async fn models_dev_provider_model_switch_then_prompt_uses_selected_model() -> Result<()> {
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };

    let prompt = "When the first gpt model was released?";
    let answer_text = "The first GPT model (GPT-1) was released in June 2018.";
    let startup_model = "azure-model-a";
    let selected_model = "azure-model-b";
    let provider_id = "azure-local";

    let (provider_base_url, models_dev_url, server) = spawn_models_dev_and_responses_server(
        "azure",
        "Azure",
        &[startup_model, selected_model],
        selected_model,
        answer_text,
    )?;
    let codex_home = tempdir_with_models_dev_provider_config(
        &repo_root,
        provider_id,
        "Azure",
        startup_model,
        provider_base_url.as_str(),
        "AZURE_TEST_KEY",
    )?;

    let mut env = HashMap::new();
    env.insert("AZURE_TEST_KEY".to_string(), "test-azure-token".to_string());
    env.insert("CODEX_MODELS_DEV_URL".to_string(), models_dev_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        startup_model,
        selected_model,
        Some(prompt),
        env,
    )
    .await
    .context("switch model via /model for models.dev provider and run prompt")?;
    let requests = server
        .join()
        .expect("models.dev/responses server should join")?;

    anyhow::ensure!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /api.json ")),
        "expected GET /api.json request; got requests: {requests:?}; output: {output}"
    );
    assert_model_and_provider_in_config(codex_home.path(), selected_model, provider_id, &output)?;
    anyhow::ensure!(
        output.contains("2018"),
        "expected answer to contain 2018, got output: {output}"
    );
    let responses_request = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/responses "))
        .context("missing POST /v1/responses request")?;
    anyhow::ensure!(
        responses_request.contains("\"model\":\"azure-model-b\""),
        "expected switched model in /responses request body; request: {responses_request}"
    );
    anyhow::ensure!(
        responses_request.contains(prompt),
        "expected prompt text in /responses request body; request: {responses_request}"
    );

    Ok(())
}

#[tokio::test]
async fn models_dev_provider_config_parses_custom_provider() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempdir_with_models_dev_provider_config(
        &repo_root,
        "azure-local",
        "Azure",
        "azure-model-a",
        "http://127.0.0.1:12345/v1",
        "AZURE_TEST_KEY",
    )?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;

    anyhow::ensure!(
        config.model_provider_id == "azure-local",
        "expected model_provider_id=azure-local, got {}",
        config.model_provider_id
    );
    anyhow::ensure!(
        config.model_provider.name == "Azure",
        "expected model_provider.name=Azure, got {}",
        config.model_provider.name
    );
    anyhow::ensure!(
        config.model_providers.contains_key("azure-local"),
        "expected model_providers to contain azure-local, got keys: {:?}",
        config.model_providers.keys().collect::<Vec<_>>()
    );

    Ok(())
}

#[tokio::test]
async fn cross_provider_model_switch_applies_immediately_without_manual_new_session() -> Result<()>
{
    // run_codex_cli_with_filter() does not work on Windows due to PTY limitations.
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };

    let startup_model = "azure-model-a";
    let selected_model = "claude-4.6-opus";
    let prompt = "Name the provider handling this prompt.";
    let azure_answer = "Azure should not handle the post-switch prompt.";
    let copilot_answer = "Copilot provider handled this prompt immediately.";

    let (azure_base_url, azure_server) = spawn_openai_compat_models_and_responses_server(
        serde_json::json!({
            "object": "list",
            "data": [
                {"id": startup_model, "object": "model"},
                {"id": "azure-model-b", "object": "model"}
            ]
        }),
        startup_model,
        azure_answer,
    )?;
    let (copilot_base_url, copilot_server) = spawn_openai_compat_models_and_responses_server(
        serde_json::json!({
            "object": "list",
            "data": [
                {"id": selected_model, "object": "model"}
            ]
        }),
        selected_model,
        copilot_answer,
    )?;
    let (models_dev_url, models_dev_server) = spawn_models_dev_catalog_server(serde_json::json!({
        "azure": {
            "id": "azure",
            "name": "Azure",
            "api": azure_base_url.clone(),
            "models": {
                startup_model: {"id": startup_model, "name": startup_model},
                "azure-model-b": {"id": "azure-model-b", "name": "azure-model-b"}
            }
        },
        "github-copilot": {
            "id": "github-copilot",
            "name": "GitHub Copilot",
            "api": copilot_base_url.clone(),
            "models": {
                selected_model: {"id": selected_model, "name": selected_model}
            }
        },
        "ollama": {
            "id": "ollama",
            "name": "Ollama",
            "api": "http://127.0.0.1:11434/v1",
            "models": {
                "llama3.2:latest": {"id": "llama3.2:latest", "name": "llama3.2:latest"}
            }
        },
        "lmstudio": {
            "id": "lmstudio",
            "name": "LM Studio",
            "api": "http://127.0.0.1:1234/v1",
            "models": {
                "qwen2.5-coder:7b": {"id": "qwen2.5-coder:7b", "name": "qwen2.5-coder:7b"}
            }
        }
    }))?;
    let codex_home = tempdir_with_dual_provider_config(
        &repo_root,
        startup_model,
        azure_base_url.as_str(),
        copilot_base_url.as_str(),
    )?;

    let mut env = HashMap::new();
    env.insert("AZURE_TEST_KEY".to_string(), "test-azure-token".to_string());
    env.insert(
        "GITHUB_COPILOT_TOKEN".to_string(),
        "test-copilot-token".to_string(),
    );
    env.insert("CODEX_MODELS_DEV_URL".to_string(), models_dev_url);

    let output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        startup_model,
        selected_model,
        Some(prompt),
        env,
    )
    .await
    .context("cross-provider switch via /model should apply immediately in-session")?;

    let azure_requests = azure_server
        .join()
        .expect("azure test server should join")?;
    let copilot_requests = copilot_server
        .join()
        .expect("copilot test server should join")?;
    let models_dev_requests = models_dev_server
        .join()
        .expect("models.dev server should join")?;

    anyhow::ensure!(
        copilot_requests
            .iter()
            .any(|request| is_models_endpoint_request(request)),
        "expected Copilot provider /models discovery request; got requests: {copilot_requests:?}"
    );
    anyhow::ensure!(
        models_dev_requests
            .iter()
            .any(|request| request.starts_with("GET /api.json ")),
        "expected GET /api.json request; got requests: {models_dev_requests:?}"
    );

    assert_model_and_provider_in_config(
        codex_home.path(),
        selected_model,
        "github-copilot",
        &output,
    )?;
    anyhow::ensure!(
        output.contains(copilot_answer),
        "expected Copilot answer after in-session provider switch, got: {output}"
    );
    anyhow::ensure!(
        !output.contains("Start a new session to apply provider change"),
        "did not expect restart hint after provider switch, got: {output}"
    );

    let copilot_responses_request = copilot_requests
        .iter()
        .find(|request| request.starts_with("POST /v1/responses "))
        .context("missing Copilot POST /v1/responses request after provider switch")?;
    anyhow::ensure!(
        copilot_responses_request.contains(format!("\"model\":\"{selected_model}\"").as_str()),
        "expected selected model in Copilot /responses request body; request: {copilot_responses_request}"
    );
    anyhow::ensure!(
        copilot_responses_request.contains(prompt),
        "expected prompt text in Copilot /responses request body; request: {copilot_responses_request}"
    );
    anyhow::ensure!(
        !azure_requests
            .iter()
            .any(|request| request.starts_with("POST /v1/responses ")),
        "did not expect post-switch /responses requests against Azure provider; requests: {azure_requests:?}"
    );

    Ok(())
}

fn tempdir_with_ollama_config(repo_root: &Path, model: &str) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let repo_root_display = repo_root.display();
    let config_contents = format!(
        r#"model_provider = "ollama"
model = "{model}"
cli_auth_credentials_store = "file"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn tempdir_with_github_copilot_config(
    repo_root: &Path,
    model: &str,
    base_url: &str,
    catalog_models: &[&str],
) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let source_catalog_path = codex_utils_cargo_bin::find_resource!("../core/models.json")?;
    let source_catalog = std::fs::read_to_string(&source_catalog_path)?;
    let mut source_catalog: JsonValue = serde_json::from_str(&source_catalog)?;
    let models = source_catalog
        .get_mut("models")
        .and_then(JsonValue::as_array_mut)
        .context("models array missing")?;
    let template = models.first().cloned().context("models array is empty")?;
    *models = catalog_models
        .iter()
        .enumerate()
        .map(|(index, catalog_model)| {
            model_from_template(&template, catalog_model, catalog_model, index as i64)
        })
        .collect::<Result<Vec<_>>>()?;

    let custom_catalog_path = codex_home.path().join("catalog.json");
    std::fs::write(
        &custom_catalog_path,
        serde_json::to_string(&source_catalog)?,
    )?;

    let repo_root_display = repo_root.display();
    let catalog_display = custom_catalog_path.display();
    let config_contents = format!(
        r#"model_provider = "github-copilot"
model = "{model}"
model_catalog_json = "{catalog_display}"
cli_auth_credentials_store = "file"

[model_providers.github-copilot]
name = "GitHub Copilot"
base_url = "{base_url}"
env_key = "GITHUB_COPILOT_TOKEN"
wire_api = "responses"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn tempdir_with_models_dev_provider_config(
    repo_root: &Path,
    provider_id: &str,
    provider_name: &str,
    model: &str,
    base_url: &str,
    env_key: &str,
) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let repo_root_display = repo_root.display();
    let config_contents = format!(
        r#"model_provider = "{provider_id}"
model = "{model}"
cli_auth_credentials_store = "file"

[model_providers.{provider_id}]
name = "{provider_name}"
base_url = "{base_url}"
env_key = "{env_key}"
wire_api = "responses"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn tempdir_with_dual_provider_config(
    repo_root: &Path,
    startup_model: &str,
    azure_base_url: &str,
    copilot_base_url: &str,
) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let repo_root_display = repo_root.display();
    let config_contents = format!(
        r#"model_provider = "azure-local"
model = "{startup_model}"
cli_auth_credentials_store = "file"

[model_providers.azure-local]
name = "Azure"
base_url = "{azure_base_url}"
env_key = "AZURE_TEST_KEY"
wire_api = "responses"

[model_providers.github-copilot]
name = "GitHub Copilot"
base_url = "{copilot_base_url}"
env_key = "GITHUB_COPILOT_TOKEN"
wire_api = "responses"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn is_models_endpoint_request(request_line: &str) -> bool {
    request_line.starts_with("GET /v1/models ") || request_line.starts_with("GET /v1/models?")
}

fn spawn_models_server(
    response_json: serde_json::Value,
) -> Result<(String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let response_body = response_json.to_string();
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(20);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .context("failed to set read timeout")?;
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                request.extend_from_slice(&chunk[..bytes_read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(err)
                                if err.kind() == std::io::ErrorKind::WouldBlock
                                    || err.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                break;
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }

                    let request = String::from_utf8_lossy(&request).to_string();
                    requests.push(request.lines().next().unwrap_or_default().to_string());

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write model list response")?;
                    stream
                        .flush()
                        .context("failed to flush model list response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(2));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((format!("http://{address}/v1"), handle))
}

fn spawn_failing_ollama_discovery_server()
-> Result<(String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(20);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .context("failed to set read timeout")?;
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                request.extend_from_slice(&chunk[..bytes_read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(err)
                                if err.kind() == std::io::ErrorKind::WouldBlock
                                    || err.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                break;
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }

                    let request = String::from_utf8_lossy(&request).to_string();
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    requests.push(request_line.clone());

                    let response = if is_models_endpoint_request(&request_line)
                        || request_line.starts_with("GET /api/tags ")
                    {
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 24\r\n\r\n{\"error\":\"test failure\"}"
                            .to_string()
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write discovery failure response")?;
                    stream
                        .flush()
                        .context("failed to flush discovery failure response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(2));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((format!("http://{address}/v1"), handle))
}

fn model_from_template(
    template: &JsonValue,
    slug: &str,
    display_name: &str,
    priority: i64,
) -> Result<JsonValue> {
    let mut model = template
        .as_object()
        .cloned()
        .context("template model is not an object")?;
    model.insert("slug".to_string(), JsonValue::String(slug.to_string()));
    model.insert(
        "display_name".to_string(),
        JsonValue::String(display_name.to_string()),
    );
    model.insert(
        "description".to_string(),
        JsonValue::String(format!("{display_name} description")),
    );
    model.insert("priority".to_string(), JsonValue::from(priority));
    model.insert(
        "visibility".to_string(),
        JsonValue::String("list".to_string()),
    );
    model.insert("supported_in_api".to_string(), JsonValue::Bool(true));
    model.insert(
        "default_reasoning_level".to_string(),
        JsonValue::String("medium".to_string()),
    );
    model.insert(
        "supported_reasoning_levels".to_string(),
        serde_json::json!([
            {"effort": "medium", "description": "medium"}
        ]),
    );
    model.insert("upgrade".to_string(), JsonValue::Null);
    model.insert("availability_nux".to_string(), JsonValue::Null);

    Ok(JsonValue::Object(model))
}

async fn run_codex_cli_with_filter(
    codex_cli: &Path,
    codex_home: &Path,
    cwd: &Path,
    startup_model_hint: &str,
    filter: &str,
    prompt_after_switch: Option<&str>,
    extra_env: HashMap<String, String>,
) -> Result<String> {
    run_codex_cli_with_filter_options(
        codex_cli,
        codex_home,
        cwd,
        startup_model_hint,
        filter,
        prompt_after_switch,
        extra_env,
        true,
    )
    .await
}

async fn run_codex_cli_with_filter_options(
    codex_cli: &Path,
    codex_home: &Path,
    cwd: &Path,
    startup_model_hint: &str,
    filter: &str,
    prompt_after_switch: Option<&str>,
    extra_env: HashMap<String, String>,
    send_escape_before_prompt: bool,
) -> Result<String> {
    let mut env = HashMap::new();
    env.insert("CODEX_HOME".to_string(), codex_home.display().to_string());
    env.extend(extra_env);

    let args = vec![
        "--no-alt-screen".to_string(),
        "-C".to_string(),
        cwd.display().to_string(),
        "-c".to_string(),
        "analytics.enabled=false".to_string(),
    ];

    let spawned = codex_utils_pty::spawn_pty_process(
        codex_cli.to_string_lossy().as_ref(),
        &args,
        cwd,
        &env,
        &None,
        codex_utils_pty::TerminalSize::default(),
    )
    .await?;

    let mut output = Vec::new();
    let codex_utils_pty::SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    let mut output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
    let mut exit_rx = exit_rx;
    let writer_tx = session.writer_sender();
    let writer_for_input = writer_tx.clone();
    let (startup_ready_tx, mut startup_ready_rx) = watch::channel(false);
    let (reasoning_prompt_tx, mut reasoning_prompt_rx) = watch::channel(false);
    const REASONING_CONFIRM_HINT: &str = "Press enter to confirm or esc to go back";
    let filter = filter.to_string();
    let prompt_after_switch = prompt_after_switch.map(std::borrow::ToOwned::to_owned);
    let input_task = tokio::spawn(async move {
        // Wait for startup to finish before dispatching `/model`.
        let _ = timeout(Duration::from_secs(20), async {
            loop {
                if *startup_ready_rx.borrow() {
                    break;
                }
                if startup_ready_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        // Allow provider model refresh to complete before opening `/model`.
        sleep(Duration::from_millis(2200)).await;
        type_text_with_stabilization(&writer_for_input, "/model").await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;
        sleep(Duration::from_millis(1200)).await;
        type_text_with_stabilization(&writer_for_input, &filter).await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;
        // Some models open an effort submenu after model selection.
        // Confirm only when that submenu is actually shown.
        let reasoning_prompt_visible = timeout(Duration::from_millis(1400), async {
            loop {
                if *reasoning_prompt_rx.borrow() {
                    break true;
                }
                if reasoning_prompt_rx.changed().await.is_err() {
                    break false;
                }
            }
        })
        .await
        .unwrap_or(false);
        if reasoning_prompt_visible {
            sleep(Duration::from_millis(200)).await;
            let _ = writer_for_input.send(vec![b'\r']).await;
        }
        if let Some(prompt) = prompt_after_switch {
            // Allow model/effort selection to settle before prompt input.
            sleep(Duration::from_millis(900)).await;
            if send_escape_before_prompt {
                let _ = writer_for_input.send(vec![27]).await;
                sleep(Duration::from_millis(700)).await;
            }
            type_text_with_stabilization(&writer_for_input, &prompt).await;
            sleep(Duration::from_millis(120)).await;
            let _ = writer_for_input.send(vec![b'\r']).await;
            // Allow the streamed assistant output to be rendered before exit.
            sleep(Duration::from_millis(2500)).await;
        }
        sleep(Duration::from_millis(2500)).await;
        for _ in 0..4 {
            let _ = writer_for_input.send(vec![3]).await;
            sleep(Duration::from_millis(300)).await;
        }
    });

    let exit_code_result = timeout(Duration::from_secs(30), async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);
                        if !*startup_ready_tx.borrow() {
                            let startup_ready = if startup_model_hint.is_empty() {
                                true
                            } else {
                                output
                                    .windows(startup_model_hint.len())
                                    .any(|window| window == startup_model_hint.as_bytes())
                            };
                            if startup_ready {
                                let _ = startup_ready_tx.send(true);
                            }
                        }
                        if !*reasoning_prompt_tx.borrow()
                            && output
                                .windows(REASONING_CONFIRM_HINT.len())
                                .any(|window| window == REASONING_CONFIRM_HINT.as_bytes())
                        {
                            let _ = reasoning_prompt_tx.send(true);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break exit_rx.await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                },
                result = &mut exit_rx => break result,
            }
        }
    })
    .await;

    input_task.abort();

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            anyhow::bail!("timed out waiting for codex CLI to exit");
        }
    };
    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    if std::env::var("CODEX_E2E_DUMP_OUTPUT").ok().as_deref() == Some("1") {
        eprintln!(
            "--- BEGIN model_switching_e2e PTY OUTPUT ---\n{output}\n--- END model_switching_e2e PTY OUTPUT ---"
        );
    }
    anyhow::ensure!(
        exit_code == 0 || exit_code == 130,
        "unexpected exit code from codex: {exit_code}; output: {output}"
    );

    Ok(output)
}

fn find_codex_cli(cwd: &Path) -> Option<PathBuf> {
    // Always build a fresh local binary once per test process so PTY E2E
    // assertions exercise the current source tree instead of stale artifacts.
    if ensure_fallback_codex_binary_is_built(cwd).is_ok() {
        let fallback = cwd.join("codex-rs/target/debug/codex");
        if fallback.is_file() {
            return Some(fallback);
        }
    }

    codex_utils_cargo_bin::cargo_bin("codex").ok()
}

fn ensure_fallback_codex_binary_is_built(repo_root: &Path) -> Result<()> {
    static BUILD_RESULT: OnceLock<Result<()>> = OnceLock::new();
    let result = BUILD_RESULT.get_or_init(|| {
        let status = Command::new("cargo")
            .arg("build")
            .arg("--bin")
            .arg("codex")
            .current_dir(repo_root.join("codex-rs"))
            .status()
            .context("spawn cargo build --bin codex")?;
        anyhow::ensure!(
            status.success(),
            "cargo build --bin codex failed with status {status}"
        );
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(err) => Err(anyhow::anyhow!(
            "failed to build fallback codex binary: {err}"
        )),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<Option<(String, Vec<u8>)>> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .context("failed to set read timeout")?;

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(bytes_read) => {
                raw.extend_from_slice(&chunk[..bytes_read]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let Some(header_end) = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
    else {
        return Ok(None);
    };
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let request_line = headers
        .lines()
        .next()
        .context("missing request line")?
        .to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);

    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(bytes_read) => body.extend_from_slice(&chunk[..bytes_read]),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(Some((request_line, body)))
}

fn spawn_openai_compat_models_and_responses_server(
    models_response_json: serde_json::Value,
    response_model: &str,
    answer_text: &str,
) -> Result<(String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let models_response_body = models_response_json.to_string();
    let response_model_json = serde_json::to_string(response_model)?;
    let answer_json = serde_json::to_string(answer_text)?;
    let responses_sse = format!(
        "event: response.created\n\
data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-1\",\"model\":{response_model_json}}}}}\n\n\
event: response.output_item.done\n\
data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{answer_json}}}]}}}}\n\n\
event: response.completed\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp-1\",\"usage\":{{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}}}}\n\n"
    );
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(30);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some((request_line, body_bytes)) = read_http_request(&mut stream)? else {
                        continue;
                    };
                    let body = String::from_utf8_lossy(&body_bytes).to_string();
                    requests.push(format!("{request_line}\n{body}"));

                    let response = if is_models_endpoint_request(&request_line) {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            models_response_body.len(),
                            models_response_body
                        )
                    } else if request_line.starts_with("POST /v1/responses ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                            responses_sse.len(),
                            responses_sse
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };

                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write test server response")?;
                    stream
                        .flush()
                        .context("failed to flush test server response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(20));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((format!("http://{address}/v1"), handle))
}

fn spawn_models_dev_catalog_server(
    response_json: serde_json::Value,
) -> Result<(String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let response_body = response_json.to_string();
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(20);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .context("failed to set read timeout")?;
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                request.extend_from_slice(&chunk[..bytes_read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(err)
                                if err.kind() == std::io::ErrorKind::WouldBlock
                                    || err.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                break;
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }

                    let request = String::from_utf8_lossy(&request).to_string();
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    requests.push(request_line.clone());

                    let response = if request_line.starts_with("GET /api.json ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write models.dev response")?;
                    stream
                        .flush()
                        .context("failed to flush models.dev response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(2));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((format!("http://{address}/api.json"), handle))
}

fn spawn_openai_compat_models_with_chat_completions_fallback_server(
    models_response_json: serde_json::Value,
    fallback_model: &str,
    answer_text: &str,
) -> Result<(String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let models_response_body = models_response_json.to_string();
    let fallback_model_json = serde_json::to_string(fallback_model)?;
    let answer_json = serde_json::to_string(answer_text)?;
    let responses_error = format!(
        "{{\"error\":{{\"message\":\"model {fallback_model} does not support Responses API.\",\"code\":\"unsupported_api_for_model\"}}}}"
    );
    let chat_completions_response = format!(
        "{{\"id\":\"chatcmpl-fallback-1\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":{answer_json}}},\"finish_reason\":\"stop\"}}],\"model\":{fallback_model_json}}}"
    );
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(30);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some((request_line, body_bytes)) = read_http_request(&mut stream)? else {
                        continue;
                    };
                    let body = String::from_utf8_lossy(&body_bytes).to_string();
                    requests.push(format!("{request_line}\n{body}"));

                    let response = if is_models_endpoint_request(&request_line) {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            models_response_body.len(),
                            models_response_body
                        )
                    } else if request_line.starts_with("POST /v1/responses ") {
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            responses_error.len(),
                            responses_error
                        )
                    } else if request_line.starts_with("POST /v1/chat/completions ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            chat_completions_response.len(),
                            chat_completions_response
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };

                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write test server response")?;
                    stream
                        .flush()
                        .context("failed to flush test server response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(20));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((format!("http://{address}/v1"), handle))
}

fn spawn_models_dev_and_responses_server(
    models_dev_provider_id: &str,
    models_dev_provider_name: &str,
    model_ids: &[&str],
    response_model: &str,
    answer_text: &str,
) -> Result<(String, String, thread::JoinHandle<Result<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;

    let provider_api = format!("http://{address}/v1");
    let model_entries = model_ids
        .iter()
        .map(|model_id| {
            (
                model_id.to_string(),
                serde_json::json!({
                    "id": model_id,
                    "name": model_id,
                    "release_date": "2026-01-01",
                    "attachment": false,
                    "reasoning": true,
                    "temperature": true,
                    "tool_call": true,
                    "limit": {"context": 128000, "output": 4096},
                    "options": {}
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let models_dev_body = serde_json::json!({
        models_dev_provider_id: {
            "id": models_dev_provider_id,
            "name": models_dev_provider_name,
            "api": provider_api,
            "env": ["AZURE_TEST_KEY"],
            "models": model_entries
        }
    })
    .to_string();

    let response_model_json = serde_json::to_string(response_model)?;
    let answer_json = serde_json::to_string(answer_text)?;
    let responses_sse = format!(
        "event: response.created\n\
data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-1\",\"model\":{response_model_json}}}}}\n\n\
event: response.output_item.done\n\
data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{answer_json}}}]}}}}\n\n\
event: response.completed\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp-1\",\"usage\":{{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}}}}\n\n"
    );

    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let hard_deadline = Instant::now() + Duration::from_secs(30);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some((request_line, body_bytes)) = read_http_request(&mut stream)? else {
                        continue;
                    };
                    let body = String::from_utf8_lossy(&body_bytes).to_string();
                    requests.push(format!("{request_line}\n{body}"));

                    let response = if request_line.starts_with("GET /api.json ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            models_dev_body.len(),
                            models_dev_body
                        )
                    } else if request_line.starts_with("POST /v1/responses ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                            responses_sse.len(),
                            responses_sse
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };

                    stream
                        .write_all(response.as_bytes())
                        .context("failed to write test server response")?;
                    stream
                        .flush()
                        .context("failed to flush test server response")?;

                    idle_deadline = Some(Instant::now() + Duration::from_secs(20));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((provider_api, format!("http://{address}/api.json"), handle))
}

async fn type_text_with_stabilization(writer: &tokio::sync::mpsc::Sender<Vec<u8>>, text: &str) {
    for byte in text.bytes() {
        let _ = writer.send(vec![byte]).await;
        sleep(Duration::from_millis(30)).await;
    }

    // Ensure the final typed byte is flushed through paste-burst heuristics
    // before Enter by forcing one timed non-char edit.
    if !text.is_empty() {
        let _ = writer.send(vec![b' ']).await;
        sleep(Duration::from_millis(30)).await;
        let _ = writer.send(vec![127]).await;
        sleep(Duration::from_millis(30)).await;
    }
}

fn assert_model_and_provider_in_config(
    codex_home: &Path,
    expected_model: &str,
    expected_provider: &str,
    output: &str,
) -> Result<()> {
    let config = std::fs::read_to_string(codex_home.join("config.toml"))?;
    let parsed: toml::Value = toml::from_str(&config)?;
    let actual_model = parsed
        .get("model")
        .and_then(toml::Value::as_str)
        .context("missing model in config.toml")?;
    let actual_provider = parsed
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .context("missing model_provider in config.toml")?;

    anyhow::ensure!(
        actual_model == expected_model,
        "expected model={expected_model}, got {actual_model}; config: {config}; output: {output}"
    );
    anyhow::ensure!(
        actual_provider == expected_provider,
        "expected provider={expected_provider}, got {actual_provider}; config: {config}"
    );
    Ok(())
}
