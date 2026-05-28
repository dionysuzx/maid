# Maid

![Maid banner](assets/maid-banner.png)

Maid is a small Rust + Axum server that polls GitHub as a bot account. When a
pull request comment mentions that bot, or when a configured master account has
an open pull request in an allowlisted repository, Maid checks out the PR in its
own cache, runs local `codex` from that checkout, and posts Codex's final answer
as a normal PR comment.

Maid uses polling, not webhooks. It expects `git`, `gh`, and `codex` to be on
`PATH`.
Maid marks work in progress with an `eyes` reaction and completed work with a
`+1` reaction. Mention requests use reactions on the mention comment; automatic
PR-open reviews use reactions on the PR description.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init     # create ~/.maid/config.toml
just start    # start Maid in the background
just logs     # follow ~/.maid/maid.log
```

Fill in `bot_login` and `master_accounts` in `~/.maid/config.toml` before
starting. Maid only responds to mentions authored by one of the configured
master accounts. `auto_review_accounts` controls which master accounts can get
automatic reviews in allowlisted repositories; if omitted, it defaults to
`master_accounts`. `auto_review_repos` lists the repositories to poll for open
PRs. Set either option to `[]` to disable automatic PR-open reviews.
`task_limit_per_24h` caps how many mention and automatic review tasks Maid will
start in a rolling 24-hour window. Omit it for no limit; set it to `0` to pause
new task starts without marking eligible work handled. Maid stores that rolling
task-start ledger at `~/.maid/task-starts.json` by default.

Maid asks `gh` for the bot account's token, so the bot account must be logged in
locally:

```sh
gh auth login
gh auth status --hostname github.com
```

`just start` runs Maid in the background. Runtime state lives in `~/.maid`,
including `~/.maid/maid.log`, `~/.maid/maid.pid`, and the default repo cache.
Use `just status` to check it, `just stop` to stop it, and `just update` to
pull the latest `main` and restart.

See [config.example.toml](config.example.toml) for all config options and example values.

Development:

```sh
just dev
just check
```
