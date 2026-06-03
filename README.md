![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust polling bot for GitHub pull requests. It runs as a GitHub
bot account, prepares isolated worktrees for eligible PR tasks, runs `codex`,
and posts Codex's final answer as a normal PR comment.

Maid responds when an allowed master account mentions the bot in a PR comment.
Automatic reviews can be enabled for allowlisted repos. Trusted master mentions
can also start an operator request with `/operate`, which lets Codex use local
tools such as `git` and `gh` to make changes, commit, push, or open pull
requests when the request calls for it. Maid still posts Codex's final status
back to the pull request.

Maid keeps one bare git repository per GitHub repo and creates an
isolated git worktree for each task trigger. Concurrent requests on the same PR
therefore run in separate working directories while sharing git objects.

## Prerequisites

- A GitHub bot account authenticated with `gh`; Maid uses that account's token.
- Rust/Cargo, `git`, `gh`, `codex`, `just`, and `nvim` on `PATH`.
- For `/operate`, the Maid machine's `gh` auth and git config must be able to
  commit, push, and open pull requests non-interactively.

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
- `github_api_requests_per_hour`: global GitHub REST API request budget.
  Defaults to `1200`, which is 24% of GitHub's normal 5,000 authenticated
  requests per hour account limit and paces requests at one call every 3
  seconds. Every GitHub API call Maid makes goes through this budget, including
  calls made by concurrent Codex tasks. Maid polls as soon as GitHub permits and
  stays under this configured API budget.
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
  `{{request_text}}`, and `{{operator_trigger}}`.

See [config.example.toml](config.example.toml) for every option.
