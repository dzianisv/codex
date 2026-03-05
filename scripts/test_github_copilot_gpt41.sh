#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT/codex-rs"

RUN_LIVE=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-live)
      RUN_LIVE=0
      shift
      ;;
    *)
      echo "Unknown argument: $1"
      echo "Usage: $0 [--no-live]"
      exit 2
      ;;
  esac
done

echo "[copilot-test] running mocked device-auth tests"
CARGO_INCREMENTAL=0 RUSTFLAGS='-C debuginfo=0' \
  cargo +stable test -p codex-login --test all github_copilot_device_auth -- --nocapture

if [[ "$RUN_LIVE" -eq 0 ]]; then
  echo "[copilot-test] skipped live API check (--no-live)"
  exit 0
fi

if [[ -z "${GITHUB_COPILOT_TOKEN:-}" ]]; then
  if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required to load GITHUB_COPILOT_TOKEN from auth.json files."
    echo "Install jq or export GITHUB_COPILOT_TOKEN directly."
    exit 1
  fi

  AUTH_FILES=(
    "/tmp/codex-copilot-e2e/auth.json"
    "${CODEX_HOME:-$HOME/.codex}/auth.json"
  )

  for auth_file in "${AUTH_FILES[@]}"; do
    if [[ -f "$auth_file" ]]; then
      token="$(jq -r '."GITHUB_COPILOT_TOKEN" // ."OPENAI_API_KEY" // empty' "$auth_file")"
      if [[ -n "$token" ]]; then
        GITHUB_COPILOT_TOKEN="$token"
        export GITHUB_COPILOT_TOKEN
        echo "[copilot-test] using token from $auth_file"
        break
      fi
    fi
  done
fi

if [[ -z "${GITHUB_COPILOT_TOKEN:-}" ]]; then
  echo "GITHUB_COPILOT_TOKEN is not set."
  echo "Export GITHUB_COPILOT_TOKEN or run \`codex login --github-copilot\` first."
  exit 1
fi

echo "[copilot-test] running live gpt-4.1 Copilot API check"
export RUN_GITHUB_COPILOT_LIVE_TESTS=1
CARGO_INCREMENTAL=0 RUSTFLAGS='-C debuginfo=0' \
  cargo +stable test -p codex-login --test all github_copilot_gpt41_live_chat_completion_succeeds -- --ignored --nocapture
