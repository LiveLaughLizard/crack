<p align="center">
  <img src="assets/logo.png" alt="crack" width="260">
</p>

<h3 align="center">Coding agents, certified cracked.</h3>

<p align="center">
  A fast, native coding agent with a GPU-rendered interface that works like a terminal agent.<br>
  Point it at a project and it reads, searches, edits, builds and tests: with any model, in a window or in CI.
</p>

---

**crack** is written in Rust on [GPUI](https://www.gpui.rs), the UI framework behind
the Zed editor. Launch it from a project directory (or pass `-C <dir>`) and it works
there, like `claude` or `opencode` do in a terminal:

- **Knows your project.** Reads `AGENTS.md` (and `CLAUDE.md`) from the project root
  down to where you are, and picks up more of them in subdirectories as it reads files
  there. It loads skills from `SKILL.md` folders.
- **Does the work.** Runs shell commands, reads, writes and edits files, searches with
  glob and grep, fetches web pages, and keeps a todo list. Build mode has full tools;
  Plan mode is read-only.
- **Any model.**
  - **Cloud:** Anthropic, OpenAI, Gemini, Amazon Bedrock, Google Vertex AI, Azure and
    Microsoft Foundry.
  - **Hosted:** OpenRouter, Groq, xAI, Mistral, Together and Fireworks.
  - **Chinese clouds:** Qwen, Kimi, Z.ai and MiniMax, plus DeepSeek.
  - **Self-hosted:** Ollama, LM Studio and any OpenAI-compatible server.

  Reasoning effort is sent the way each provider expects it.
- **MCP.** Connects to Model Context Protocol servers over stdio or HTTP and gives
  their tools to the model.
- **Tabs.** Run several sessions at once; turns keep going in the background.
- **Point and click, or keyboard only.** Connect providers from a dialog with links
  to each console. There are pickers for models, effort, sessions and skills, a command
  palette, and opencode-style keybindings.
- **Headless for CI.** `crack run "<prompt>"` does one turn without a window and writes
  text or JSON lines. Exit codes are meaningful.
- **Build types.** A full build, one without Chinese services, or an air-gapped build
  with every cloud provider compiled out.

## Build

```sh
nix-shell          # or: nix develop
cargo build --release
./target/release/crack
```

The shell provides rustup (pinned by `rust-toolchain.toml`) and the libraries GPUI
needs on Linux (Wayland, X11, Vulkan, fontconfig).

## Providers

The easiest way to connect one is in the app. Click **Connect a provider** on the home
screen, or press `ctrl-x p` (also `/connect`, the command palette, or a "Connect…" row
in the model picker). The Providers dialog lists every provider with its status.
Pick one to see:

- links to its key or console page, which open in your browser;
- the fields it needs: API key (masked), endpoint, region, project or AWS profile;
- **Test connection** and **Save** buttons;
- for Vertex AI and AWS SSO, a **Sign in** button that runs `gcloud auth login` or
  `aws sso login --profile …`, which finish in your browser.

Keys are saved to `~/.config/crack/credentials.toml` (mode 600) and other settings to
`~/.config/crack/config.toml`. **Disconnect** removes them. The dialog works with the
mouse or the keyboard: ↑↓ to choose, Enter or Tab to edit, Enter to save, `ctrl-t` to
test, Esc to go back.

Environment variables work too, which suits CI. The model is `provider/model`:
`-m anthropic/claude-opus-5-5`, `-m ollama/qwen3-coder`, `-m openrouter/x-ai/grok-4`.

| Provider | id | Credentials |
|---|---|---|
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`), optional `ANTHROPIC_BASE_URL` |
| Amazon Bedrock (Claude, Messages API) | `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`, an AWS profile (`AWS_PROFILE`; static keys or SSO), or `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`(/`AWS_SESSION_TOKEN`); `AWS_REGION` |
| Google Vertex AI (Claude) | `vertex` | `GOOGLE_CLOUD_PROJECT` (or `ANTHROPIC_VERTEX_PROJECT_ID`), `CLOUD_ML_REGION` (default `global`); token from `GOOGLE_OAUTH_ACCESS_TOKEN` or `gcloud auth print-access-token` |
| Microsoft Foundry (Claude) | `foundry` | `ANTHROPIC_FOUNDRY_API_KEY`, `ANTHROPIC_FOUNDRY_RESOURCE` (or `ANTHROPIC_FOUNDRY_BASE_URL`) |
| OpenAI | `openai` | `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` |
| Azure OpenAI | `azure` | `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_DEPLOYMENTS` (comma-separated) |
| Google Gemini | `google` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| OpenRouter, Groq, xAI, Mistral, Together, Fireworks | `openrouter`, `groq`, `xai`, `mistral`, `together`, `fireworks` | `<NAME>_API_KEY` |
| Qwen (Alibaba Cloud Model Studio) | `qwen` | `DASHSCOPE_API_KEY`; China region: `DASHSCOPE_BASE_URL=https://dashscope.aliyuncs.com/compatible-mode/v1` |
| Kimi (Moonshot AI) | `kimi` | `MOONSHOT_API_KEY`; China: `MOONSHOT_BASE_URL=https://api.moonshot.cn/v1` |
| Z.ai / Zhipu (GLM) | `zai` | `ZAI_API_KEY` (or `ZHIPUAI_API_KEY`); China: `ZAI_BASE_URL=https://open.bigmodel.cn/api/paas/v4` |
| MiniMax | `minimax` | `MINIMAX_API_KEY`; China: `MINIMAX_BASE_URL=https://api.minimax.cn/v1` |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` |
| Ollama, LM Studio | `ollama`, `lmstudio` | none; used when the server answers on its default port (`OLLAMA_HOST` to change) |
| Any OpenAI-compatible server (vLLM, llama.cpp, LiteLLM, TGI…) | `custom` | `CRACK_BASE_URL` (e.g. `http://gpu-box:8000/v1`), optional `CRACK_API_KEY` |

OpenAI-compatible servers are asked for their model list (`GET /models`). `crack
providers` shows what's configured and `crack models` lists every usable model.

Self-hosted or extra providers go in the config file:

```toml
# ~/.config/crack/config.toml   (project overrides: .crack/config.toml or crack.toml)
model = "anthropic/claude-opus-5-5"
effort = "high"            # default, minimal, low, medium, high, xhigh, max
agent = "build"            # build or plan
theme = "dark"             # dark or light
instructions = ["~/notes/house-style.md"]
skills = ["~/shared-skills"]

[provider.gpu]
kind = "openai"            # openai, anthropic, azure, bedrock, vertex
name = "GPU box (vLLM)"
base_url = "http://10.0.0.5:8000/v1"
api_key_env = ["GPU_BOX_KEY"]
models = ["Qwen/Qwen3-Coder-480B"]
reasoning = "none"         # effort (reasoning_effort), nested (reasoning.effort), none

[provider.bedrock]
region = "eu-west-1"
```

### Effort

Effort is sent the way each provider takes it:

- **Claude 4.6 and later:** adaptive thinking plus `output_config.effort`.
- **Older Claude models:** a thinking budget.
- **OpenAI-style APIs:** `reasoning_effort`.
- **OpenRouter:** `reasoning.effort`.

Choose it with `-e`, `ctrl-t`, `ctrl-x e` or `/effort`.

On the first-party Anthropic API, Claude Opus 5.5, Opus 5, Fable 5.1 and Sonnet 5.5
requests ask for the server-side refusal fallback (`fallbacks: "default"`).

## Build types

Which services the binary can reach is fixed at compile time. Left-out services are
compiled out entirely: no code, endpoints or hostnames for them are in the binary.

| Build | Command | Contains |
|---|---|---|
| Full (default) | `cargo build --release` or `just build` | All providers |
| No Chinese services | `cargo build --release --no-default-features --features cloud` or `just build-no-china` | Everything except Qwen, Kimi, Z.ai, MiniMax and DeepSeek |
| Air-gapped | `cargo build --release --no-default-features` or `just build-airgap` | Only Ollama, LM Studio and self-hosted OpenAI- or Anthropic-compatible servers you configure; no `webfetch` tool |

The features are `cloud` (hosted providers and `webfetch`) and `china` (the Chinese
model clouds; it requires `cloud`). `crack providers` and the home screen show which
build is running. An air-gapped build still uses whatever `base_url` you configure, so
point it only at servers inside your network.

The Chinese providers switch region (international or China mainland, plus Z.ai's
Coding Plan) with buttons in the Providers dialog. A key only works in the region
that issued it.

## The interface

| Key | Action |
|---|---|
| `enter` / `shift-enter`, `ctrl-j` | Send / new line |
| `tab`, `shift-tab` | Cycle agent: Build (all tools) and Plan (read-only) |
| `ctrl-p` | Command palette |
| `ctrl-x p` | Providers: connect API keys, cloud accounts and local servers |
| `ctrl-x m`, `f2` | Model picker, cycle recent models |
| `ctrl-t`, `ctrl-x e` | Cycle effort, effort picker |
| `ctrl-x n`, `ctrl-x l` | New session in this tab, sessions |
| `ctrl-x t`, `ctrl-x w` | New tab, close tab |
| `ctrl-tab`, `ctrl-shift-tab`, `ctrl-x 1`–`9` | Next / previous tab, go to tab N |
| `ctrl-x k` | Skills |
| `ctrl-x b` | Toggle sidebar |
| `ctrl-x y` | Copy last response |
| `esc esc` | Interrupt the running turn |
| `up` / `down` | Prompt history |
| `ctrl-c` | Clear the prompt; quit when it's empty |

Slash commands: `/connect`, `/new`, `/sessions`, `/models`, `/effort [level]`, `/agents`,
`/skills`, `/init` (write AGENTS.md), `/reload`, `/copy`, `/rename <title>`,
`/cd <dir>`, `/sidebar`, `/tab`, `/close`, `/mcp`, `/help`, `/exit`, and
`/<skill-name> [request]`.

Sessions are saved in a SQLite database, `~/.local/share/crack/crack.db` (or
`$CRACK_DATA_DIR`), as they happen, so a crash loses at most the last few events.
They're listed per project. In the Sessions picker (`ctrl-x l`):

- typing searches the conversations themselves, not just titles;
- `ctrl-d` pressed twice deletes a session.

`crack -s last` reopens the latest session, and a plain launch reopens the tabs that
were open when you last quit. Sessions saved as JSON by older versions are imported on
first start; the old files are kept in `sessions.imported/`.

**Tabs.** Each tab is a session. A turn keeps running when you switch to another tab:
running tabs show a spinner, and a note appears when a background tab finishes. Click a
tab to switch to it. Close it with its ×, a middle-click or `ctrl-x w`; closing a
running tab stops its turn. Opening a session that's already in a tab switches to that
tab.

## MCP servers

crack connects to [Model Context Protocol](https://modelcontextprotocol.io) servers and
offers their tools to the Build agent as `mcp__<server>__<tool>`. Plan mode doesn't get
them, because they may change things. Configure servers in `~/.config/crack/config.toml`
or a project `crack.toml`:

```toml
[mcp.github]                       # a local server over stdio
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}" }

[mcp.docs]                         # a remote server over Streamable HTTP
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_TOKEN}" }
timeout = 300                      # seconds per tool call (default 120)

[mcp.old]
command = "old-server"
enabled = false
```

Claude Code's `.mcp.json` files (`{"mcpServers": {...}}`) are read as well: from the
project root down to the working directory, and `~/.config/crack/mcp.json`. `${VAR}` and
`${VAR:-default}` are expanded in commands, arguments, URLs, environment variables and
headers.

- **Where status shows:** the home screen footer (`⊙ 3 MCP`, in green, amber or red)
  and the sidebar. `/mcp` lists each server's status and error; choosing one reconnects
  it.
- **Checking from the command line:** `crack mcp` connects every server and lists its
  tools. It exits with 1 if any server fails, so you can use it in CI.
- **Headless runs:** `crack run` connects the servers before the turn. Allow their tools
  with `--tools mcp` (all of them), `--tools mcp__github` (one server) or the exact tool
  names.
- **Remote servers:** authenticate with headers; there's no OAuth login flow.

## Headless and CI

```sh
crack run "fix the failing tests"                       # streams the answer; tools on stderr
crack run --format json "update CHANGELOG.md" > events.jsonl
git diff origin/main | crack run -a plan "review this change"
crack run --tools read,grep,glob -m openai/gpt-5 "where is auth handled?"
crack run --max-steps 40 -e high -s last "continue"
```

The exit code is 0 on success, 1 on an error and 130 when interrupted. JSON mode prints
one event per line: `text`, `thinking`, `tool_call`, `tool_result`, `usage`,
`notice`, `error`, and a final `done` with the session id, usage and the result text.
Piped stdin is added to the prompt when it arrives within a second; put `-` among the
arguments to wait for it however long it takes (`slow-cmd | crack run - "summarize"`).

```yaml
# GitHub Actions
- run: crack run --format json "Update the docs for the API changes in this PR" | tee crack.jsonl
  env:
    ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
```

Shell completion (with live `--model` completion): `source <(crack completion bash)`
(also `zsh` and `fish`).

## Instructions and skills

Instruction files go into the system prompt, in this order:

1. **Global:** `~/.config/crack/AGENTS.md`, or `~/.claude/CLAUDE.md` when it is absent.
2. **Project:** `AGENTS.md` in each directory from the project root (the nearest `.git`)
   down to the working directory. A directory without one falls back to its `CLAUDE.md`.
3. **Configured:** files listed under `instructions` in the config.

`AGENTS.md` files in deeper directories are added when the agent reads files there.

Skills are folders with a `SKILL.md` (YAML frontmatter `name` and `description`).
crack looks for them in these places:

- `.crack/skills`, `.claude/skills` and `.agents/skills`, from the working directory
  up to the project root;
- `~/.config/crack/skills`, `~/.claude/skills` and `~/.agents/skills`;
- the config's `skills` list.

The model sees each skill's name and description, and loads one with the `skill` tool.
You can run one with `/<name>`. A skill with `disable-model-invocation: true` is only
run by hand. `crack skills` lists what was found.

## Tools

`bash`, `read`, `write`, `edit`, `glob`, `grep`, `list`, `webfetch`, `todowrite`, and
`skill`. The Plan agent gets the read-only ones.

## Development

`just` lists the tasks: `just dev`, `just headless "<prompt>"`, `just check` (fmt, clippy,
tests), `just install` (to ~/.local/bin), `just mock` with `just dev-mock` to try it
without an API key.

`cargo test` runs the unit tests. `cargo run --example mock_provider` starts a scripted
model server (OpenAI and Anthropic protocols) for trying crack without an API key:

```sh
CRACK_BASE_URL=http://127.0.0.1:8765/v1 cargo run -- run -m custom/mock "run: ls"
```

## License

[MIT](LICENSE). Release binaries also carry the licenses of the libraries they include.
