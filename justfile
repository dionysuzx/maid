set dotenv-load

maid_home := env_var_or_default("MAID_HOME", env_var("HOME") + "/.maid")
config_file := maid_home + "/config.toml"
pid_file := maid_home + "/maid.pid"
log_file := maid_home + "/maid.log"
maid_bin := justfile_directory() + "/target/debug/maid"

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
    cargo build

    RUST_LOG="${RUST_LOG:-maid=info}" nohup setsid "{{maid_bin}}" </dev/null >>"{{log_file}}" 2>&1 &
    launcher_pid="$!"

    for _ in {1..10}; do
        if just status >/dev/null 2>&1; then
            pid="$(<"{{pid_file}}")"
            echo "maid is running with pid $pid"
            echo "logs: just logs"
            exit 0
        fi
        sleep 0.2
    done

    if kill -0 "$launcher_pid" 2>/dev/null; then
        kill "$launcher_pid" 2>/dev/null || true
    fi

    echo "maid failed to start; see {{log_file}}"
    exit 1

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

    is_maid_pid() {
        [[ "$1" =~ ^[0-9]+$ ]] || return 1
        local target cmdline
        target="$(readlink "/proc/$1/exe" 2>/dev/null || true)"
        cmdline="$(tr '\0' '\n' <"/proc/$1/cmdline" 2>/dev/null | head -n 1 || true)"
        [[ "$target" == "{{maid_bin}}" || "$cmdline" == "{{maid_bin}}" ]]
    }

    if [[ ! -f "{{pid_file}}" ]]; then
        echo "maid is not running"
        exit 1
    fi

    pid="$(<"{{pid_file}}")"
    if is_maid_pid "$pid"; then
        echo "maid is running with pid $pid"
        exit 0
    fi

    rm -f "{{pid_file}}"
    echo "maid is not running"
    exit 1

stop:
    #!/usr/bin/env bash
    set -euo pipefail

    is_maid_pid() {
        [[ "$1" =~ ^[0-9]+$ ]] || return 1
        local target cmdline
        target="$(readlink "/proc/$1/exe" 2>/dev/null || true)"
        cmdline="$(tr '\0' '\n' <"/proc/$1/cmdline" 2>/dev/null | head -n 1 || true)"
        [[ "$target" == "{{maid_bin}}" || "$cmdline" == "{{maid_bin}}" ]]
    }

    wait_for_stop() {
        for _ in {1..20}; do
            if ! kill -0 "$pid" 2>/dev/null; then
                rm -f "{{pid_file}}"
                echo "maid stopped"
                exit 0
            fi
            sleep 0.25
        done

        echo "maid did not stop after 5 seconds; pid still running: $pid"
        exit 1
    }

    if [[ ! -f "{{pid_file}}" ]]; then
        echo "maid is not running"
        exit 0
    fi

    pid="$(<"{{pid_file}}")"
    if ! is_maid_pid "$pid"; then
        rm -f "{{pid_file}}"
        echo "maid is not running"
        exit 0
    fi

    kill "$pid" 2>/dev/null || true
    wait_for_stop
