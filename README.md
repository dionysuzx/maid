# Maid

![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust + Axum bot runner for GitHub work. It polls GitHub as a bot
account, checks out eligible repositories into a local cache, runs `codex` from
that checkout, and publishes Codex's final answer back to GitHub.

Maid handles three workflows:

- Mention requests: an allowed master account mentions the bot in a PR comment.
- Automatic reviews: an allowed master account opens a PR in an allowlisted repo.
- Issue implementation: an allowed master account opens a labeled issue in an
  allowlisted repo.

It uses polling, not webhooks. It expects `git`, `gh`, and `codex` on `PATH`.
In-progress work gets an `eyes` reaction; completed work gets a `+1` reaction.
Mention requests use reactions on the mention comment; automatic PR-open reviews
use reactions on the PR description; automatic issue implementation uses
reactions on the issue.

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
- `auto_implement_accounts`: master accounts eligible for automatic issue
  implementation; defaults to `master_accounts`.
- `auto_implement_repos`: repositories to poll for labeled issue implementation;
  empty or omitted disables issue implementation.
- `auto_implement_label`: issue label that triggers implementation; defaults to
  `maid`.
- `auto_implement_window_days`: recently updated issue polling window; defaults
  to 30 days.
- `task_limit_per_24h`: optional rolling 24-hour cap across mention, automatic
  review, and issue implementation tasks. Omit it for no limit; set it to `0` to
  pause new task starts.

`auto_implement_accounts` keeps labeled issues from untrusted authors out of the
write-capable Codex path. For automatic issue implementation, the configured
publishing identity needs repository write access so Maid can push deterministic
branches like `maid/issue-123` and open pull requests. Maid performs the commit,
push, and PR creation itself; Codex only edits the checkout and returns the PR
body.

If issue implementation PRs should be opened by another GitHub account, configure
`implementation_actor`. Maid still polls, comments, and marks work as the bot,
but it uses the implementation actor's `gh` token to open PRs:

```toml
[implementation_actor]
login = "dionysuzx"
git_auth = "host"
commit_identity = "host"
```

With `git_auth = "host"`, Maid uses the repository SSH remote for implementation
branches and does not inject the bot token into git commands. With
`commit_identity = "host"`, Maid lets the host git config choose author,
committer, signing key, and signing behavior. The host must be able to commit
and push non-interactively:

```sh
gh auth login --user maid-bot
gh auth login --user dionysuzx
gh auth status --hostname github.com
git config --global user.name
git config --global user.email
git config --global commit.gpgsign
ssh -T git@github.com
```

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
