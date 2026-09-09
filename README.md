![Maid banner](assets/maid-banner-v2.jpg)

Maid is a smol, review-only bot that runs Codex on GitHub events.

It watches for opened PRs and issue or PR mentions to your configured GitHub bot account
(a "maid"), checks that the request came from a trusted master account, prepares
an isolated repository, runs a disposable Codex worker, and posts Codex's final answer back as a
comment. It can also run automatic reviews for configured repositories or for
configured accounts across all public repositories.

Each task runs in a fresh disposable Git repository. Maid derives its fetch URL
from the validated GitHub owner and repository, passes bot authentication only
to that fetch, and never lets a worker-controlled Git directory cross back into
a privileged parent Git command.

## Getting Started

Maid expects a Linux controller with Rust/Cargo, Docker, `git`, `gh`, `just`,
and `nvim` on `PATH`. Native macOS workers are not supported. Authenticate `gh`
as the GitHub bot account, then clone Maid:

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
```

Build the pinned worker image,
then authenticate Codex into Maid's dedicated auth directory (the default is
`~/.maid/codex`):

```sh
mkdir -p ~/.maid/codex
chmod 700 ~/.maid/codex
just worker-login
```

Verify the worker boundary before starting Maid:

```sh
just verify-worker
```

Create your local config, edit it, then start the bot:

```sh
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

Each review runs in the pinned `maid-codex-worker:0.153.4` Linux image. Docker
mounts only the disposable repository and the dedicated Codex authentication
directory; the GitHub token remains in the controller. The repository, auth
mount, and container root are read-only. The container drops capabilities,
forbids privilege gain, and sets fixed memory, CPU, PID, and temporary-storage
limits. Docker's outer seccomp filter is disabled because Codex's nested Linux
sandbox needs user namespaces; the non-root worker still has no capabilities,
and Codex applies its own command seccomp and filesystem policy.

Codex uses one fixed review policy: repository reads only, shell network off,
and approvals disabled. Every invocation includes `--ignore-user-config`,
`--ignore-rules`, and `--ephemeral`. This keeps user configuration, project
exec rules, and resumable session state out of the worker contract. Codex still
needs container network access to reach the model service; its shell sandbox
denies model-generated commands that access the network or Codex auth files.
Run `just verify-worker` after rebuilding the image to exercise those denials
with synthetic fixtures.

Maid also bounds task time and captured output, kills the worker process group,
and asks Docker to remove a timed-out worker. Published review comments carry
Maid provenance, neutralize GitHub mentions, and have a conservative size
limit.

## Migration from `/operate`

`/operate` has been removed. Maid responds with an explicit unsupported-command
notice and does not start a worker. Remove `codex_bin`, `codex_worker_path`, and
`codex_prompts.operator_mention` from existing configuration, then build and
verify the worker image. Implementation, commits, pushes, and pull requests
belong in a separate trusted workflow with its own credential and publication
contract; Maid intentionally does not expose controller credentials to workers.

## Metrics

Maid serves Prometheus metrics at `http://127.0.0.1:9464/metrics` by default.
`maid_last_successful_poll_timestamp_seconds` records the Unix timestamp of the
last poll that completed successfully. The sample is absent until the first
successful poll. Configure `metrics_bind_address` to use a different address.
