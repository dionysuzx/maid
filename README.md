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

Set `auto_review_teams` to `org/team-slug` entries, for example
`Some-Org/eng-infra`, to review an open pull request in `auto_review_repos`
when that team is still in the pull request's requested reviewers. Any author
is eligible except Maid itself. The review is the same automatic review used
for trusted authors, including the same prompt, one comment, and the same
handled reaction and local marker, so a pull request is reviewed at most once.
Empty or omitted leaves team review off.

Maid does not walk an existing backlog. It reads the newest 50 open pull
requests in each watched repository and starts a team review only when the
pull request was created within `github_notification_window_hours` (24 by
default). A team request that GitHub has already removed by that read is not
reviewed. Author auto-review does not use this window. `review_requested`
notifications are not the trigger: GitHub rewrites notification reasons, and
the bot only receives them when it is on the team. The pull request's
`requested_teams` is the signal, read from that same open-pull-request list.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
