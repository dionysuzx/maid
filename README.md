![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust + Axum polling bot for GitHub pull requests. It runs as a
GitHub bot account, checks out eligible PRs into a local cache, runs `codex`,
and posts Codex's final answer as a normal PR comment.

Maid responds when an allowed master account mentions the bot in a PR comment.
Automatic reviews can be enabled for allowlisted repos.

## Prerequisites

- A GitHub bot account authenticated with `gh`; Maid uses that account's token.
- Rust/Cargo, `git`, `gh`, `codex`, `just`, and `nvim` on `PATH`.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init
just config
just start
```

## Configuration

Runtime config lives at `~/.maid/config.toml`. The required fields are
`bot_login` and `master_accounts`.

See [config.example.toml](config.example.toml) for every option.
