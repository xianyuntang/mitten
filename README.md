# mitten

A personal agent you chat with in the terminal or over Discord. It runs on your machine with
models from [OpenCode Go](https://opencode.ai), reads files and the web, remembers what you tell
it, calls tools on MCP servers, and hands coding work to Claude Code. Anything that changes
something asks you first, or goes past a reviewer model in auto mode.

## Install

macOS (Apple silicon or Intel) and Linux (x86_64 or arm64, glibc 2.35+):

```sh
curl -fsSL https://raw.githubusercontent.com/xianyuntang/mitten/main/install.sh | sh
```

This puts `mitten` in `~/.local/bin`. Set `MITTEN_VERSION=v0.1.0` to pin a release or
`MITTEN_INSTALL_DIR` to install elsewhere. From source instead:

```sh
cargo install --git https://github.com/xianyuntang/mitten mitten
```

## Set up

```sh
mitten configure   # model and API key, Discord bot, web search, MCP servers, Claude Code
mitten             # chat in the terminal
mitten install     # run the Discord bot in the background (launchd or systemd)
```

`mitten install` records your shell's `PATH`, so MCP servers started with `npx` or `uvx` and
`claude` are found the way your terminal finds them. Run it again after upgrading mitten or
installing new tools. The config lives at `~/.config/mitten/config.toml`; see
[`config.example.toml`](config.example.toml) for every option.

## What it can do

- **Read-only tools**: files and directories (hidden paths refused), web pages, web search
  through SearXNG, the current time.
- **Memory**: notes that carry over into later conversations.
- **Discord**: one thread per conversation, image and text attachments, button approvals.
- **MCP**: tools from local (stdio) or remote (HTTP) servers, e.g. Linear.
- **Claude Code**: delegates coding tasks to `claude -p` in directories you allow.
- **Approval**: ask every time, or `auto`, where a reviewer model passes low-risk actions and
  asks you about the rest.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
