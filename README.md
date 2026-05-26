# Maid

![Maid banner](assets/maid-banner.png)

Maid is a small Rust + Axum server that polls GitHub notifications as
`mayushii-nyan`. When a pull request comment mentions `@mayushii-nyan`, Maid
checks out the PR in its own cache, runs local `codex` from that checkout, and
posts Codex's final answer as a normal PR comment.

Maid intentionally uses polling instead of webhooks. It has no database, queue,
UI, or deployment machinery.

Maid expects `git`, `gh`, and `codex` to be available on `PATH`.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just run
```

If `GITHUB_TOKEN` is not set, Maid shows the active GitHub CLI account and asks
whether it should use that account's token. For the bot flow, make sure the
active `gh` account is `mayushii-nyan`, or set `GITHUB_TOKEN` explicitly:

```sh
export GITHUB_TOKEN="$(gh auth token -u mayushii-nyan)"
just run
```

## Behavior

- Poll unread GitHub notifications for `mention` notifications.
- Continue only when the notification subject is a pull request and the latest
  comment body actually mentions `@mayushii-nyan`.
- Ignore comments authored by `mayushii-nyan`.
- Handle PR timeline comments and review comments, but always reply with a
  normal PR comment rather than an inline review-thread reply.
- Reuse Maid-owned local checkouts under `MAID_CACHE_DIR`.
- Run `codex exec` in the prepared PR checkout.
- Pass Codex the mention URL, PR URL, raw mention body, and cleaned request text.
- Post Codex output directly as the GitHub comment body.
- Suppress or mark notifications only after the comment is posted successfully.

## Configuration

Authentication:

```sh
export GITHUB_TOKEN=...
```

If `GITHUB_TOKEN` is not set, Maid shows the active `gh` account and asks
whether it should use that account's token.

Optional:

```sh
export MAID_BOT_LOGIN=mayushii-nyan
export MAID_BIND=127.0.0.1:3000
export MAID_CACHE_DIR="$HOME/.cache/maid"
export MAID_POLL_SECONDS=20
export MAID_CODEX_BIN=codex
export MAID_GITHUB_API_IP=140.82.112.5 # optional local DNS troubleshooting override
```

`MAID_POLL_SECONDS` is clamped to a minimum of 10 seconds to keep polling lively
without hammering GitHub.

Maid runs Codex with approval disabled and full sandbox access:

```sh
codex --ask-for-approval never exec --sandbox danger-full-access ...
```

## Run

```sh
just run
```

Health check:

```sh
curl http://127.0.0.1:3000/healthz
```

## Development

```sh
just fmt
just clippy
just test
just check
```
