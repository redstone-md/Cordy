<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Cordy&note=a%20terminal%20coding%20agent%20in%20Rust&theme=dark">
  <img alt="Cordy — a terminal coding agent in Rust" src="https://gfx.redstone.md/strip?label=Cordy&note=a%20terminal%20coding%20agent%20in%20Rust&theme=light" width="840">
</picture>

**English** · [Русский](README.ru.md)

Hot-swap any model or provider mid-conversation. Edit files behind a permission gate.
MCP, skills, sub-agents, and background jobs — in one fast, keyboard-driven TUI.

[![CI](https://github.com/redstone-md/Cordy/actions/workflows/ci.yml/badge.svg)](https://github.com/redstone-md/Cordy/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/redstone-md/Cordy?display_name=tag&sort=semver)](https://github.com/redstone-md/Cordy/releases)
[![crates.io](https://img.shields.io/crates/v/cordy?logo=rust)](https://crates.io/crates/cordy)
[![License: MIT](https://img.shields.io/badge/license-MIT-6e9bf5)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024-b7410e)](https://www.rust-lang.org)

</div>

Cordy is a cross-platform terminal agent that talks to every major API family — OpenAI Chat
Completions, OpenAI-compatible endpoints, Anthropic Messages, and the OpenAI Responses API —
through one canonical model, so switching provider or model never loses your conversation. It
edits files with an exact, diff-confirmed `edit`, runs a builtin toolset whose command output is
compressed by a native token optimizer, and speaks the Model Context Protocol.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Highlights&note=what%20sets%20it%20apart&theme=dark">
  <img alt="Highlights" src="https://gfx.redstone.md/strip?label=Highlights&note=what%20sets%20it%20apart&theme=light" width="840">
</picture>

- **Hot-swap models & providers** mid-conversation — start on a local model, finish on a frontier
  one, context intact. `^P` → pick a model, or `/connect` to add a provider.
- **Every API family** — OpenAI Chat, OpenAI-compatible (Ollama / vLLM / OpenRouter / Groq / …),
  Anthropic Messages, OpenAI Responses. Wire formats never leak past the adapter.
- **Builtin tools** — `read` `write` `edit` `multiedit` `grep` `glob` `ls` `bash` `todo` `web_search`
  `web_fetch` `process` `rewind`, all behind a permission gate you can pre-approve with globs.
- **Background jobs** — run a dev server with `bash background:true`, then `process` to poll,
  wait on a regex, or kill the whole process tree.
- **MCP, skills, sub-agents** — connect MCP servers, load progressive-disclosure skills, and
  delegate work to isolated sub-agents via the `task` tool.
- **Free web search** — DuckDuckGo out of the box (no key), or Exa when `EXA_API_KEY` is set.
- **Live cost & context HUD** — tokens, `$` cost, optimizer savings, context fill, background
  jobs and sub-agents in a side panel.
- **OpenCode-inspired UI** — command palette, prompt history, multiline input, sessions, a
  provider manager, eight themes, and full config hot-reload.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Install&note=prebuilt%20binaries%20or%20from%20source&theme=dark">
  <img alt="Install" src="https://gfx.redstone.md/strip?label=Install&note=prebuilt%20binaries%20or%20from%20source&theme=light" width="840">
</picture>

**Linux / macOS**

```sh
curl -fsSL https://raw.githubusercontent.com/redstone-md/Cordy/main/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://raw.githubusercontent.com/redstone-md/Cordy/main/install.ps1 | iex
```

The scripts download the signed binary for your platform, verify its checksum, and drop `cordy`
on your `PATH`.

**With cargo** — `cargo install cordy`

**Prebuilt binaries** — or grab the archive for your platform from the
[latest release](https://github.com/redstone-md/Cordy/releases/latest), unpack, and run `cordy`.

**From source** (Rust 1.88+) — the default build is full-featured (MCP included):

```sh
git clone https://github.com/redstone-md/Cordy.git
cd Cordy
cargo build --release
# binary at target/release/cordy
```

Want a leaner binary without the MCP client? `cargo build --release --no-default-features`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Quick%20start&note=connect%20a%20provider%20and%20go&theme=dark">
  <img alt="Quick start" src="https://gfx.redstone.md/strip?label=Quick%20start&note=connect%20a%20provider%20and%20go&theme=light" width="840">
</picture>

Point Cordy at any OpenAI-compatible endpoint via environment variables, then run it:

```sh
export OPENAI_API_KEY=sk-...
export CORDY_BASE_URL=https://api.openai.com/v1   # or any compatible endpoint
export CORDY_MODEL=gpt-4o-mini
cordy
```

…or skip the env vars and press **`^P` → /connect** inside the app to add a provider through the
wizard. Providers you connect are saved to `~/.cordy/config.toml`, and the last one is restored on
launch. All user state — config, keys, sessions, cache — lives under `~/.cordy`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Keybinds&note=OpenCode-style&theme=dark">
  <img alt="Keybinds" src="https://gfx.redstone.md/strip?label=Keybinds&note=OpenCode-style&theme=light" width="840">
</picture>

| Key | Action |
| --- | --- |
| `Enter` | send · `^J` / `Alt+Enter` newline |
| `Tab` / `Shift+Tab` | cycle agent/mode |
| `^P` | command palette — models, agents, themes, providers, commands |
| `↑` / `↓` | prompt history · move across lines |
| `Esc` | interrupt the running turn |
| `^X` then … | leader: `l` sessions · `m` models · `t` theme · `s` status · `y` copy reply |
| `^R` · `^L` · `^-`/`^.` | rename session · redraw · undo/redo input |
| wheel · `PgUp`/`PgDn` · `^G` | scroll the transcript |

Slash commands include `/connect` `/providers` `/model` `/sessions` `/permissions` `/mouse`
`/thinking` `/compact` `/goal` `/ralph` and more — type `/` to autocomplete.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Configuration&note=hot-reloaded%20%C2%B7%20secrets%20stay%20in%20env&theme=dark">
  <img alt="Configuration" src="https://gfx.redstone.md/strip?label=Configuration&note=hot-reloaded%20%C2%B7%20secrets%20stay%20in%20env&theme=light" width="840">
</picture>

`~/.cordy/config.toml` (merged with a project-level `.cordy/config.toml`). It is **hot-reloaded**
— edits apply within a second, no restart. Secrets never live here; API keys go in the OS env or
`~/.cordy/keys.json` (written by `/connect`).

```toml
optimize = true
theme = "tokyonight"   # mono (default) · dark · tokyonight · catppuccin · gruvbox · nord · rosepine · light

# Fine-tune any role color on top of the theme
[colors]
accent  = "#7aa2f7"
surface = "#24283b"

# Pre-approve commands so the agent stops asking
[permissions]
mode  = "ask"          # or "auto"
allow = ["bash:ls *", "bash:git *", "bash:cargo *"]
deny  = ["bash:rm -rf *"]

# A provider (secrets stay in env / keys.json, never here)
[[provider]]
name = "openai"
kind = "openai-chat"
base_url = "https://api.openai.com/v1"

# An MCP server (in the default build)
[[mcp]]
name = "browsermcp"
transport = "stdio"
command = "npx @browsermcp/mcp@latest"
enabled = true
```

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Project%20layout&note=one%20crate%2C%20clean%20module%20boundaries&theme=dark">
  <img alt="Project layout" src="https://gfx.redstone.md/strip?label=Project%20layout&note=one%20crate%2C%20clean%20module%20boundaries&theme=light" width="840">
</picture>

```
src/
  core/       canonical model (ContentBlock/Message/WireEvent), agent loop, prompt, context,
              sessions, permissions, autonomous (ralph) loop, auth
  provider/   Provider trait + one adapter per API family (+ retry decorator)
  tools/      Tool trait, builtins, native output optimizer, sub-agents, background jobs
  skills/     progressive-disclosure skill loader
  agents/     sub-agent registry
  mcp/        MCP client (on by default; opt out with --no-default-features)
  config/     TOML load/merge, model & provider profiles
  tui/        ratatui MVU app — model, update, view, themes, markdown
```

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=Contributing&note=fmt%20%C2%B7%20clippy%20%C2%B7%20test&theme=dark">
  <img alt="Contributing" src="https://gfx.redstone.md/strip?label=Contributing&note=fmt%20%C2%B7%20clippy%20%C2%B7%20test&theme=light" width="840">
</picture>

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Before opening a PR:

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://gfx.redstone.md/strip?label=License&note=MIT&theme=dark">
  <img alt="License" src="https://gfx.redstone.md/strip?label=License&note=MIT&theme=light" width="840">
</picture>

Released under the [MIT License](LICENSE).

<div align="center">
<sub>Part of <a href="https://github.com/redstone-md">redstone.md</a></sub>
</div>
