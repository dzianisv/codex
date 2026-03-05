#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::bail;
use anyhow::ensure;
use core_test_support::skip_if_no_network;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

const GITHUB_COPILOT_API_BASE_URL: &str = "https://api.githubcopilot.com";
const GITHUB_COPILOT_TEST_MODEL: &str = "gpt-4.1";
const GITHUB_COPILOT_TOKEN_ENV: &str = "GITHUB_COPILOT_TOKEN";
const RUN_LIVE_TESTS_ENV: &str = "RUN_GITHUB_COPILOT_LIVE_TESTS";
const RESPONSE_MARKER: &str = "COPILOT_GPT41_LIVE_OK";

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelEntry>,
}

fn load_live_token_or_skip() -> Option<String> {
    match std::env::var(RUN_LIVE_TESTS_ENV) {
        Ok(value) if value == "1" => {}
        _ => {
            eprintln!(
                "skipping live Copilot test: set {RUN_LIVE_TESTS_ENV}=1 to enable it explicitly"
            );
            return None;
        }
    }

    let token = std::env::var(GITHUB_COPILOT_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if token.is_none() {
        eprintln!(
            "skipping live Copilot test: set {GITHUB_COPILOT_TOKEN_ENV} to a valid Copilot token"
        );
    }

    token
}

#[tokio::test]
#[ignore = "live test; requires RUN_GITHUB_COPILOT_LIVE_TESTS=1 and GITHUB_COPILOT_TOKEN"]
async fn github_copilot_gpt41_live_chat_completion_succeeds() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let Some(token) = load_live_token_or_skip() else {
        return Ok(());
    };

    let client = reqwest::Client::new();

    let models_response = client
        .get(format!("{GITHUB_COPILOT_API_BASE_URL}/models"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Openai-Intent", "conversation-edits")
        .send()
        .await
        .context("failed to request Copilot models list")?;

    ensure!(
        models_response.status().is_success(),
        "Copilot /models returned {}",
        models_response.status()
    );

    let models: ModelsListResponse = models_response
        .json()
        .await
        .context("failed to parse Copilot /models response")?;
    ensure!(
        models
            .data
            .iter()
            .any(|model| model.id == GITHUB_COPILOT_TEST_MODEL),
        "{GITHUB_COPILOT_TEST_MODEL} is not listed by Copilot /models"
    );

    let chat_payload = json!({
        "model": GITHUB_COPILOT_TEST_MODEL,
        "messages": [
            { "role": "user", "content": format!("Reply with exactly: {RESPONSE_MARKER}") }
        ],
        "max_tokens": 24,
        "temperature": 0
    });

    let chat_response = client
        .post(format!("{GITHUB_COPILOT_API_BASE_URL}/chat/completions"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Openai-Intent", "conversation-edits")
        .header("Content-Type", "application/json")
        .json(&chat_payload)
        .send()
        .await
        .context("failed to request Copilot chat completion")?;

    let status = chat_response.status();
    let body = chat_response
        .text()
        .await
        .context("failed reading Copilot chat completion response body")?;
    if !status.is_success() {
        bail!("Copilot /chat/completions returned {status}: {body}");
    }

    let body_json: Value = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse Copilot chat response as JSON: {body}"))?;

    let returned_model = body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure!(
        returned_model.starts_with("gpt-4.1"),
        "unexpected model returned by Copilot: {returned_model}"
    );

    let message = body_json
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure!(
        message.contains(RESPONSE_MARKER),
        "response did not contain marker: {message}"
    );

    Ok(())
}
