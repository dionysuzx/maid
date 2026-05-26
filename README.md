# Maid

![Maid banner](assets/maid-banner.png)

Maid is a small Rust + Axum server that polls GitHub notifications as a bot
account. When a pull request comment mentions that bot, Maid checks out the PR
in its own cache, runs local `codex` from that checkout, and posts Codex's final
answer as a normal PR comment.

Maid uses polling, not webhooks. It expects `git`, `gh`, and `codex` to be on
`PATH`.

## Quick Start

```sh
git clone https://github.com/dionysuzx/maid.git
cd maid
just init     # create ~/.maid/config.toml
just start    # start Maid in the background
just logs     # follow ~/.maid/maid.log
```

Fill in `bot_login` in `~/.maid/config.toml` before starting. Maid asks `gh` for
the bot account's token. You can also provide one explicitly:

```sh
export GITHUB_TOKEN=...
just start
```

`just start` runs Maid in the background. Runtime state lives in `~/.maid`,
including `~/.maid/maid.log`, `~/.maid/maid.pid`, and the default repo cache.
Use `just status` to check it, `just stop` to stop it, and `just update` to
pull the latest `main` and restart.

Config options:

```toml
bot_login = ""              # required
bind = "127.0.0.1:3000"
poll_seconds = 20
cache_dir = "~/.maid/cache"
codex_bin = "codex"
github_api_ip = ""          # optional
```

Development:

```sh
just dev
just check
```
