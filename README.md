![Maid banner](assets/maid-banner-v2.jpg)

Maid is a smol bot that runs local Codex on GitHub events.

It watches for opened PRs and issue or PR mentions to your configured GitHub bot account
(a "maid), checks that the request came from a trusted master account, prepares
an isolated worktree, runs `codex`, and posts Codex's final answer back as a
comment. It can also run automatic reviews for configured repositories, and
trusted users can request adhoc operator tasks with `/operate`.

Each task runs in its own git worktree while sharing a cached bare repository
for the source GitHub repo.

## Getting Started

Maid expects Rust/Cargo, `git`, `gh`, `codex`, `just`, and `nvim` on `PATH`.
Authenticate `gh` as the GitHub bot account before starting Maid.

Clone the repo, create your local config, edit it, then start the bot:

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init
just config
just start
```

Useful commands:

```sh
just status
just logs
just stop
just restart
just update
```

## Configuration

Runtime config lives at `~/.maid/config.toml`. Run `just config` to edit it, and
use [config.example.toml](config.example.toml) as the configuration reference.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
