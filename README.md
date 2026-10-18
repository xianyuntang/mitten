# mitten

A personal agent that runs on your machine. Chat with it in the terminal or over Discord. It
reads files and web pages, searches the web, remembers what you tell it, calls tools on MCP
servers such as Linear, and hands coding work to Claude Code.

Mitten's own tools only read. Anything that changes something (an MCP call, a Claude Code task,
a settings change) asks you first, or goes past a reviewer model in auto mode.

## Install

macOS (Apple silicon or Intel) and Linux (x86_64 or arm64, glibc 2.35 or newer):

```sh
curl -fsSL https://raw.githubusercontent.com/xianyuntang/mitten/main/install.sh | sh
```

The script downloads the release for your platform, checks its SHA-256, and puts `mitten` in
`~/.local/bin`. Options:

| Variable | Default | Meaning |
| --- | --- | --- |
| `MITTEN_VERSION` | latest | Release tag to install, e.g. `v0.1.0` |
| `MITTEN_INSTALL_DIR` | `~/.local/bin` | Where the binary goes |

To upgrade later, run `mitten update`. It reinstalls the latest release over the current binary
and restarts the background service if one is installed.

Or build from source with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/xianyuntang/mitten mitten
```

## Quick start

```sh
mitten configure   # full-screen setup; ←/→ switch pages, Save on any page
mitten             # chat in the terminal
```

`mitten configure` asks for your API key and model, tests the connection, and writes
`~/.config/mitten/config.toml`. Only the **Model** page is required.

In the terminal chat, `/new` starts a fresh conversation, `/cast <request>` sends out a cast of crabs (see `cast_crabs` under Tools), and `/exit` (or Ctrl-C) quits.
Conversations are saved, so the next `mitten` picks up where you left off.

## Discord

Mitten can answer you in Discord, by DM or in any server channel it can read.

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications),
   add a bot, and copy its token.
2. Under **Bot**, turn on **Message Content Intent**. Without it the bot cannot connect.
3. Invite the bot to your server with the permissions **View Channels**, **Send Messages**,
   **Send Messages in Threads**, **Create Public Threads**, **Read Message History**,
   **Add Reactions**, **Attach Files**, and **Embed Links**.
4. Find your Discord user ID: turn on **Developer Mode** in Discord's settings, then right-click
   your name and choose **Copy User ID**.
5. Run `mitten configure`, open the **Discord** page, and enter the token and your user ID.
6. Run `mitten install` to keep the bot running in the background.

How it behaves:

- Only the user IDs you list are heard; everyone else is ignored.
- Each server can have its own list instead, set under **Add server** on the Discord page of
  `mitten configure`: the server ID, its user IDs, and whether it replies to every message or
  only when @mentioned. In config, that is `[discord.guilds.<server ID>]` with `allowed_users`
  and `mention_only = true`. Other servers and DMs use the main list. A thread the bot is already
  answering in needs no further mention until mitten restarts. To copy a server ID, right-click
  the server with Developer Mode on.
- A message in a server channel opens a thread, and each thread is its own conversation.
  Idle threads archive after an hour.
- Each person has their own memory, shared by all of their threads and DMs; the terminal has one
  of its own. Messages from different people are never merged into one turn.
- 👀 means received, ✅ done, ❌ failed. `/new` starts over in that channel or thread, and `/cast <request>` sends out a cast of crabs.
- Messages sent in quick succession, or while mitten is still answering, are merged and answered
  once.
- Images, text files (including the `message.txt` Discord makes from a long paste), and PDF,
  Word (`.docx`), and PowerPoint (`.pptx`) documents you attach go to the model; documents as
  their text, up to 50,000 characters. A PDF's first 3 pages also go as images, and the model can
  look at any other page with `view_pdf`, so it sees the tables, figures, and scanned pages its
  text misses.
- Approvals show up as **Run** and **Deny** buttons.

## Running in the background

```sh
mitten install     # launchd on macOS, a systemd user service on Linux
mitten restart     # after editing the config by hand
mitten uninstall
```

The service runs `mitten serve` and restarts it if it exits. `mitten install` records your
shell's `PATH`, so `npx`, `uvx`, and `claude` resolve the way they do in your terminal, whether
they come from Homebrew, asdf, nvm, or elsewhere. `mitten update` restarts it for you; run
`mitten install` again after installing new tools.

Logs:

- macOS: `~/Library/Logs/mitten.log`
- Linux: `journalctl --user -u mitten -f`
- Terminal chat: `~/.local/share/mitten/mitten.log`

## Tools

| Tool | What it does | Needs approval |
| --- | --- | --- |
| `read_file`, `list_dir` | Reads files, including the text of PDF, `.docx`, and `.pptx` documents up to 20 MB, and lists directories. Hidden paths (like `~/.ssh` or `.env`) and Mitten's own config and database are refused. | No |
| `fetch_url` | Reads a web page as text, optionally rendered in headless Chrome, or a linked PDF, `.docx`, or `.pptx` document up to 20 MB; a PDF's first 3 pages also come as images. Private network addresses are refused. | No |
| `view_pdf` | Shows the model pages of a local or linked PDF as images, up to 10 per call, for layout, tables, figures, and scans that the text misses. Same path and address rules as `read_file` and `fetch_url`. | No |
| `send_file` | Discord only. Sends a local file or a downloaded one (under 3 MB) as an attachment, e.g. an image the user asked to see. Same path and address rules as `read_file` and `fetch_url`. | No |
| `web_search` | Searches through your SearXNG instance or the Brave Search API (pick one in `mitten configure`), optionally only recent results (`time_range`), news sites (`category`), or a date span (`after`, `before`; on SearXNG only Google engines honor it). Results show their publish date when known. | No |
| `cast_crabs` | Cast of crabs mode (螃蟹群; a group of crabs is a cast, and mitten is a mitten crab), modeled on Kimi's Agent Swarm and used when the model judges a task big enough, or whenever a message starts with `/cast <request>`. The model acts as an orchestrator and dispatches up to 100 named crabs per call, each with its own role and task, 8 running at a time, while a live board shows what each one is doing. Crabs use only the read-only tools above and report back; the model sends more waves as needed and writes the answer. Needs web search configured. | No |
| `memory` | Saves short notes about the person talking, loaded into their later conversations. | No |
| `skill` | Loads, saves, or removes skills: saved instructions for recurring tasks, kept in the database and shared by everyone. Their names and descriptions are listed in every conversation; the model loads one when a task matches, and offers to save a finished multi-step task as a new one. | Save and remove |
| `cron` | Schedules a named prompt to run later, once (`at`) or on a cron expression, in the config's `timezone` (default: this machine's; restart `mitten serve` after changing it). Each run starts a fresh conversation and posts its reply in the Discord channel or thread where the job was made. Discord only; jobs run while `mitten serve` is up, and a run missed while it was down happens once on startup. Approval prompts during a run are denied unless approval is set to auto. A job's instructions are shown when it is made or changed; ask to change its instructions or timing, remove it, or give it a trial run now (posted where the job was made, schedule unchanged). | No |
| `settings` | Reads or changes the model and a few limits. | Yes |
| MCP tools | Any tool from the MCP servers you connect. | Yes, per server |
| `claude_code` | Hands a coding task to Claude Code. | Yes |

### MCP servers

Add servers on the **MCP** page of `mitten configure`, or in the config. Their tools reach the
model as `<server>__<tool>`.

```toml
# A remote server. Linear accepts a personal API key as the bearer token.
[mcp.servers.linear]
url = "https://mcp.linear.app/mcp"
token = "lin_api_..."

# A local server started with npx.
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_..." }
approve = true   # the default: ask before each call
```

Servers connect once at startup and are shared by every conversation. Restart after changing
them.

### Claude Code

With [Claude Code](https://claude.com/claude-code) installed and logged in, Mitten can delegate
work that changes code or runs commands. It runs `claude -p` in a directory you allow, shows each
step as it goes, and reports the result. A later task can resume the same Claude Code session.

```toml
[tools.claude_code]
dirs = ["~/repos"]                   # it may only work in these directories
permission_mode = "acceptEdits"      # or "plan" or "default"; bypassPermissions is refused
allowed_tools = ["Bash(cargo test:*)", "Bash(git diff:*)"]
```

## Approval

Every action that needs approval shows you what will run. There are two modes:

- **Ask** (default): you approve each action.
- **Auto**: a reviewer model reads your request and the proposed action. Reading, and small
  changes you asked for, go ahead with a 🛡️ note. Anything that deletes, publishes, messages
  other people, changes permissions, touches many items, or that the reviewer is unsure about is
  put to you with its reason. A failed review also falls back to asking.

```toml
[approval]
mode = "auto"
model = "glm-5.3-flash"   # optional; defaults to the main model
```

The model cannot turn on auto mode itself; only `mitten configure` or the config file can.

## Configuration

Everything lives in `~/.config/mitten/config.toml` (or pass `--config PATH`).
[`config.example.toml`](config.example.toml) documents every option. `mitten configure` keeps
settings it doesn't show, such as MCP servers you added by hand. Most changes apply from the next
message; MCP servers, web search, and Discord need a restart.

## Development

```sh
cargo test                    # unit tests
cargo test -- --ignored       # also the tests that call real services (npx, your model, claude)
cargo clippy --all-targets -- -D warnings
```

Pushing a `v*` tag that matches the version in `crates/mitten/Cargo.toml` builds every platform
and publishes a GitHub release.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
