#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Usage: ./install.sh [--help]

Builds the release `codex` binary and installs it into:
  $CODEX_INSTALL_DIR (if set), otherwise the directory of the currently
  resolved `codex` command when writable, else ~/.cargo/bin

Environment knobs:
  CODEX_MIN_FREE_KB         Minimum free space before build (default: 3145728)
  CODEX_BUILD_MODE          fast (default) or max
  CODEX_BUILD_JOBS          Cargo build jobs (default: auto)
  CODEX_BUILD_INCREMENTAL   auto (default), always, never
  CODEX_REBUILD             if-missing (default), always, never
  CODEX_SOURCE_BIN          Prebuilt binary path to install (skips build unless CODEX_REBUILD=always)
  CODEX_INSTALL_LOCK_DIR    Lock dir to prevent concurrent installers
  CARGO_TARGET_DIR          Build cache/output directory (default: codex-rs/target)
                           Set this to a path on another disk when local space is tight.
EOF
}

if [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$ROOT_DIR"

lock_dir=${CODEX_INSTALL_LOCK_DIR:-"$ROOT_DIR/.install-codex.lock"}
lock_pid_file="$lock_dir/pid"
if mkdir "$lock_dir" 2>/dev/null; then
    echo "$$" >"$lock_pid_file"
else
    stale_pid=""
    if [ -f "$lock_pid_file" ]; then
        stale_pid=$(cat "$lock_pid_file" 2>/dev/null || true)
    fi
    if [ -n "$stale_pid" ] && kill -0 "$stale_pid" 2>/dev/null; then
        echo "error: install.sh is already running under pid $stale_pid" >&2
        echo "wait for that run to finish, or stop it before retrying." >&2
        exit 1
    fi

    rm -rf "$lock_dir"
    mkdir "$lock_dir"
    echo "$$" >"$lock_pid_file"
fi
cleanup_lock() {
    rm -rf "$lock_dir"
}
trap cleanup_lock EXIT INT TERM

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required but was not found in PATH" >&2
    exit 1
fi

target_dir=${CARGO_TARGET_DIR:-"$ROOT_DIR/codex-rs/target"}
case "$target_dir" in
    /*)
        ;;
    *)
        target_dir="$ROOT_DIR/$target_dir"
        ;;
esac
build_stamp_file="$target_dir/release/.codex-install-build-stamp"

existing_dir_for_path() {
    path=$1
    while [ ! -e "$path" ]; do
        parent=$(dirname -- "$path")
        if [ "$parent" = "$path" ]; then
            break
        fi
        path=$parent
    done

    if [ -d "$path" ]; then
        printf '%s\n' "$path"
    else
        dirname -- "$path"
    fi
}

collect_source_files() {
    find "$ROOT_DIR/codex-rs" \
        -path "$ROOT_DIR/codex-rs/target" -prune -o \
        -path "$target_dir" -prune -o \
        -type f -print 2>/dev/null || true
    if [ -d "$ROOT_DIR/.cargo" ]; then
        find "$ROOT_DIR/.cargo" -type f -print 2>/dev/null || true
    fi
    if [ -f "$ROOT_DIR/rust-toolchain.toml" ]; then
        printf '%s\n' "$ROOT_DIR/rust-toolchain.toml"
    fi
    if [ -f "$ROOT_DIR/rust-toolchain" ]; then
        printf '%s\n' "$ROOT_DIR/rust-toolchain"
    fi
}

compute_source_fingerprint() {
    source_listing=$(
        collect_source_files | LC_ALL=C sort
    )
    if [ -z "$source_listing" ]; then
        printf '0:0\n'
        return
    fi

    printf '%s\n' "$source_listing" | while IFS= read -r path; do
        [ -n "$path" ] || continue
        set -- $(cksum < "$path")
        rel_path=${path#$ROOT_DIR/}
        printf '%s %s %s\n' "$1" "$2" "$rel_path"
    done | cksum | awk '{print $1 ":" $2}'
}

read_build_stamp_value() {
    key=$1
    if [ ! -f "$build_stamp_file" ]; then
        return 1
    fi
    sed -n "s/^$key=//p" "$build_stamp_file" | head -n 1
}

write_build_stamp() {
    mkdir -p "$(dirname -- "$build_stamp_file")"
    {
        printf 'fingerprint=%s\n' "$current_source_fingerprint"
        printf 'build_mode=%s\n' "$build_mode"
        printf 'build_incremental=%s\n' "$resolved_build_incremental"
        printf 'build_rustflags=%s\n' "$build_rustflags"
    } >"$build_stamp_file"
}

stale_reason=""
current_source_fingerprint=""
is_default_source_stale() {
    bin_path=$1
    stale_reason=""
    current_source_fingerprint=""

    if [ ! -x "$bin_path" ]; then
        stale_reason="binary missing"
        current_source_fingerprint=$(compute_source_fingerprint)
        return 0
    fi

    bin_version=$("$bin_path" --version 2>/dev/null || true)
    git_describe=$(git -C "$ROOT_DIR" describe --tags --always 2>/dev/null || true)
    if [ -n "$git_describe" ] && [ -n "$bin_version" ]; then
        case "$bin_version" in
            *"$git_describe"*)
                ;;
            *)
                stale_reason="binary version '$bin_version' does not match git describe '$git_describe'"
                current_source_fingerprint=$(compute_source_fingerprint)
                return 0
                ;;
        esac
    fi

    if [ -f "$build_stamp_file" ]; then
        current_source_fingerprint=$(compute_source_fingerprint)
        recorded_fingerprint=$(read_build_stamp_value fingerprint || true)
        recorded_build_mode=$(read_build_stamp_value build_mode || true)
        recorded_build_incremental=$(read_build_stamp_value build_incremental || true)
        recorded_build_rustflags=$(read_build_stamp_value build_rustflags || true)

        if [ "$recorded_fingerprint" != "$current_source_fingerprint" ]; then
            stale_reason="source fingerprint changed"
            return 0
        fi
        if [ "$recorded_build_mode" != "$build_mode" ]; then
            stale_reason="build mode changed from '$recorded_build_mode' to '$build_mode'"
            return 0
        fi
        if [ "$recorded_build_incremental" != "$resolved_build_incremental" ]; then
            stale_reason="incremental build setting changed"
            return 0
        fi
        if [ "$recorded_build_rustflags" != "$build_rustflags" ]; then
            stale_reason="RUSTFLAGS changed"
            return 0
        fi

        return 1
    fi

    newer_source=$(find "$ROOT_DIR/codex-rs" \
        -path "$ROOT_DIR/codex-rs/target" -prune -o \
        -path "$target_dir" -prune -o \
        -type f -newer "$bin_path" -print -quit 2>/dev/null || true)
    if [ -n "$newer_source" ]; then
        stale_reason="newer source detected at ${newer_source#$ROOT_DIR/}"
        current_source_fingerprint=$(compute_source_fingerprint)
        return 0
    fi

    return 1
}

build_rustflags=${RUSTFLAGS:-}
if [ -n "$build_rustflags" ]; then
    build_rustflags="$build_rustflags -Cdebuginfo=0"
else
    build_rustflags="-Cdebuginfo=0"
fi

build_mode=${CODEX_BUILD_MODE:-fast}
case "$build_mode" in
    fast)
        profile_overrides="CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16"
        ;;
    max)
        profile_overrides=""
        ;;
    *)
        echo "error: CODEX_BUILD_MODE must be one of: fast, max" >&2
        exit 1
        ;;
esac

build_incremental_mode=${CODEX_BUILD_INCREMENTAL:-auto}
case "$build_incremental_mode" in
    auto)
        if [ "$build_mode" = "fast" ]; then
            resolved_build_incremental=1
        else
            resolved_build_incremental=0
        fi
        ;;
    always)
        resolved_build_incremental=1
        ;;
    never)
        resolved_build_incremental=0
        ;;
    *)
        echo "error: CODEX_BUILD_INCREMENTAL must be one of: auto, always, never" >&2
        exit 1
        ;;
esac

default_source_bin="$target_dir/release/codex"
source_bin=${CODEX_SOURCE_BIN:-"$default_source_bin"}
rebuild_mode=${CODEX_REBUILD:-if-missing}
build_reason=""
case "$rebuild_mode" in
    if-missing)
        should_build=0
        build_reason=""
        if [ ! -x "$source_bin" ]; then
            should_build=1
            build_reason="binary missing"
        fi
        if [ "$source_bin" != "$default_source_bin" ]; then
            should_build=0
            build_reason=""
        elif is_default_source_stale "$source_bin"; then
            should_build=1
            build_reason=$stale_reason
        fi
        ;;
    always)
        should_build=1
        source_bin="$default_source_bin"
        build_reason="CODEX_REBUILD=always"
        current_source_fingerprint=$(compute_source_fingerprint)
        ;;
    never)
        should_build=0
        build_reason=""
        ;;
    *)
        echo "error: CODEX_REBUILD must be one of: if-missing, always, never" >&2
        exit 1
        ;;
esac

if [ "$should_build" -eq 0 ] && [ ! -x "$source_bin" ]; then
    echo "error: CODEX_REBUILD=$rebuild_mode but built binary is missing at $source_bin" >&2
    exit 1
fi

if [ "$should_build" -eq 1 ]; then
    required_kb=${CODEX_MIN_FREE_KB:-3145728}
    space_check_dir=$(existing_dir_for_path "$target_dir")
    available_kb=$(df -Pk "$space_check_dir" | awk 'NR==2 {print $4}')
    if [ "$available_kb" -lt "$required_kb" ]; then
        echo "Low free disk space detected for build target (${available_kb} KB at $space_check_dir)." >&2
        echo "Running cargo clean for CARGO_TARGET_DIR=$target_dir..." >&2
        CARGO_TARGET_DIR="$target_dir" cargo clean --manifest-path "$ROOT_DIR/codex-rs/Cargo.toml" >/dev/null 2>&1 || true
        available_kb=$(df -Pk "$space_check_dir" | awk 'NR==2 {print $4}')
    fi
    if [ "$available_kb" -lt "$required_kb" ]; then
        echo "error: not enough free disk space to build codex release" >&2
        echo "available: ${available_kb} KB, required: ${required_kb} KB" >&2
        echo "target dir: $target_dir" >&2
        echo "free space and retry, or point CARGO_TARGET_DIR at a larger disk, for example:" >&2
        echo "  CARGO_TARGET_DIR=/path/on/larger/disk/codex-target ./install.sh" >&2
        echo "You can also lower the threshold with CODEX_MIN_FREE_KB." >&2
        exit 1
    fi

    if [ -n "${CODEX_BUILD_JOBS:-}" ]; then
        build_jobs=$CODEX_BUILD_JOBS
    elif [ "$available_kb" -lt 5242880 ]; then
        build_jobs=1
    else
        build_jobs=4
    fi
    if [ "$build_jobs" -lt 1 ] 2>/dev/null; then
        echo "error: CODEX_BUILD_JOBS must be >= 1" >&2
        exit 1
    fi

    if [ -n "$build_reason" ]; then
        echo "Building codex release binary (mode=$build_mode jobs=$build_jobs incremental=$resolved_build_incremental target=$target_dir reason=$build_reason)..."
    else
        echo "Building codex release binary (mode=$build_mode jobs=$build_jobs incremental=$resolved_build_incremental target=$target_dir)..."
    fi
    if [ -n "$profile_overrides" ]; then
        env $profile_overrides CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL="$resolved_build_incremental" \
            CARGO_TARGET_DIR="$target_dir" RUSTFLAGS="$build_rustflags" cargo build --locked \
            --manifest-path "$ROOT_DIR/codex-rs/Cargo.toml" \
            -p codex-cli --bin codex --release -j "$build_jobs"
    else
        CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL="$resolved_build_incremental" \
            CARGO_TARGET_DIR="$target_dir" RUSTFLAGS="$build_rustflags" cargo build --locked \
            --manifest-path "$ROOT_DIR/codex-rs/Cargo.toml" \
            -p codex-cli --bin codex --release -j "$build_jobs"
    fi
    if [ -z "$current_source_fingerprint" ]; then
        current_source_fingerprint=$(compute_source_fingerprint)
    fi
    if [ "$source_bin" = "$default_source_bin" ]; then
        write_build_stamp
    fi
else
    echo "Skipping build (CODEX_REBUILD=$rebuild_mode, using existing binary)..."
fi

if [ ! -x "$source_bin" ]; then
    echo "error: build completed but binary not found at $source_bin" >&2
    exit 1
fi

resolved_codex=$(command -v codex 2>/dev/null || true)
if [ -n "${CODEX_INSTALL_DIR:-}" ]; then
    dest_dir=$CODEX_INSTALL_DIR
elif [ -n "$resolved_codex" ] && [ -w "$(dirname -- "$resolved_codex")" ]; then
    dest_dir=$(dirname -- "$resolved_codex")
else
    dest_dir="$HOME/.cargo/bin"
fi
mkdir -p "$dest_dir"
install -m 0755 "$source_bin" "$dest_dir/codex"

echo "Installed: $dest_dir/codex"
"$dest_dir/codex" --version

resolved_after=$(command -v codex 2>/dev/null || true)
if [ "$resolved_after" != "$dest_dir/codex" ]; then
    echo "warning: shell resolves codex to: ${resolved_after:-<not found>}" >&2
    echo "         installed binary is: $dest_dir/codex" >&2
fi
