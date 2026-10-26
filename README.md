![Maid banner](assets/maid-banner-v2.jpg)

Maid is a smol bot that runs local Codex on GitHub events.

It watches for opened PRs and issue or PR mentions to your configured GitHub bot account
(a "maid), checks that the request came from a trusted master account, prepares
an isolated worktree, runs `codex`, and posts Codex's final answer back as a
comment. It can also run automatic reviews for configured repositories, for
configured accounts across all public repositories, or when a configured team
is requested on an open pull request, and trusted users can request adhoc
operator tasks with `/operate`.

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

Set `auto_review_public_accounts` to trusted GitHub logins whose open pull
requests Maid should discover across GitHub. Maid ignores PRs into private base
repositories. Repository-scoped review remains available through
`auto_review_repos` and `auto_review_accounts`.

For `auto_review_teams`, GitHub moves `updated_at` when a draft is marked
ready or a reviewer is requested, and also when someone later comments or
pushes on a pull request that still requests the team. The pull request list
has no request timestamp, and the timeline is a separate oldest-first history.
GitHub rewrites notification reasons, and the bot only receives
`review_requested` when it is on the team.

A normal shutdown waits until a posted review is written to the ledger, including
while that write is still failing. Killing the process after the GitHub comment
and before the write can review the same head once more.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
