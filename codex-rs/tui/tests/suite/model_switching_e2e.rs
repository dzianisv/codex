use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
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
    let codex_home = tempdir_with_catalog_and_config(&repo_root)?;

    let first_output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "gpt-5.3-codex",
        "claude",
        HashMap::new(),
    )
    .await
    .context("switch from gpt-5.3-codex to claude-opus-4.6")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        "claude-opus-4.6",
        "github-copilot",
        &first_output,
    )?;

    let second_output = run_codex_cli_with_filter(
        &codex_cli,
        codex_home.path(),
        &repo_root,
        "claude-opus-4.6",
        "gpt-5.3-codex",
        HashMap::new(),
    )
    .await
    .context("switch back from claude-opus-4.6 to gpt-5.3-codex")?;
    assert_model_and_provider_in_config(
        codex_home.path(),
        "gpt-5.3-codex",
        "github-copilot",
        &second_output,
    )?;

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
        env,
    )
    .await
    .context("attempt to switch to model_picker_enabled=false model")?;
    assert_model_and_provider_in_config(codex_home.path(), "llama3.2:latest", "ollama", &output)?;

    let requests = server.join().expect("model listing server should join")?;
    assert!(
        requests
            .iter()
            .any(|line| line.starts_with("GET /v1/models ")),
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
        env,
    )
    .await
    .context("switch model for ollama provider")?;
    assert_model_and_provider_in_config(codex_home.path(), "qwen2.5-coder:7b", "ollama", &output)?;

    let requests = server.join().expect("model listing server should join")?;
    assert!(
        requests
            .iter()
            .any(|line| line.starts_with("GET /v1/models ")),
        "expected at least one GET /v1/models request, got: {requests:?}"
    );

    Ok(())
}

fn tempdir_with_catalog_and_config(repo_root: &Path) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let source_catalog_path = codex_utils_cargo_bin::find_resource!("../core/models.json")?;
    let source_catalog = std::fs::read_to_string(&source_catalog_path)?;
    let mut source_catalog: JsonValue = serde_json::from_str(&source_catalog)?;
    let models = source_catalog
        .get_mut("models")
        .and_then(JsonValue::as_array_mut)
        .context("models array missing")?;
    let template = models.first().cloned().context("models array is empty")?;

    let gpt_model = model_from_template(&template, "gpt-5.3-codex", "gpt-5.3-codex", 0)?;
    let claude_model = model_from_template(&template, "claude-opus-4.6", "claude-opus-4.6", 1)?;
    *models = vec![gpt_model, claude_model];

    let custom_catalog_path = codex_home.path().join("catalog.json");
    std::fs::write(
        &custom_catalog_path,
        serde_json::to_string(&source_catalog)?,
    )?;

    let repo_root_display = repo_root.display();
    let catalog_display = custom_catalog_path.display();
    let config_contents = format!(
        r#"model_provider = "github-copilot"
model = "gpt-5.3-codex"
model_catalog_json = "{catalog_display}"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
}

fn tempdir_with_ollama_config(repo_root: &Path, model: &str) -> Result<TempDir> {
    let codex_home = tempfile::tempdir()?;

    let repo_root_display = repo_root.display();
    let config_contents = format!(
        r#"model_provider = "ollama"
model = "{model}"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;

    Ok(codex_home)
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
    extra_env: HashMap<String, String>,
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
    let filter = filter.to_string();
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
        sleep(Duration::from_millis(500)).await;
        type_text_with_stabilization(&writer_for_input, "/model").await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;
        sleep(Duration::from_millis(1200)).await;
        type_text_with_stabilization(&writer_for_input, &filter).await;
        sleep(Duration::from_millis(120)).await;
        let _ = writer_for_input.send(vec![b'\r']).await;
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
                        if !startup_model_hint.is_empty()
                            && !*startup_ready_tx.borrow()
                            && chunk
                                .windows(startup_model_hint.len())
                                .any(|window| window == startup_model_hint.as_bytes())
                        {
                            let _ = startup_ready_tx.send(true);
                        }
                        output.extend_from_slice(&chunk);
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
    anyhow::ensure!(
        exit_code == 0 || exit_code == 130,
        "unexpected exit code from codex: {exit_code}; output: {output}"
    );

    Ok(output)
}

fn find_codex_cli(cwd: &Path) -> Option<PathBuf> {
    if let Ok(path) = codex_utils_cargo_bin::cargo_bin("codex") {
        return Some(path);
    }
    let fallback = cwd.join("codex-rs/target/debug/codex");
    if fallback.is_file() {
        return Some(fallback);
    }
    None
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
