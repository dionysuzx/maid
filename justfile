set dotenv-load

maid_home := env_var_or_default("MAID_HOME", env_var("HOME") + "/.maid")
config_file := maid_home + "/config.toml"
pid_file := maid_home + "/maid.pid"
log_file := maid_home + "/maid.log"

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

    mkdir -p "{{maid_home}}"

    if [[ -e "{{config_file}}" ]]; then
        echo "{{config_file}} already exists"
        exit 0
    fi

    cp "{{ justfile_directory() }}/config.example.toml" "{{config_file}}"
    perl -0pi -e 's/^bot_login = "maid-bot"/bot_login = ""/m; s/master_accounts = \["your-name"\]/master_accounts = ["dionysuzx"]/m' "{{config_file}}"

    echo "created {{config_file}}"
    echo "edit bot_login and master_accounts, then run: just start"
    echo
    echo "optional config reference:"
    cat "{{ justfile_directory() }}/config.example.toml"

config:
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ ! -e "{{config_file}}" ]]; then
        just init
    fi

    nvim "{{config_file}}"

start:
    #!/usr/bin/env bash
    set -euo pipefail

    mkdir -p "{{maid_home}}"

    if [[ -f "{{pid_file}}" ]]; then
        pid="$(<"{{pid_file}}")"
        if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
            echo "maid is already running with pid $pid"
            echo "logs: just logs"
            exit 0
        fi
        rm -f "{{pid_file}}"
    fi

    cargo build
    RUST_LOG="${RUST_LOG:-maid=info}" nohup target/debug/maid >>"{{log_file}}" 2>&1 &
    pid="$!"
    echo "$pid" >"{{pid_file}}"

    sleep 1
    if ! kill -0 "$pid" 2>/dev/null; then
        rm -f "{{pid_file}}"
        echo "maid failed to start; see {{log_file}}"
        exit 1
    fi

    echo "maid started with pid $pid"
    echo "logs: just logs"

dev:
    RUST_LOG="${RUST_LOG:-maid=info}" cargo run

update:
    just stop
    git pull --ff-only origin main
    just config
    just start

restart: stop start

logs:
    #!/usr/bin/env bash
    set -euo pipefail

    mkdir -p "{{maid_home}}"
    touch "{{log_file}}"
    tail -n "${LINES:-80}" -f "{{log_file}}"

status:
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ ! -f "{{pid_file}}" ]]; then
        echo "maid is not running"
        exit 1
    fi

    pid="$(<"{{pid_file}}")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
        echo "maid is running with pid $pid"
        exit 0
    fi

    rm -f "{{pid_file}}"
    echo "maid is not running"
    exit 1

stop:
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ ! -f "{{pid_file}}" ]]; then
        echo "maid is not running"
        exit 0
    fi

    pid="$(<"{{pid_file}}")"
    if ! [[ "$pid" =~ ^[0-9]+$ ]] || ! kill -0 "$pid" 2>/dev/null; then
        rm -f "{{pid_file}}"
        echo "maid is not running"
        exit 0
    fi

    kill "$pid"
    for _ in {1..20}; do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "{{pid_file}}"
            echo "maid stopped"
            exit 0
        fi
        sleep 0.25
    done

    echo "maid did not stop after 5 seconds; pid $pid is still running"
    exit 1
