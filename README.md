![Maid banner](assets/maid-banner-v2.jpg)

Maid is a small Rust polling bot for GitHub pull requests. It runs as a GitHub
bot account, checks out eligible PRs into a local cache, runs `codex`, and posts
Codex's final answer as a normal PR comment.

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
`bot_login`, `master_accounts`, `codex_model`, and
`codex_reasoning_effort`. The `[codex_prompts]` section is also required.

- `auto_review_accounts`: master accounts eligible for automatic PR reviews;
  defaults to `master_accounts`.
- `auto_review_repos`: repositories to poll for automatic PR reviews; empty or
  omitted means mention-only mode.
- `task_limit_per_24h`: optional rolling 24-hour cap. Omit it for no limit; set
  it to `0` to pause new task starts.
- `codex_model` and `codex_reasoning_effort`: required Codex invocation
  settings. Posted comment footers report these values.
- `[codex_prompts]`: required prompt templates for the internal Codex run.
  These are the full prompts Maid sends after placeholder interpolation.
  Mention templates support `{{mention_url}}`, `{{pr_url}}`, `{{raw_body}}`,
  and `{{cleaned_text}}`. Automatic review templates support `{{pr_url}}`
  and `{{author}}`.

Posted comments include a Codex metadata footer with the configured model,
reasoning effort, and the input prompt collapsed behind a details block.

See [config.example.toml](config.example.toml) for every option.
