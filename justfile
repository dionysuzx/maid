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
    perl -0pi -e 's/^bot_login = "maid-bot"/bot_login = ""/m; s/your-name/dionysuzx/g' "{{config_file}}"

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

    running_pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' || true)"
    if [[ -n "$running_pids" ]]; then
        echo "maid is already running without {{pid_file}}:"
        echo "$running_pids"
        echo "run: just stop"
        exit 1
    fi

    cargo build
    RUST_LOG="${RUST_LOG:-maid=info}" nohup target/debug/maid >>"{{log_file}}" 2>&1 &
    pid="$!"

    sleep 1
    if ! kill -0 "$pid" 2>/dev/null; then
        rm -f "{{pid_file}}"
        echo "maid failed to start; see {{log_file}}"
        exit 1
    fi

    if [[ ! -f "{{pid_file}}" ]]; then
        kill "$pid" 2>/dev/null || true
        echo "maid failed to write {{pid_file}}; see {{log_file}}"
        exit 1
    fi

    daemon_pid="$(<"{{pid_file}}")"
    if [[ "$daemon_pid" != "$pid" ]]; then
        kill "$pid" 2>/dev/null || true
        echo "maid wrote unexpected pid $daemon_pid; see {{log_file}}"
        exit 1
    fi

    echo "maid started with pid $daemon_pid"
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
        pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' || true)"
        if [[ -n "$pids" ]]; then
            echo "maid is running without {{pid_file}}:"
            echo "$pids"
            exit 0
        fi
        echo "maid is not running"
        exit 1
    fi

    pid="$(<"{{pid_file}}")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
        echo "maid is running with pid $pid"
        exit 0
    fi

    rm -f "{{pid_file}}"
    pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' || true)"
    if [[ -n "$pids" ]]; then
        echo "maid is running without {{pid_file}}:"
        echo "$pids"
        exit 0
    fi
    echo "maid is not running"
    exit 1

stop:
    #!/usr/bin/env bash
    set -euo pipefail

    wait_for_stop() {
        local pids="$1"
        for _ in {1..20}; do
            local running=""
            for pid in $pids; do
                if kill -0 "$pid" 2>/dev/null; then
                    running="$running $pid"
                fi
            done
            if [[ -z "${running// }" ]]; then
                rm -f "{{pid_file}}"
                echo "maid stopped"
                exit 0
            fi
            sleep 0.25
        done

        echo "maid did not stop after 5 seconds; pids still running:$running"
        exit 1
    }

    if [[ ! -f "{{pid_file}}" ]]; then
        pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' || true)"
        if [[ -z "$pids" ]]; then
            echo "maid is not running"
            exit 0
        fi
        kill $pids 2>/dev/null || true
        wait_for_stop "$pids"
    fi

    pid="$(<"{{pid_file}}")"
    if ! [[ "$pid" =~ ^[0-9]+$ ]] || ! kill -0 "$pid" 2>/dev/null; then
        rm -f "{{pid_file}}"
        pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' || true)"
        if [[ -z "$pids" ]]; then
            echo "maid is not running"
            exit 0
        fi
        kill $pids 2>/dev/null || true
        wait_for_stop "$pids"
    fi

    extra_pids="$(pgrep -u "$(id -u)" -f 'target/debug/maid' | grep -v "^$pid$" || true)"
    pids="$pid ${extra_pids:-}"
    kill $pids 2>/dev/null || true
    wait_for_stop "$pids"
