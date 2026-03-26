//! Verifies that the agent retries when the SSE stream terminates before
//! delivering a `response.completed` event.

use codex_core::ModelProviderInfo;
use codex_core::WireApi;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_reasoning_summary_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_early_close() {
    skip_if_no_network!();

    let call_id = "call-retry";
    let reasoning_id = "reasoning-retry";
    let args = json!({
        "command": "printf retry",
        "timeout_ms": 1_000
    })
    .to_string();
    let incomplete_sse = responses::sse(vec![
        ev_response_created("resp_incomplete"),
        ev_reasoning_item_added(reasoning_id, &[""]),
        ev_reasoning_summary_text_delta("thinking"),
        ev_function_call(call_id, "shell_command", &args),
    ]);
    let completed_sse = responses::sse_completed("resp_ok");

    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: incomplete_sse,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed_sse,
        }],
    ])
    .await;

    // Configure retry behavior explicitly to avoid mutating process-wide
    // environment variables.

    let model_provider = ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        // Environment variable that should exist in the test environment.
        // ModelClient will return an error if the environment variable for the
        // provider is not set.
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        // exercise retry path: first attempt yields incomplete stream, so allow 1 retry
        request_max_retries: Some(0),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await
        .unwrap();

    // Wait until TurnComplete (should succeed after retry).
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "expected retry after incomplete SSE stream"
    );
    let second_body: Value = serde_json::from_slice(&requests[1]).expect("parse second request");
    let second_input = second_body["input"]
        .as_array()
        .expect("second request input should be an array");
    let reasoning_index = second_input
        .iter()
        .position(|item| {
            item.get("type").and_then(Value::as_str) == Some("reasoning")
                && item.get("id").and_then(Value::as_str) == Some(reasoning_id)
        })
        .expect("retry request should include the interrupted reasoning item");
    let function_call_index = second_input
        .iter()
        .position(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
        .expect("retry request should include the interrupted function call");
    let function_output_index = second_input
        .iter()
        .position(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
        .expect("retry request should include the tool output from the interrupted call");
    assert!(
        reasoning_index < function_call_index && function_call_index < function_output_index,
        "expected retry replay order reasoning -> function_call -> function_call_output"
    );

    server.shutdown().await;
}
