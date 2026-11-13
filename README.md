![Maid banner](assets/maid-banner-v2.jpg)

Maid is a GitHub bot that reviews pull requests and answers mentions with local
coding agents (Codex and Claude Code), posting every agent's reply in one comment.

## Setup

Needs Rust, `git`, `just`, `nvim`, `gh` logged in as the bot account, and the
agent CLIs (`codex`, `claude`) logged in for non-interactive use.

```sh
just init    # creates ~/.maid/config.toml
just config  # edit it; config.example.toml documents every option
just start   # then: just status, just logs, just stop
```
