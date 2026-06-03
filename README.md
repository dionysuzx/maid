![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust polling bot for GitHub pull requests. It runs as a GitHub
bot account, prepares isolated worktrees for eligible PR tasks, runs `codex`,
and posts Codex's final answer as a normal PR comment.

Maid responds when an allowed master account mentions the bot in a PR comment.
Automatic reviews can be enabled for allowlisted repos. Labeled issues from
trusted accounts can be implemented automatically. Trusted master mentions can
also start an operator request with `/operate`, which lets Codex use local tools
such as `git` and `gh` to make changes, commit, push, or open pull requests when
the request calls for it. Maid still posts Codex's final status back to the pull
request.

Maid keeps one bare git repository per GitHub repo and creates an
isolated git worktree for each task trigger. Concurrent requests on the same PR
therefore run in separate working directories while sharing git objects.

## Prerequisites

- A GitHub bot account authenticated with `gh`; Maid uses that account's token.
- Rust/Cargo, `git`, `gh`, `codex`, `just`, and `nvim` on `PATH`.
- For `/operate` and host-backed issue implementation, the Maid machine's `gh`
  auth and git config must be able to commit, push, and open pull requests
  non-interactively.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init
just config
just start
```

For `/operate`, verify the publishing setup before starting Maid:

```sh
gh auth status --hostname github.com
git config --global user.name
git config --global user.email
```

## Configuration

Runtime config lives at `~/.maid/config.toml`. The required fields are
`bot_login`, `master_accounts`, `codex_model`, and
`codex_reasoning_effort`. The `[codex_prompts]` section is also required.

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
- `git_dir`: local directory for bare git repositories and task worktrees.
  Defaults to `~/.maid/git`.
- `task_limit_per_24h`: optional rolling 24-hour cap. Omit it for no limit; set
  it to `0` to pause new task starts.
- `max_concurrent_requests`: maximum Codex tasks Maid may run at once. Defaults
  to `1`, which still lets polling continue while a task is running.
- `codex_model` and `codex_reasoning_effort`: required Codex invocation
  settings.
- `[codex_prompts]`: required prompt templates for the internal Codex run.
  These are the full prompts Maid sends after placeholder interpolation.
  Mention templates support `{{mention_url}}`, `{{pr_url}}`, `{{raw_body}}`,
  and `{{cleaned_text}}`. Automatic review templates support `{{pr_url}}`
  and `{{author}}`. Operator templates support `{{bot_login}}`,
  `{{trigger_author}}`, `{{mention_url}}`, `{{pr_url}}`, `{{raw_body}}`,
  `{{request_text}}`, and `{{operator_trigger}}`. Issue implementation
  templates are required when `auto_implement_repos` is configured and support
  `{{issue_url}}`, `{{branch}}`, `{{title}}`, and `{{body}}`.

`auto_implement_accounts` keeps labeled issues from untrusted authors out of the
write-capable Codex path. For automatic issue implementation, the configured
publishing identity needs repository write access so Maid can push deterministic
branches like `maid/issue-123` and open pull requests. Maid performs the commit,
push, and PR creation itself; Codex only edits the worktree and returns the PR
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
worktree before running Codex. The host must be able to commit and push
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
