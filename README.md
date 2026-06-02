# Maid

![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust polling bot for GitHub work. It runs as a GitHub bot
account, checks out eligible repositories into a local cache, runs `codex`, and
publishes Codex's final answer back to GitHub.

Maid handles three workflows:

- Mention requests: an allowed master account mentions the bot in a PR comment.
- Automatic reviews: an allowed master account opens a PR in an allowlisted repo.
- Issue implementation: an allowed master account opens a labeled issue in an
  allowlisted repo.

It uses polling, not webhooks. In-progress work gets an `eyes` reaction;
completed work gets a `+1` reaction. Mention requests use reactions on the
mention comment; automatic PR-open reviews use reactions on the PR description;
automatic issue implementation uses reactions on the issue.

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
`bot_login`, `master_accounts`, `codex_model`, and `codex_reasoning_effort`.
The `[codex_prompts]` section is also required.

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
- `codex_model` and `codex_reasoning_effort`: required Codex invocation
  settings. Posted comment footers report these values.
- `[codex_prompts]`: required prompt templates for the internal Codex run.
  Mention templates support `{{mention_url}}`, `{{pr_url}}`, `{{raw_body}}`,
  and `{{cleaned_text}}`. Automatic review templates support `{{pr_url}}` and
  `{{author}}`. Issue implementation templates are required when
  `auto_implement_repos` is configured and support `{{issue_url}}`, `{{branch}}`,
  `{{title}}`, and `{{body}}`.

Posted comments include a Codex metadata footer with the configured model,
reasoning effort, and the input prompt collapsed behind a details block.

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
login = "your-name"
git_auth = "host"
commit_identity = "host"

[implementation_actor.expected_git_identity]
name = "Your Name"
email = "your-name@users.noreply.github.com"
gpgsign = true
gpg_format = "ssh"
```

With `git_auth = "host"`, Maid uses the repository SSH remote for implementation
branches and does not inject the bot token into git commands. With
`commit_identity = "host"`, Maid lets the host git config choose author,
committer, signing key, and signing behavior. `expected_git_identity` is
optional; when present, Maid checks only the configured fields from the prepared
checkout before running Codex. The host must be able to commit and push
non-interactively:

```sh
gh auth login --user maid-bot
gh auth login --user your-name
gh auth status --hostname github.com
git config --global user.name
git config --global user.email
git config --global commit.gpgsign
git config --global gpg.format
ssh -T git@github.com
```

See [config.example.toml](config.example.toml) for every option.
