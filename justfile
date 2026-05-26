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

run:
    RUST_LOG="${RUST_LOG:-maid=info}" cargo run

health:
    curl --fail --silent --show-error http://127.0.0.1:3000/healthz
