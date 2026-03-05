use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::time::Duration;
use std::time::Instant;

const ANSI_BLUE: &str = "\x1b[94m";
const ANSI_GRAY: &str = "\x1b[90m";
const ANSI_RESET: &str = "\x1b[0m";

const GITHUB_COPILOT_OAUTH_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const GITHUB_COPILOT_SCOPE: &str = "read:user";
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_EXPIRES_IN_SECONDS: u64 = 900;

#[derive(Debug, Clone)]
pub struct GithubCopilotDeviceAuthOptions {
    pub client_id: String,
    pub device_code_url: String,
    pub access_token_url: String,
    pub user_agent: String,
}

impl Default for GithubCopilotDeviceAuthOptions {
    fn default() -> Self {
        let version = env!("CARGO_PKG_VERSION");
        Self {
            client_id: GITHUB_COPILOT_OAUTH_CLIENT_ID.to_string(),
            device_code_url: GITHUB_DEVICE_CODE_URL.to_string(),
            access_token_url: GITHUB_ACCESS_TOKEN_URL.to_string(),
            user_agent: format!("codex/{version}"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_poll_interval_seconds")]
    interval: u64,
    #[serde(default = "default_expires_in_seconds")]
    expires_in: u64,
}

#[derive(Debug, Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
    scope: &'a str,
}

#[derive(Debug, Serialize)]
struct AccessTokenRequest<'a> {
    client_id: &'a str,
    device_code: &'a str,
    grant_type: &'static str,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

fn default_poll_interval_seconds() -> u64 {
    DEFAULT_POLL_INTERVAL_SECONDS
}

fn default_expires_in_seconds() -> u64 {
    DEFAULT_EXPIRES_IN_SECONDS
}

fn format_error_details(error: &str, error_description: Option<&str>) -> String {
    match error_description
        .map(str::trim)
        .filter(|description| !description.is_empty())
    {
        Some(description) => format!("{error}: {description}"),
        None => error.to_string(),
    }
}

fn print_device_code_prompt(verification_uri: &str, user_code: &str, expires_in_seconds: u64) {
    let version = env!("CARGO_PKG_VERSION");
    let expires_minutes = (expires_in_seconds.max(60)) / 60;
    println!(
        "\nWelcome to Codex [v{ANSI_GRAY}{version}{ANSI_RESET}]\n{ANSI_GRAY}OpenAI's command-line coding agent{ANSI_RESET}\n\
\nFollow these steps to sign in with GitHub Copilot:\n\
\n1. Open this link in your browser and authorize the Codex CLI\n   {ANSI_BLUE}{verification_uri}{ANSI_RESET}\n\
\n2. Enter this one-time code {ANSI_GRAY}(expires in about {expires_minutes} minutes){ANSI_RESET}\n   {ANSI_BLUE}{user_code}{ANSI_RESET}\n\
\n{ANSI_GRAY}Device codes are a common phishing target. Never share this code.{ANSI_RESET}\n",
    );
}

async fn request_device_code(
    client: &reqwest::Client,
    options: &GithubCopilotDeviceAuthOptions,
) -> io::Result<DeviceCodeResponse> {
    let response = client
        .post(&options.device_code_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", &options.user_agent)
        .json(&DeviceCodeRequest {
            client_id: &options.client_id,
            scope: GITHUB_COPILOT_SCOPE,
        })
        .send()
        .await
        .map_err(io::Error::other)?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(io::Error::other)?;
        return Err(io::Error::other(format!(
            "GitHub device code request failed with status {status}: {body}"
        )));
    }

    response.json().await.map_err(io::Error::other)
}

fn next_poll_sleep_duration(deadline: Instant, interval: Duration) -> io::Result<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "GitHub device authorization timed out before completion",
        ));
    }

    Ok(interval.min(deadline.saturating_duration_since(now)))
}

async fn poll_for_access_token(
    client: &reqwest::Client,
    options: &GithubCopilotDeviceAuthOptions,
    device_code: &DeviceCodeResponse,
) -> io::Result<String> {
    let mut poll_interval = Duration::from_secs(device_code.interval);
    let deadline = Instant::now() + Duration::from_secs(device_code.expires_in);

    loop {
        let response = client
            .post(&options.access_token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("User-Agent", &options.user_agent)
            .json(&AccessTokenRequest {
                client_id: &options.client_id,
                device_code: &device_code.device_code,
                grant_type: GITHUB_DEVICE_CODE_GRANT_TYPE,
            })
            .send()
            .await
            .map_err(io::Error::other)?;

        let status = response.status();
        let body = response.text().await.map_err(io::Error::other)?;
        let parsed = serde_json::from_str::<AccessTokenResponse>(&body).ok();

        if let Some(parsed) = parsed {
            if let Some(access_token) = parsed.access_token {
                return Ok(access_token);
            }

            if let Some(error) = parsed.error.as_deref() {
                match error {
                    "authorization_pending" => {
                        let sleep_for = next_poll_sleep_duration(deadline, poll_interval)?;
                        tokio::time::sleep(sleep_for).await;
                    }
                    "slow_down" => {
                        poll_interval = parsed
                            .interval
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| poll_interval + Duration::from_secs(5));
                        let sleep_for = next_poll_sleep_duration(deadline, poll_interval)?;
                        tokio::time::sleep(sleep_for).await;
                    }
                    "expired_token" => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "GitHub device authorization expired; run `codex login --github-copilot` again",
                        ));
                    }
                    "access_denied" => {
                        let details =
                            format_error_details(error, parsed.error_description.as_deref());
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("GitHub device authorization denied: {details}"),
                        ));
                    }
                    _ => {
                        let details =
                            format_error_details(error, parsed.error_description.as_deref());
                        return Err(io::Error::other(format!(
                            "GitHub device authorization failed: {details}"
                        )));
                    }
                }

                continue;
            }
        }

        if !status.is_success() {
            return Err(io::Error::other(format!(
                "GitHub access token request failed with status {status}: {body}"
            )));
        }

        return Err(io::Error::other(
            "GitHub access token response did not include an access token",
        ));
    }
}

pub async fn run_github_copilot_login() -> io::Result<String> {
    run_github_copilot_login_with_options(GithubCopilotDeviceAuthOptions::default()).await
}

pub async fn run_github_copilot_login_with_options(
    options: GithubCopilotDeviceAuthOptions,
) -> io::Result<String> {
    let client = reqwest::Client::new();
    let device_code = request_device_code(&client, &options).await?;
    print_device_code_prompt(
        &device_code.verification_uri,
        &device_code.user_code,
        device_code.expires_in,
    );
    poll_for_access_token(&client, &options, &device_code).await
}
