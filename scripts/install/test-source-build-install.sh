#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

fail() {
    echo "error: $1" >&2
    exit 1
}

assert_eq() {
    expected=$1
    actual=$2
    context=$3
    if [ "$expected" != "$actual" ]; then
        fail "$context: expected '$expected', got '$actual'"
    fi
}

assert_file_exists() {
    if [ ! -e "$1" ]; then
        fail "expected file to exist: $1"
    fi
}

make_fake_workspace() {
    workspace=$1

    cp "$ROOT_DIR/install.sh" "$workspace/install.sh"
    chmod +x "$workspace/install.sh"
    mkdir -p "$workspace/codex-rs/src" "$workspace/fake-bin"

    cat >"$workspace/codex-rs/Cargo.toml" <<'EOF'
[package]
name = "codex-cli"
version = "0.0.0"
edition = "2021"
EOF

    cat >"$workspace/codex-rs/src/main.rs" <<'EOF'
fn main() {}
EOF

    cat >"$workspace/fake-bin/cargo" <<'EOF'
#!/bin/sh
set -eu

test_log=${TEST_LOG:?}
manifest_path=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --manifest-path)
            manifest_path=$2
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

if [ -z "$manifest_path" ]; then
    echo "missing --manifest-path" >&2
    exit 1
fi

manifest_dir=$(dirname -- "$manifest_path")
target_dir=${CARGO_TARGET_DIR:-"$manifest_dir/target"}
mkdir -p "$target_dir/release"
printf 'incremental=%s\n' "${CARGO_INCREMENTAL:-unset}" >>"$test_log"
cat >"$target_dir/release/codex" <<'BIN'
#!/bin/sh
echo "codex test-describe"
BIN
chmod +x "$target_dir/release/codex"
EOF
    chmod +x "$workspace/fake-bin/cargo"

    cat >"$workspace/fake-bin/git" <<'EOF'
#!/bin/sh
set -eu

if [ "${1:-}" = "-C" ]; then
    shift 2
fi

if [ "${1:-}" = "describe" ]; then
    echo "test-describe"
    exit 0
fi

echo "unexpected git invocation: $*" >&2
exit 1
EOF
    chmod +x "$workspace/fake-bin/git"
}

run_install() {
    workspace=$1
    test_log=$2
    shift 2
    (
        cd "$workspace"
        env PATH="$workspace/fake-bin:$PATH" \
            TEST_LOG="$test_log" \
            CODEX_MIN_FREE_KB=1 \
            CODEX_INSTALL_DIR="$workspace/bin" \
            "$@" \
            ./install.sh
    )
}

count_builds() {
    log_file=$1
    if [ ! -f "$log_file" ]; then
        printf '0\n'
        return
    fi
    wc -l <"$log_file" | tr -d ' '
}

assert_help_mentions_new_knobs() {
    help_output=$(sh "$ROOT_DIR/install.sh" --help)
    printf '%s\n' "$help_output" | grep "CODEX_BUILD_INCREMENTAL" >/dev/null || fail "--help should mention CODEX_BUILD_INCREMENTAL"
    printf '%s\n' "$help_output" | grep "CARGO_TARGET_DIR" >/dev/null || fail "--help should mention CARGO_TARGET_DIR"
}

test_skips_rebuild_when_source_content_is_unchanged() {
    workspace=$(mktemp -d)
    test_log="$workspace/cargo.log"
    trap 'rm -rf "$workspace"' EXIT INT TERM

    make_fake_workspace "$workspace"

    run_install "$workspace" "$test_log"
    assert_eq "1" "$(count_builds "$test_log")" "first run should build once"
    grep '^incremental=1$' "$test_log" >/dev/null || fail "fast mode should enable incremental builds"
    assert_file_exists "$workspace/bin/codex"

    run_install "$workspace" "$test_log"
    assert_eq "1" "$(count_builds "$test_log")" "second run should reuse the existing build"

    touch "$workspace/codex-rs/src/main.rs"
    run_install "$workspace" "$test_log"
    assert_eq "1" "$(count_builds "$test_log")" "touching a source file without content changes should not rebuild"

    printf '\n// content change\n' >>"$workspace/codex-rs/src/main.rs"
    run_install "$workspace" "$test_log"
    assert_eq "2" "$(count_builds "$test_log")" "changing source content should rebuild"
    assert_file_exists "$workspace/codex-rs/target/release/.codex-install-build-stamp"

    rm -rf "$workspace"
    trap - EXIT INT TERM
}

test_honors_custom_target_dir() {
    workspace=$(mktemp -d)
    test_log="$workspace/cargo.log"
    target_dir="$workspace/shared-target"
    trap 'rm -rf "$workspace"' EXIT INT TERM

    make_fake_workspace "$workspace"

    run_install "$workspace" "$test_log" CARGO_TARGET_DIR="$target_dir" CODEX_BUILD_MODE=max
    assert_eq "1" "$(count_builds "$test_log")" "custom target dir run should build once"
    grep '^incremental=0$' "$test_log" >/dev/null || fail "max mode should disable incremental builds by default"
    assert_file_exists "$target_dir/release/codex"
    assert_file_exists "$target_dir/release/.codex-install-build-stamp"

    run_install "$workspace" "$test_log" CARGO_TARGET_DIR="$target_dir" CODEX_BUILD_MODE=max
    assert_eq "1" "$(count_builds "$test_log")" "custom target dir should reuse the previous build"

    rm -rf "$workspace"
    trap - EXIT INT TERM
}

assert_help_mentions_new_knobs
test_skips_rebuild_when_source_content_is_unchanged
test_honors_custom_target_dir
