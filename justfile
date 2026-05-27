set dotenv-load

default:
    just --list

fmt:
    cargo fmt --all

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

check: fmt clippy test

init:
    #!/usr/bin/env bash
    set -euo pipefail

    maid_home="${MAID_HOME:-$HOME/.maid}"
    config_file="$maid_home/config.toml"
    mkdir -p "$maid_home"

    if [[ -e "$config_file" ]]; then
        echo "$config_file already exists"
        exit 0
    fi

    {
        printf '%s\n' '# Fill in the GitHub bot account login and master accounts before starting Maid.'
        printf '%s\n' 'bot_login = ""'
        printf '%s\n' 'master_accounts = ["dionysuzx"]'
    } >"$config_file"

    echo "created $config_file"
    echo "edit bot_login and master_accounts, then run: just start"

start:
    #!/usr/bin/env bash
    set -euo pipefail

    maid_home="${MAID_HOME:-$HOME/.maid}"
    pid_file="$maid_home/maid.pid"
    log_file="$maid_home/maid.log"
    mkdir -p "$maid_home"

    if [[ -f "$pid_file" ]]; then
        pid="$(<"$pid_file")"
        if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
            echo "maid is already running with pid $pid"
            echo "logs: just logs"
            exit 0
        fi
        rm -f "$pid_file"
    fi

    cargo build
    RUST_LOG="${RUST_LOG:-maid=info}" nohup target/debug/maid >>"$log_file" 2>&1 &
    pid="$!"
    echo "$pid" >"$pid_file"

    sleep 1
    if ! kill -0 "$pid" 2>/dev/null; then
        rm -f "$pid_file"
        echo "maid failed to start; see $log_file"
        exit 1
    fi

    echo "maid started with pid $pid"
    echo "logs: just logs"

dev:
    RUST_LOG="${RUST_LOG:-maid=info}" cargo run

update:
    just stop
    git pull --ff-only origin main
    just start

logs:
    #!/usr/bin/env bash
    set -euo pipefail

    maid_home="${MAID_HOME:-$HOME/.maid}"
    log_file="$maid_home/maid.log"
    mkdir -p "$maid_home"
    touch "$log_file"
    tail -n "${LINES:-80}" -f "$log_file"

status:
    #!/usr/bin/env bash
    set -euo pipefail

    maid_home="${MAID_HOME:-$HOME/.maid}"
    pid_file="$maid_home/maid.pid"
    if [[ ! -f "$pid_file" ]]; then
        echo "maid is not running"
        exit 1
    fi

    pid="$(<"$pid_file")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
        echo "maid is running with pid $pid"
        exit 0
    fi

    rm -f "$pid_file"
    echo "maid is not running"
    exit 1

stop:
    #!/usr/bin/env bash
    set -euo pipefail

    maid_home="${MAID_HOME:-$HOME/.maid}"
    pid_file="$maid_home/maid.pid"
    if [[ ! -f "$pid_file" ]]; then
        echo "maid is not running"
        exit 0
    fi

    pid="$(<"$pid_file")"
    if ! [[ "$pid" =~ ^[0-9]+$ ]] || ! kill -0 "$pid" 2>/dev/null; then
        rm -f "$pid_file"
        echo "maid is not running"
        exit 0
    fi

    kill "$pid"
    for _ in {1..20}; do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$pid_file"
            echo "maid stopped"
            exit 0
        fi
        sleep 0.25
    done

    echo "maid did not stop after 5 seconds; pid $pid is still running"
    exit 1

health:
    curl --fail --silent --show-error http://127.0.0.1:3000/healthz
