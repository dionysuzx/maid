![Maid banner](assets/maid-banner-v2.jpg)

Maid is a smol bot that runs local coding agents (Codex and Claude Code) on GitHub events.

It watches for opened PRs and issue or PR mentions to your configured GitHub bot account
(a "maid), checks that the request came from a trusted master account, prepares
an isolated worktree, runs each configured agent in parallel, and posts one
comment with every agent's answer and the exact prompt they received. It can also run automatic reviews for configured repositories, for
configured accounts across all public repositories, or when a configured team
or account is requested as a reviewer on an open pull request, and trusted
users can request adhoc operator tasks with `/operate`.

Each task runs in its own git worktree while sharing a cached bare repository
for the source GitHub repo.

## Getting Started

Maid expects Rust/Cargo, `git`, `gh`, `just`, `nvim`, and each configured agent
CLI (`codex`, `claude`) on `PATH`, authenticated for non-interactive use.
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

Set `auto_review_requested_reviewers` to teams (`org/team-slug`) and accounts
(`login`) whose review request on a pull request in `auto_review_repos` asks
Maid to review it. List the bot's own login to review whenever someone requests
it. GitHub moves `updated_at` when a draft is marked ready or a reviewer is
requested, and also when someone later comments or pushes on a pull request
that still requests the reviewer. The pull request list has no request timestamp, and the timeline is a separate oldest-first history.
GitHub rewrites notification reasons, so Maid reads review requests from the
pull request, not from `review_requested` notifications.

Set `continuous_review = true` to review every new head commit of a pull
request in scope, not just the first one. Repository discovery then reads the
most recently updated page, because a push moves a pull request to the top.
A pull request Maid has reviewed stays in scope, even after GitHub clears the
review request that brought it in once the reviewer reviews. Maid records the last head it reviewed for each pull request in
`~/.maid/reviewed-heads.json`, written after the review comment is posted.
Each entry keeps the head's tree too, so a force-push that only rewrites
history, leaving the code unchanged, is not reviewed again. Each review
starts with the commit it covers, because the author may push again before
it posts.
When an automatic review starts, Maid adds 👀 to the pull request so its author
can see Maid is working. The reaction is best effort and is not a record.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
