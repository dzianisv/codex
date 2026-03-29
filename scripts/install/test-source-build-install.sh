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

assert_contains() {
    haystack=$1
    needle=$2
    context=$3
    printf '%s' "$haystack" | grep -F -- "$needle" >/dev/null || fail "$context: missing '$needle'"
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
command=${1:-}
if [ -n "$command" ]; then
    shift
fi
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
if [ "$command" = "clean" ]; then
    rm -rf "$target_dir"
    exit 0
fi
if [ "$command" != "build" ]; then
    echo "unexpected cargo invocation: $command" >&2
    exit 1
fi
mkdir -p "$target_dir/release"
printf 'incremental=%s\n' "${CARGO_INCREMENTAL:-unset}" >>"$test_log"
cat >"$target_dir/release/codex" <<'BIN'
#!/bin/sh
echo "codex test-describe"
BIN
chmod +x "$target_dir/release/codex"
EOF
    chmod +x "$workspace/fake-bin/cargo"

    cat >"$workspace/fake-bin/df" <<'EOF'
#!/bin/sh
set -eu

path=""
for arg in "$@"; do
    path=$arg
done

default_kb=${TEST_DF_DEFAULT_KB:-10485760}
low_path=${TEST_DF_LOW_PATH:-}
low_kb=${TEST_DF_LOW_KB:-1}
high_path=${TEST_DF_HIGH_PATH:-}
high_kb=${TEST_DF_HIGH_KB:-10485760}

available_kb=$default_kb
if [ -n "$high_path" ] && [ "$path" = "$high_path" ]; then
    available_kb=$high_kb
fi
if [ -n "$low_path" ] && [ "$path" = "$low_path" ]; then
    available_kb=$low_kb
fi

printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
printf 'testfs 9999999 0 %s 0%% %s\n' "$available_kb" "$path"
EOF
    chmod +x "$workspace/fake-bin/df"

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
    printf '%s\n' "$help_output" | grep "another disk" >/dev/null || fail "--help should explain when to move CARGO_TARGET_DIR"
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

test_uses_custom_target_dir_filesystem_for_space_check() {
    workspace=$(mktemp -d)
    test_log="$workspace/cargo.log"
    target_dir="$workspace/shared-target"
    trap 'rm -rf "$workspace"' EXIT INT TERM

    make_fake_workspace "$workspace"
    mkdir -p "$target_dir"

    run_install "$workspace" "$test_log" \
        CARGO_TARGET_DIR="$target_dir" \
        CODEX_MIN_FREE_KB=100 \
        TEST_DF_DEFAULT_KB=10 \
        TEST_DF_LOW_PATH="$workspace" \
        TEST_DF_LOW_KB=10 \
        TEST_DF_HIGH_PATH="$target_dir" \
        TEST_DF_HIGH_KB=1000
    assert_eq "1" "$(count_builds "$test_log")" "custom target dir filesystem should satisfy the space check"
    assert_file_exists "$target_dir/release/codex"

    rm -rf "$workspace"
    trap - EXIT INT TERM
}

test_low_space_error_mentions_external_target_dir() {
    workspace=$(mktemp -d)
    test_log="$workspace/cargo.log"
    trap 'rm -rf "$workspace"' EXIT INT TERM

    make_fake_workspace "$workspace"

    set +e
    output=$(
        cd "$workspace" && env PATH="$workspace/fake-bin:$PATH" \
            TEST_LOG="$test_log" \
            CODEX_INSTALL_DIR="$workspace/bin" \
            CODEX_MIN_FREE_KB=100 \
            TEST_DF_DEFAULT_KB=10 \
            TEST_DF_LOW_PATH="$workspace/codex-rs/target" \
            TEST_DF_LOW_KB=10 \
            ./install.sh 2>&1
    )
    status=$?
    set -e

    if [ "$status" -eq 0 ]; then
        fail "install should fail when the build target filesystem remains full"
    fi

    assert_contains "$output" "target dir: $workspace/codex-rs/target" "low-space error should show the target dir"
    assert_contains "$output" "CARGO_TARGET_DIR=/path/on/larger/disk/codex-target ./install.sh" "low-space error should suggest using another disk"

    rm -rf "$workspace"
    trap - EXIT INT TERM
}

assert_help_mentions_new_knobs
test_skips_rebuild_when_source_content_is_unchanged
test_honors_custom_target_dir
test_uses_custom_target_dir_filesystem_for_space_check
test_low_space_error_mentions_external_target_dir
