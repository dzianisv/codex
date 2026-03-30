use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value as JsonValue;
use tempfile::TempDir;
use tokio::select;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio::time::timeout;

const INTERRUPTED_HINT: &str = "Conversation interrupted - tell the model what to do differently.";

#[tokio::test]
async fn interrupted_reasoning_follow_up_does_not_replay_orphan_reasoning() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };

    let response_model = "gpt-5.3-codex";
    let follow_up_text = "Recovered after interrupt.";
    let (base_url, first_response_started_rx, server) =
        spawn_interrupt_reasoning_server(response_model, follow_up_text)?;
    let codex_home =
        tempdir_with_local_provider_config(&repo_root, response_model, base_url.as_str(), true)?;

    let mut env = HashMap::new();
    env.insert("AZURE_TEST_KEY".to_string(), "test-azure-token".to_string());

    let output = run_codex_interrupt_sequence(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        response_model,
        "Start reasoning, I may interrupt you.",
        "Continue after interrupt.",
        follow_up_text,
        env,
        first_response_started_rx,
    )
    .await?;

    let requests = server
        .join()
        .expect("interrupt reasoning test server should join")?;
    assert_eq!(
        requests.len(),
        2,
        "expected exactly two /responses requests, got {requests:?}"
    );

    let follow_up_body = requests[1]
        .split_once('\n')
        .map(|(_, body)| body)
        .context("missing captured follow-up request body")?;
    let follow_up_request: JsonValue =
        serde_json::from_str(follow_up_body).context("parse follow-up request body")?;
    let reasoning_items = follow_up_request["input"]
        .as_array()
        .context("follow-up request input should be an array")?
        .iter()
        .filter(|item| item.get("type").and_then(JsonValue::as_str) == Some("reasoning"))
        .collect::<Vec<_>>();
    assert!(
        reasoning_items.is_empty(),
        "expected no replayed reasoning items after interrupt, got {reasoning_items:?}"
    );
    assert!(
        follow_up_body.contains("Continue after interrupt."),
        "expected follow-up prompt in second request body, got {follow_up_body}"
    );
    assert!(
        output.contains(INTERRUPTED_HINT),
        "expected interrupted-turn guidance in PTY output, got: {output}"
    );
    assert!(
        output.contains(follow_up_text),
        "expected follow-up assistant output in PTY output, got: {output}"
    );
    assert!(
        !output.contains("required following item"),
        "expected no orphan reasoning error in PTY output, got: {output}"
    );

    Ok(())
}

#[tokio::test]
async fn resumed_interrupted_reasoning_follow_up_does_not_replay_orphan_reasoning() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let Some(codex_cli) = find_codex_cli(&repo_root) else {
        eprintln!("skipping integration test because codex binary is unavailable");
        return Ok(());
    };

    let response_model = "gpt-5.3-codex";
    let follow_up_text = "Recovered after resume.";
    let (base_url, first_response_started_rx, server) =
        spawn_interrupt_reasoning_server(response_model, follow_up_text)?;
    let codex_home =
        tempdir_with_local_provider_config(&repo_root, response_model, base_url.as_str(), false)?;

    let mut env = HashMap::new();
    env.insert("AZURE_TEST_KEY".to_string(), "test-azure-token".to_string());

    let initial_output = run_codex_interrupt_sequence(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        response_model,
        "Start reasoning, I may interrupt you.",
        "",
        "",
        env.clone(),
        first_response_started_rx,
    )
    .await?;
    assert!(
        initial_output.contains(INTERRUPTED_HINT),
        "expected interrupted-turn guidance before resume, got: {initial_output}"
    );

    let resumed_output = run_codex_resume_follow_up_sequence(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        response_model,
        "Continue after resume.",
        follow_up_text,
        env,
    )
    .await?;

    let requests = server
        .join()
        .expect("interrupt reasoning resume test server should join")?;
    assert_eq!(
        requests.len(),
        2,
        "expected exactly two /responses requests across interrupt + resume, got {requests:?}"
    );

    let follow_up_body = requests[1]
        .split_once('\n')
        .map(|(_, body)| body)
        .context("missing captured resumed follow-up request body")?;
    let follow_up_request: JsonValue =
        serde_json::from_str(follow_up_body).context("parse resumed follow-up request body")?;
    let reasoning_items = follow_up_request["input"]
        .as_array()
        .context("resumed follow-up request input should be an array")?
        .iter()
        .filter(|item| item.get("type").and_then(JsonValue::as_str) == Some("reasoning"))
        .collect::<Vec<_>>();
    assert!(
        reasoning_items.is_empty(),
        "expected no replayed reasoning items after resume, got {reasoning_items:?}"
    );
    assert!(
        follow_up_body.contains("Continue after resume."),
        "expected resumed follow-up prompt in second request body, got {follow_up_body}"
    );
    assert!(
        resumed_output.contains(follow_up_text),
        "expected resumed follow-up assistant output in PTY output, got: {resumed_output}"
    );
    assert!(
        !resumed_output.contains("required following item"),
        "expected no orphan reasoning error after resume, got: {resumed_output}"
    );

    Ok(())
}

fn tempdir_with_local_provider_config(
    repo_root: &Path,
    model: &str,
    base_url: &str,
    disable_response_storage: bool,
) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let repo_root_toml = toml::Value::String(repo_root.display().to_string()).to_string();
    let model_toml = toml::Value::String(model.to_string()).to_string();
    let base_url_toml = toml::Value::String(base_url.to_string()).to_string();
    let config_contents = format!(
        r#"model_provider = "azure-local"
model = {model_toml}
disable_response_storage = {disable_response_storage}
cli_auth_credentials_store = "file"
show_tooltips = false

[model_providers.azure-local]
name = "Azure"
base_url = {base_url_toml}
env_key = "AZURE_TEST_KEY"
wire_api = "responses"

[notice.model_migrations]
"gpt-5.3-codex" = "gpt-5.4"

[projects.{repo_root_toml}]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn spawn_interrupt_reasoning_server(
    response_model: &str,
    follow_up_text: &str,
) -> Result<(
    String,
    watch::Receiver<bool>,
    thread::JoinHandle<Result<Vec<String>>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let (first_response_started_tx, first_response_started_rx) = watch::channel(false);
    let response_model_json = serde_json::to_string(response_model)?;
    let follow_up_text_json = serde_json::to_string(follow_up_text)?;
    let first_response_sse = sse(vec![
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp-interrupt",
                "model": response_model,
            }
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "type": "reasoning",
                "id": "rs_interrupt",
                "summary": [{"type": "summary_text", "text": ""}],
            }
        }),
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "thinking",
            "summary_index": 0,
        }),
    ]);
    let second_response_sse = format!(
        "event: response.created\n\
data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-follow-up\",\"model\":{response_model_json}}}}}\n\n\
event: response.output_item.done\n\
data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg-follow-up\",\"content\":[{{\"type\":\"output_text\",\"text\":{follow_up_text_json}}}]}}}}\n\n\
event: response.completed\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp-follow-up\",\"usage\":{{\"input_tokens\":0,\"input_tokens_details\":null,\"output_tokens\":0,\"output_tokens_details\":null,\"total_tokens\":0}}}}}}\n\n"
    );

    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .context("failed to set nonblocking listener")?;
        let mut requests = Vec::new();
        let mut responses_seen = 0usize;
        let hard_deadline = Instant::now() + Duration::from_secs(30);
        let mut idle_deadline: Option<Instant> = None;
        while Instant::now() < hard_deadline
            && idle_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(true)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .context("failed to set read timeout")?;
                    let (request_line, body) = read_http_request(&mut stream)?;
                    requests.push(format!("{request_line}\n{body}"));

                    if request_line.starts_with("POST /v1/responses ") {
                        responses_seen += 1;
                        if responses_seen == 1 {
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{first_response_sse}"
                            );
                            stream
                                .write_all(response.as_bytes())
                                .context("failed to write first SSE response")?;
                            stream
                                .flush()
                                .context("failed to flush first SSE response")?;
                            let _ = first_response_started_tx.send(true);
                            wait_for_client_disconnect(&mut stream, Duration::from_secs(10))?;
                        } else if responses_seen == 2 {
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                                second_response_sse.len(),
                                second_response_sse
                            );
                            stream
                                .write_all(response.as_bytes())
                                .context("failed to write second SSE response")?;
                            stream
                                .flush()
                                .context("failed to flush second SSE response")?;
                        } else {
                            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            stream
                                .write_all(response.as_bytes())
                                .context("failed to write unexpected request response")?;
                            stream
                                .flush()
                                .context("failed to flush unexpected request response")?;
                        }
                    } else {
                        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        stream
                            .write_all(response.as_bytes())
                            .context("failed to write 404 response")?;
                        stream.flush().context("failed to flush 404 response")?;
                    }

                    let idle_timeout = if responses_seen >= 2 { 2 } else { 15 };
                    idle_deadline = Some(Instant::now() + Duration::from_secs(idle_timeout));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(requests)
    });

    Ok((
        format!("http://{address}/v1"),
        first_response_started_rx,
        handle,
    ))
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Result<(String, String)> {
    let mut raw_request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(bytes_read) => {
                raw_request.extend_from_slice(&chunk[..bytes_read]);
                if raw_request.windows(4).any(|window| window == b"\r\n\r\n") {
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

    let header_end = raw_request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .context("HTTP headers terminator not found")?;
    let headers = String::from_utf8_lossy(&raw_request[..header_end]).to_string();
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

    let mut body_bytes = raw_request[header_end..].to_vec();
    while body_bytes.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(bytes_read) => body_bytes.extend_from_slice(&chunk[..bytes_read]),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok((
        request_line,
        String::from_utf8_lossy(&body_bytes).to_string(),
    ))
}

fn wait_for_client_disconnect(stream: &mut std::net::TcpStream, max_wait: Duration) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .context("failed to set disconnect probe timeout")?;
    let start = Instant::now();
    let mut probe = [0_u8; 1];
    while start.elapsed() < max_wait {
        match stream.read(&mut probe) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn sse(events: Vec<JsonValue>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for event in events {
        let kind = event
            .get("type")
            .and_then(JsonValue::as_str)
            .expect("SSE event missing type");
        writeln!(&mut out, "event: {kind}").expect("write event kind");
        if !event
            .as_object()
            .map(|object| object.len() == 1)
            .unwrap_or(false)
        {
            write!(&mut out, "data: {event}\n\n").expect("write event data");
        } else {
            out.push('\n');
        }
    }
    out
}

async fn run_codex_interrupt_sequence(
    codex_cli: &Path,
    codex_home: &Path,
    cwd: &Path,
    startup_model_hint: &str,
    first_prompt: &str,
    follow_up_prompt: &str,
    follow_up_text: &str,
    extra_env: HashMap<String, String>,
    mut first_response_started_rx: watch::Receiver<bool>,
) -> Result<String> {
    let mut env = HashMap::new();
    env.insert("CODEX_HOME".to_string(), codex_home.display().to_string());
    env.insert("RUST_LOG".to_string(), "trace".to_string());
    env.extend(extra_env);

    let log_dir = codex_home.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let args = codex_session_args(cwd, &log_dir);

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
    let (interrupt_ready_tx, mut interrupt_ready_rx) = watch::channel(false);
    let (follow_up_visible_tx, mut follow_up_visible_rx) = watch::channel(false);
    let first_prompt = first_prompt.to_string();
    let follow_up_prompt = follow_up_prompt.to_string();
    let startup_model_hint = startup_model_hint.to_string();
    let follow_up_output = follow_up_text.to_string();

    let input_task = tokio::spawn(async move {
        let _ = wait_for_watch_true(
            &mut startup_ready_rx,
            "startup readiness",
            Duration::from_secs(20),
        )
        .await;
        sleep(Duration::from_millis(600)).await;

        type_text_with_stabilization(&writer_for_input, &first_prompt).await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;

        wait_for_watch_true(
            &mut first_response_started_rx,
            "first response stream",
            Duration::from_secs(10),
        )
        .await?;
        sleep(Duration::from_millis(300)).await;
        let _ = writer_for_input.send(vec![27]).await;

        wait_for_watch_true(
            &mut interrupt_ready_rx,
            "interrupt guidance",
            Duration::from_secs(10),
        )
        .await?;
        sleep(Duration::from_millis(300)).await;

        if follow_up_prompt.is_empty() {
            sleep(Duration::from_millis(400)).await;
        } else {
            type_text_with_stabilization(&writer_for_input, &follow_up_prompt).await;
            sleep(Duration::from_millis(120)).await;
            let _ = writer_for_input.send(vec![b'\r']).await;

            let _ = wait_for_watch_true(
                &mut follow_up_visible_rx,
                "follow-up assistant output",
                Duration::from_secs(10),
            )
            .await;
            sleep(Duration::from_millis(400)).await;
        }

        type_text_with_stabilization(&writer_for_input, "/quit").await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;

        Ok::<(), anyhow::Error>(())
    });

    let exit_code_result = timeout(Duration::from_secs(40), async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);
                        if !*startup_ready_tx.borrow()
                            && !startup_model_hint.is_empty()
                            && output
                                .windows(startup_model_hint.len())
                                .any(|window| window == startup_model_hint.as_bytes())
                        {
                            let _ = startup_ready_tx.send(true);
                        }
                        if !*interrupt_ready_tx.borrow()
                            && output
                                .windows(INTERRUPTED_HINT.len())
                                .any(|window| window == INTERRUPTED_HINT.as_bytes())
                        {
                            let _ = interrupt_ready_tx.send(true);
                        }
                        if !*follow_up_visible_tx.borrow()
                            && !follow_up_output.is_empty()
                            && output
                                .windows(follow_up_output.len())
                                .any(|window| window == follow_up_output.as_bytes())
                        {
                            let _ = follow_up_visible_tx.send(true);
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

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            let output = String::from_utf8_lossy(&output);
            anyhow::bail!("timed out waiting for codex CLI to exit; output: {output}");
        }
    };

    if !input_task.is_finished() {
        input_task.abort();
    } else {
        input_task
            .await
            .context("join interrupt-sequence input task")??;
    }

    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    anyhow::ensure!(
        exit_code == 0 || exit_code == 130,
        "unexpected exit code from codex: {exit_code}; output: {output}"
    );

    Ok(output)
}

async fn run_codex_resume_follow_up_sequence(
    codex_cli: &Path,
    codex_home: &Path,
    cwd: &Path,
    startup_model_hint: &str,
    follow_up_prompt: &str,
    follow_up_text: &str,
    extra_env: HashMap<String, String>,
) -> Result<String> {
    let mut env = HashMap::new();
    env.insert("CODEX_HOME".to_string(), codex_home.display().to_string());
    env.insert("RUST_LOG".to_string(), "trace".to_string());
    env.extend(extra_env);

    let log_dir = codex_home.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let mut args = vec!["resume".to_string(), "--last".to_string()];
    args.extend(codex_session_args(cwd, &log_dir));

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
    let (follow_up_visible_tx, mut follow_up_visible_rx) = watch::channel(false);
    let startup_model_hint = startup_model_hint.to_string();
    let follow_up_prompt = follow_up_prompt.to_string();
    let follow_up_output = follow_up_text.to_string();

    let input_task = tokio::spawn(async move {
        wait_for_watch_true(
            &mut startup_ready_rx,
            "resume startup readiness",
            Duration::from_secs(20),
        )
        .await?;
        sleep(Duration::from_millis(600)).await;

        type_text_with_stabilization(&writer_for_input, &follow_up_prompt).await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;

        wait_for_watch_true(
            &mut follow_up_visible_rx,
            "resumed follow-up assistant output",
            Duration::from_secs(10),
        )
        .await?;
        sleep(Duration::from_millis(400)).await;

        type_text_with_stabilization(&writer_for_input, "/quit").await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;

        Ok::<(), anyhow::Error>(())
    });

    let exit_code_result = timeout(Duration::from_secs(40), async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);
                        if !*startup_ready_tx.borrow()
                            && !startup_model_hint.is_empty()
                            && output
                                .windows(startup_model_hint.len())
                                .any(|window| window == startup_model_hint.as_bytes())
                        {
                            let _ = startup_ready_tx.send(true);
                        }
                        if !*follow_up_visible_tx.borrow()
                            && !follow_up_output.is_empty()
                            && output
                                .windows(follow_up_output.len())
                                .any(|window| window == follow_up_output.as_bytes())
                        {
                            let _ = follow_up_visible_tx.send(true);
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

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            let output = String::from_utf8_lossy(&output);
            anyhow::bail!("timed out waiting for codex resume to exit; output: {output}");
        }
    };

    if !input_task.is_finished() {
        input_task.abort();
    } else {
        input_task
            .await
            .context("join resumed interrupt-sequence input task")??;
    }

    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    anyhow::ensure!(
        exit_code == 0 || exit_code == 130,
        "unexpected exit code from codex resume: {exit_code}; output: {output}"
    );

    Ok(output)
}

fn codex_session_args(cwd: &Path, log_dir: &Path) -> Vec<String> {
    vec![
        "--no-alt-screen".to_string(),
        "-C".to_string(),
        cwd.display().to_string(),
        "-c".to_string(),
        "analytics.enabled=false".to_string(),
        "-c".to_string(),
        format!("log_dir={}", log_dir.display()),
    ]
}

async fn wait_for_watch_true(
    rx: &mut watch::Receiver<bool>,
    label: &str,
    max_wait: Duration,
) -> Result<()> {
    timeout(max_wait, async {
        loop {
            if *rx.borrow() {
                return Ok(());
            }
            rx.changed()
                .await
                .with_context(|| format!("{label} channel closed"))?;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for {label}"))?
}

async fn type_text_with_stabilization(writer: &tokio::sync::mpsc::Sender<Vec<u8>>, text: &str) {
    for byte in text.bytes() {
        let _ = writer.send(vec![byte]).await;
        sleep(Duration::from_millis(30)).await;
    }

    if !text.is_empty() {
        let _ = writer.send(vec![b' ']).await;
        sleep(Duration::from_millis(30)).await;
        let _ = writer.send(vec![127]).await;
        sleep(Duration::from_millis(30)).await;
    }
}

fn find_codex_cli(cwd: &Path) -> Option<PathBuf> {
    if let Some(path) = sibling_test_binary("codex") {
        return Some(path);
    }

    if let Ok(path) = codex_utils_cargo_bin::cargo_bin("codex") {
        return Some(path);
    }

    let fallback = debug_target_binary(cwd, "codex");
    if fallback.is_file() {
        return Some(fallback);
    }

    if ensure_fallback_codex_binary_is_built(cwd).is_ok() {
        return sibling_test_binary("codex").or_else(|| {
            let fallback = debug_target_binary(cwd, "codex");
            fallback.is_file().then_some(fallback)
        });
    }

    None
}

fn ensure_fallback_codex_binary_is_built(repo_root: &Path) -> Result<()> {
    static BUILD_RESULT: OnceLock<Result<()>> = OnceLock::new();
    let result = BUILD_RESULT.get_or_init(|| {
        let status = Command::new("cargo")
            .arg("build")
            .arg("-p")
            .arg("codex-cli")
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

fn sibling_test_binary(binary: &str) -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let profile_dir = current_exe.parent()?.parent()?;
    let candidate = profile_dir.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

fn debug_target_binary(repo_root: &Path, binary: &str) -> PathBuf {
    repo_root
        .join("codex-rs")
        .join("target")
        .join("debug")
        .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX))
}
