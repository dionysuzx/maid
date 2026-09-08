![Maid banner](assets/maid-banner-v2.jpg)

Maid is a smol bot that runs local Codex on GitHub events.

It watches for opened PRs and issue or PR mentions to your configured GitHub bot account
(a "maid"), checks that the request came from a trusted master account, prepares
an isolated worktree, runs `codex`, and posts Codex's final answer back as a
comment. It can also run automatic reviews for configured repositories or for
configured accounts across all public repositories, and trusted users can
request adhoc operator tasks with `/operate`.

Each task runs in a fresh disposable Git repository. Maid derives its fetch URL
from the validated GitHub owner and repository, passes bot authentication only
to that fetch, and never lets a worker-controlled Git directory cross back into
a privileged parent Git command.

## Getting Started

Maid expects Rust/Cargo, `git`, `gh`, `codex`, `just`, and `nvim` on `PATH`.
Authenticate `gh` as the GitHub bot account before starting Maid. Authenticate
Codex separately in Maid's dedicated auth directory (the default is
`~/.maid/codex`):

```sh
mkdir -p ~/.maid/codex
chmod 700 ~/.maid/codex
CODEX_HOME=~/.maid/codex codex login
```

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

Trusted masters must be configured with their immutable numeric GitHub user ID:

```toml
master_accounts = [{ login = "your-name", id = 123456789 }]
```

Find the ID with `gh api users/your-name --jq .id`. If upgrading from the old
string-list format, replace every login string with a `{ login, id }` record
before restarting Maid. Maid refuses missing, zero, duplicate, or conflicting
trusted identities. Authorization uses only the ID returned on each GitHub
comment or pull request, so renames remain trusted while a reclaimed login does
not inherit authority. The configured login remains a display and public-PR
discovery hint; update it after a rename without changing the ID.

Set `auto_review_public_accounts` to master-account logins whose open pull
requests Maid should discover across GitHub. Maid ignores PRs into private base
repositories. Repository-scoped review remains available through
`auto_review_repos` and `auto_review_accounts`.

## Security model

Review and automatic-review workers are read-only and cannot request
escalation. Trusted `/operate` workers can write only their disposable task
repository by default; requested escalations go through Codex's automatic
approval reviewer. Automatic review is a policy decision, not unconditional
approval.

Workers receive a clean environment, a dedicated home and Codex auth home, no
shell network access by default, an explicit command search path, and no
user/project rules or user config. At startup, Maid runs the configured Codex
sandbox against synthetic markers in its auth/runtime directories and the host
temporary directories (`/tmp` and `/private/tmp` on macOS); it refuses to start
unless both review and operator profiles deny those reads. Maid
also caps task wall time and captured output, applies Unix CPU, file-size,
descriptor, and core-dump limits (plus address-space limits where supported),
and kills the worker process group
when a task ends or is cancelled. Published comments carry Maid provenance,
neutralize GitHub mentions, and have a conservative size limit.

These controls are not a VM or container boundary. Codex itself still needs
network access to the model service, and `/operate` approvals may deliberately
grant additional access. Deploy Maid under a separate OS account or container
with outbound allowlisting and OS-level memory/process quotas when handling
hostile repositories or when stronger isolation is required.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
