# Maid

![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust + Axum bot runner for GitHub pull requests. It polls GitHub
as a bot account, checks out eligible PRs into a local cache, runs `codex` from
that checkout, and posts Codex's final answer as a normal PR comment.

Maid handles two workflows:

- Mention requests: an allowed master account mentions the bot in a PR comment.
- Automatic reviews: an allowed master account opens a PR in an allowlisted repo.

It uses polling, not webhooks. It expects `git`, `gh`, and `codex` on `PATH`.
In-progress work gets an `eyes` reaction; completed work gets a `+1` reaction.

## Quick Start

Run setup as the GitHub bot account; Maid reads that account's token from `gh`.

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init     # create ~/.maid/config.toml
just config   # fill in bot_login and master_accounts
gh auth login
gh auth status --hostname github.com
just start    # start Maid in the background
just logs     # follow ~/.maid/maid.log
```

## Configuration

Runtime config lives at `~/.maid/config.toml`. The required fields are
`bot_login` and `master_accounts`.

- `auto_review_accounts`: master accounts eligible for automatic PR reviews;
  defaults to `master_accounts`.
- `auto_review_repos`: repositories to poll for automatic PR reviews; empty or
  omitted means mention-only mode.
- `task_limit_per_24h`: optional rolling 24-hour cap. Omit it for no limit; set
  it to `0` to pause new task starts.

See [config.example.toml](config.example.toml) for every option.

## Operations

Runtime state lives in `~/.maid`: config, logs, PID, repo cache, and the
task-start ledger. Use `just status`, `just stop`, `just restart`, or
`just update` to manage the background process.

## Development

```sh
just dev
just check
```
