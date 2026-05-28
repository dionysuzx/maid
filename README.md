# Maid

![Maid banner](assets/maid-banner.png)

Maid is a small Rust + Axum server that polls GitHub as a bot account. When a
pull request comment mentions that bot, when a configured master account has an
open pull request in an allowlisted repository, or when an allowlisted
repository has a labeled issue ready for implementation, Maid checks out the
repository in its own cache, runs local `codex`, and publishes the result back
to GitHub.

Maid uses polling, not webhooks. It expects `git`, `gh`, and `codex` to be on
`PATH`.
Maid marks work in progress with an `eyes` reaction and completed work with a
`+1` reaction. Mention requests use reactions on the mention comment; automatic
PR-open reviews use reactions on the PR description; automatic issue
implementation uses reactions on the issue.

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
`auto_implement_repos` lists repositories where Maid should poll open issues
with `auto_implement_label` and create implementation PRs. It is disabled when
omitted or empty. `auto_implement_window_days` bounds issue polling to recently
updated issues and defaults to 30 days.
`task_limit_per_24h` caps how many mention, automatic review, and issue
implementation tasks Maid will start in a rolling 24-hour window. Omit it for no
limit; set it to `0` to pause new task starts without marking eligible work
handled. Maid stores that rolling task-start ledger at `~/.maid/task-starts.json`
by default.

Maid asks `gh` for the bot account's token, so the bot account must be logged in
locally:

```sh
gh auth login
gh auth status --hostname github.com
```

For automatic issue implementation, the bot account also needs repository write
access so Maid can push deterministic branches like `maid/issue-123` and open
pull requests. Maid performs the commit, push, and PR creation itself; Codex only
edits the checkout and returns the PR body.

`just start` runs Maid in the background. Runtime state lives in `~/.maid`,
including `~/.maid/maid.log`, `~/.maid/maid.pid`, and the default repo cache.
Use `just status` to check it, `just stop` to stop it, `just config` to edit
the config in Vim, `just restart` to stop and start it again, and `just update`
to pull the latest `main`, edit the config, and start it again.

See [config.example.toml](config.example.toml) for all config options and example values.

Development:

```sh
just dev
just check
```
