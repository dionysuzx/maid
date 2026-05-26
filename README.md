# Maid

![Maid banner](assets/maid-banner.png)

Maid is a small Rust + Axum server that polls GitHub notifications as
`mayushii-nyan`. When a pull request comment mentions `@mayushii-nyan`, Maid
checks out the PR in its own cache, runs local `codex` from that checkout, and
posts Codex's final answer as a normal PR comment.

Maid uses polling, not webhooks. It expects `git`, `gh`, and `codex` to be on
`PATH`.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just run
```

By default, Maid asks `gh` for the `mayushii-nyan` token. You can also provide
one explicitly:

```sh
export GITHUB_TOKEN="$(gh auth token -u mayushii-nyan)"
just run
```

Useful options:

```sh
export MAID_BOT_LOGIN=mayushii-nyan
export MAID_POLL_SECONDS=20
export MAID_CACHE_DIR="$HOME/.cache/maid"
```

Development:

```sh
just check
```
