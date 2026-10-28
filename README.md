# Melchior

Melchior is an agent harness for agentic work over datasets far larger than a
context window: ticket histories, logs, CRM and ERP exports, maintenance
records, whole database tables. It is one small Rust binary with no
dependencies, built around two things:

- **[Leviathan](https://github.com/elstongun/leviathan) at the centre of every
  turn.** Leviathan indexes records of any size and answers a question in a
  few hundred tokens with cited result cards, whether it holds a thousand
  records or a million. Melchior briefs the model on what is indexed, searches
  the index with every message before the model sees it, gives the model
  first-class search tools, lets it index new data the user points at, and
  keeps its memory across conversations in Leviathan too.
- **The Cordis plugin model.** Every capability (a model provider, a tool,
  Leviathan, the system prompt, an MCP server) is a plugin that profiles list
  and patch files configure, disable, or add, without touching code. Products
  build their own distribution by adding plugins to the same core.

And underneath:

- **No third-party dependencies.** Pure Rust and `std`; `Cargo.lock` lists
  only this crate. HTTPS goes through the system `curl`. The release binary
  is about 1.3 MB.
- **Durable sessions.** An append-only JSONL log per session, each record
  fsynced before the harness acts on it, with an ownership lock. A crash never
  replays model work or loses a committed turn.
- **A drop-in for DeepSeek Harness hosts.** The same command line,
  newline-delimited JSON-RPC protocol over stdio, and patch files.

```text
                           ┌──────────────────────── melchior ─────────────────────────┐
  you / a host ── stdio ──▶│ server ─ sessions (JSONL, flock)                           │
  (CLI, JSON-RPC)          │   │                                                        │
                           │ agent loop ── prompt ─────── Leviathan briefing            │
                           │   │            turn hooks ── recall · memory              │
                           │   │            turn end ──── memory journal               │
                           │ tools: leviathan_search · leviathan_index · memory_* ·     │
                           │        files · bash · web · skills · subagent · MCP        │
                           │ llm ── curl (TLS) ──▶ OpenRouter / DeepSeek / any OpenAI-compatible API
                           └───────────┬───────────────────────────────┬────────────────┘
                                       │ MCP (stdio or HTTP)           │ MCP (HTTP or stdio)
                                       ▼                               ▼
                     Leviathan: index + memory               other MCP servers / a host's
                     (local `leviathan mcp`, remote          permission-checked tool gateway
                      `leviathan serve`, or via a gateway)
```

## Install

You need:

| | Why | How |
| --- | --- | --- |
| Rust 1.89 or newer | To build Melchior | [rustup.rs](https://rustup.rs) |
| `curl` | Melchior makes HTTPS requests through it | Already on macOS and most Linux systems |
| [Leviathan](https://github.com/elstongun/leviathan) | Indexing, search and memory. Melchior runs without it, but this is what it is built around | `cargo install leviathan-index` |
| Bubblewrap (Linux) | The sandbox for the `bash` tool | `sudo apt install bubblewrap` or `sudo dnf install bubblewrap` |
| A model API key | OpenRouter by default; DeepSeek or any OpenAI-compatible API also work | [openrouter.ai/keys](https://openrouter.ai/keys) |

Then install both binaries and set your key:

```sh
cargo install --git https://github.com/magilabsco/Melchior   # installs `melchior` into ~/.cargo/bin
cargo install leviathan-index                                # installs `leviathan`
export OPENROUTER_API_KEY=sk-or-...
melchior --version && leviathan --version                    # check both are on your PATH
```

To build from a clone instead: `git clone https://github.com/magilabsco/Melchior && cd Melchior && cargo build --release`.
The binary is `target/release/melchior`.

**On macOS** there is no Bubblewrap, so the `bash` tool refuses to run in the
default confined mode. To give the agent an unconfined shell on your own
machine, set `MELCHIOR_PERMISSION_MODE=danger-full-access`. That also lifts the
workspace-only limit on the file tools, so only do it for folders and data you
trust the agent with.

## Use

### Ask questions about a dataset

Run Melchior in a folder that holds your data (CSV, TSV, JSON Lines or JSON)
and ask in plain language:

```sh
cd ~/exports
melchior "Which customers hit the SSO login loop after release 4.2?"
```

Before the first model call, Melchior indexes every data file of 1 MB or
more in the folder with Leviathan. This step makes no model calls and costs
no tokens: about 2 seconds for 100,000 records (64 MB), about 20 seconds for
a million (650 MB). The agent then answers
from the index: it searches, counts, sums and ranks without reading the
files into its context. The index is kept per folder under
`~/.melchior/leviathan/`, so later runs in the same folder start from it.
Smaller files, or files you name, the agent indexes itself when it needs to.

### Index once, reuse it, and keep memory

For data you query often, build the index yourself so you choose the record
id, the group (customer, machine, account) and the date field, then give
Melchior a memory file to keep what it learns across runs:

```sh
leviathan index tickets.csv --id id --group customer --date created --index tickets.db
melchior --index tickets.db --memory memory.db "Which customers hit the SSO login loop after 4.2?"
melchior --index tickets.db --memory memory.db "What did we find about the SSO login loop?"   # answered from memory
```

### Choose a model

Headless runs use OpenRouter with `deepseek/deepseek-chat-v3.1` unless you
say otherwise:

```sh
MELCHIOR_MODEL=deepseek/deepseek-v4.1-flash-1017 melchior "..."                # another OpenRouter model
MELCHIOR_PROVIDER=deepseek-official MELCHIOR_MODEL=deepseek-chat \
  DEEPSEEK_API_KEY=sk-... melchior "..."                                       # DeepSeek's own API
```

Any other OpenAI-compatible endpoint (a local server, another gateway) is a
few lines in a patch file passed with `--patch`; see the `llm-pi-ai` section of
[docs/PLUGINS.md](docs/PLUGINS.md).

### Run it inside an application

`melchior --serve` speaks newline-delimited JSON-RPC on stdin and stdout, the
same protocol as DeepSeek Harness, so a host runs one process per agent and
sends it messages ([docs/PROTOCOL.md](docs/PROTOCOL.md)):

```sh
MELCHIOR_HOME=/tmp/agent melchior --serve
{"jsonrpc":"2.0","id":"1","method":"initialize","params":{"cwd":"/tmp","provider":"openrouter","model":"deepseek/deepseek-chat-v3.1"}}
{"jsonrpc":"2.0","id":"2","method":"session/prompt","params":{"sessionId":"s1","contentBlocks":[{"type":"text","text":"hello"}]}}
```

To ship your own agent with your own tools and defaults on top of Melchior,
build a distribution ([docs/EMBEDDING.md](docs/EMBEDDING.md)).

### Develop

```sh
cargo test                # unit tests and end-to-end runs over stdio (with a real Leviathan when one is installed)
cargo clippy --all-targets
```

## Leviathan in every turn

| | What the model gets |
| --- | --- |
| **Automatic indexing** | At session start, the workspace's data files of 1 MB or more are indexed before the first model call (`autoIndex`), so the model starts with an index instead of raw files. |
| **Briefing** | What is indexed: record and group counts, largest groups, date range, filter fields, and how to search. When nothing is indexed yet, how to index. |
| **Recall** | The user's message is searched before the first model call; the top cards arrive with it as `<workspace_recall>`, persisted with the turn. |
| **Tools** | `leviathan_search`, `leviathan_get`, `leviathan_resolve_group`, `leviathan_describe`, `leviathan_aggregate` for exact counts, sums and rankings (when the installed Leviathan provides it), and `leviathan_index` for new data. |
| **Memory** | Standing memories on a session's first message, matching memories on every message, one journal note per answered question, and `memory_remember` / `memory_recall` / `memory_forget`. |

Melchior reaches Leviathan through a backend: a local `leviathan mcp` it
starts itself, a remote `leviathan serve --http`, any MCP server that exposes
Leviathan's tools, or a backend a product registers (its own permission-checked
gateway, for example). Details in [docs/LEVIATHAN.md](docs/LEVIATHAN.md).

## Configuration

| Variable | Used for |
| --- | --- |
| `MELCHIOR_HOME` (or `DSH_HOME`) | Harness home. Required for `--serve`; headless runs default to `~/.melchior`. Sessions live in `<home>/melchior/sessions`. |
| `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL` | Model access (patches can name other variables) |
| `MELCHIOR_MODEL`, `MELCHIOR_PROVIDER` | Headless model and provider (default `openrouter`, `deepseek/deepseek-chat-v3.1`) |
| `LEVIATHAN_INDEX`, `LEVIATHAN_MEMORY` | A local index and memory (same as `--index`, `--memory`) |
| `LEVIATHAN_URL`, `LEVIATHAN_TOKEN` | A remote `leviathan serve --http` |
| `LEVIATHAN_BIN` | The `leviathan` executable (default: `leviathan` on `PATH`) |
| `MELCHIOR_PERMISSION_MODE=danger-full-access` | Unconfined file and shell tools (or DeepSeek Harness's `DSH_PERMISSION_MODE`) |
| `MELCHIOR_LOG` | `error`, `warn`, `info` (default), or `debug`; logs go to stderr |
| `MELCHIOR_CURL` | Path to curl (default: `/usr/bin/curl`, then `curl` on `PATH`) |

Everything else is plugin configuration in patch files
([docs/CORDIS.md](docs/CORDIS.md), [docs/PLUGINS.md](docs/PLUGINS.md)). Headless
runs also read `<home>/settings.yaml`.

## Compared with DeepSeek Harness and Hermes Agent

DeepSeek Harness is the runtime Melchior began as a replacement for; the
comparison is with `@deepseek-ai/dsh` 0.2.1-alpha.5. Hermes Agent (Nous
Research, 0.20.5 at commit `057dcdf`) is the most complete open
general-purpose agent we evaluated. Melchior's focus is narrower than either:
an agent that works from large datasets and remembers what it learned, not a
personal assistant or a coding agent.

### Platform

| | Melchior | DeepSeek Harness | Hermes Agent |
| --- | --- | --- | --- |
| Language | Rust | TypeScript on Node 22 | Python 3.11–3.13 (plus a Node TUI) |
| Third-party dependencies | None | 681 npm packages in the lock | 32 direct, 43 optional extras |
| Interface | Headless CLI; stdio JSON-RPC (DeepSeek Harness protocol) | stdio JSON-RPC, web UI, CLI | Terminal UI, messaging gateway, ACP, API server, MCP server |
| Plugin model | Cordis, ported; products build their own distribution | Cordis | Python toolsets and plugins |
| Configuration | DeepSeek Harness profiles and patches; a safe `!!js` subset | Profiles and patches; arbitrary `!!js` | `config.yaml`, `.env`, profiles, `SOUL.md` |
| Model APIs | OpenAI-compatible (OpenRouter, DeepSeek, local servers) | Several provider APIs | Many providers (OpenAI, Anthropic, Bedrock, Vertex, Gemini, Nous Portal, local) |

### Data, memory, and context

| | Melchior | DeepSeek Harness | Hermes Agent |
| --- | --- | --- | --- |
| Large datasets | Leviathan index: briefing, per-message recall, search tools, indexing new data mid-turn | Files and shell only | Files and shell only |
| Long-term memory | Leviathan memory: typed claims, supersession, secret refusal, namespaces | No | `MEMORY.md` and `USER.md`, optional Honcho, Mem0, Supermemory |
| Writes to memory | Agent tools plus one journal note per answered turn | — | Agent tool, with periodic nudges |
| Session storage | JSONL per session, fsync per record, ownership lock | JSONL, SQLite query index | SQLite (WAL) with full-text search |
| Searching past conversations | Through memory journal notes | No | `session_search` with LLM summaries |
| Context limits | Elides old tool output, then drops old exchanges | Compaction plugins | LLM compression into linked sessions |
| MCP | Client, streamable HTTP and stdio | Client | Client (stdio and HTTP, OAuth) and server |

### Agent capabilities

| | Melchior | DeepSeek Harness | Hermes Agent |
| --- | --- | --- | --- |
| Files and search | `read`, `write`, `edit`, `glob`, `grep` | Same | Read, write, patch, search |
| Shell | `bash` with background jobs | Same, plus PowerShell | Terminal and process tools on 7 backends (local, Docker, SSH, Modal, Daytona, …) |
| Web | `web_fetch` | `web_fetch`, `web_search` | Search, extract, full browser automation |
| Images, audio, video | No (text only) | `read_image` | Vision, image and video generation, TTS, voice |
| Subagents | `subagent` (bounded delegation) | Plus fork, list, message, interrupt | `delegate_task`, parallel and asynchronous |
| Planning | Goals, todo list, plan mode | Same, plus workflows and `ralph` loops | Todo list, kanban |
| Skills | `skill` (from `<home>/skills` and the workspace) | Same | Creates and improves its own skills |

### Safety

| | Melchior | DeepSeek Harness | Hermes Agent |
| --- | --- | --- | --- |
| Supply chain | No dependencies; external programs `curl` and (optionally) `leviathan` | Large npm tree | Large Python tree |
| File and shell confinement | Workspace-only paths; `bash` in its own Bubblewrap with no network | File and shell sandbox plugins | Path checks; isolation depends on the backend |
| Code in configuration | `!!js` limited to expressions without side effects | Arbitrary JavaScript | Python plugins |
| Secrets | Kept out of tool subprocesses, session logs, and logs; refused by memory | Credential store | Credential pools and stores |

### What Melchior does not have, and how to add it

| Missing | To add it |
| --- | --- |
| Image, audio, and video input | Allow content parts in `llm.rs` messages, then add a `read_image` tool |
| `web_search` | A tool in `tools/web.rs` that calls a search provider over `http.rs` |
| Non-OpenAI model APIs | Implement `llm::Provider` and register it from a plugin |
| MCP OAuth | An auth step beside the HTTP transport in `mcp.rs` |
| Agent swarms (fork, list, message, interrupt) | Tools over the session registry in `server.rs` |
| Workflows, loops, scheduling | A plugin that drives `agent::run_turn` |
| Asking the user mid-turn | A protocol method plus a blocking tool |
| Self-written skills | Skills are instructions; Melchior leaves changing them to a reviewed process |
| Reading DeepSeek Harness sessions | An importer in `session.rs` ([details](docs/DIFFERENCES.md#sessions)) |

## Extending

The harness is a set of Cordis plugins. A profile lists them, a patch can
disable or configure them, and each plugin registers its tools, prompt
sections, and hooks into services that it injects. Everything a plugin adds
is removed again when it is disposed. To ship your own binary, build a
[distribution](docs/EMBEDDING.md).

| To add | Implement | Register in | Example |
| --- | --- | --- | --- |
| A tool | `tools::Tool` (`spec`, `call`) | A plugin's `apply`, via `tools::register` | `ToolWeb` in `plugins.rs`, `tools/web.rs` |
| A plugin | `cordis::Plugin` (`name`, `inject`, `apply`) | `Distribution::registry`, and a profile to load it by default | `LeviathanMemoryPlugin` in `leviathan/memory.rs` |
| A way to reach Leviathan | `leviathan::Backend` | `leviathan-backends`, selected with `backend: <name>` | `McpBackend` in `leviathan/mod.rs` |
| A system-prompt section | A `prompt::Section` closure over `PromptCx` | `prompt.sections.add` | `leviathan/briefing` |
| Context for each message | A `TurnHook` | `turn-hooks` | `leviathan/recall` |
| Work after a turn | A `TurnEndHook` | `turn-end-hooks` | `leviathan-memory/journal` |
| A model provider | `llm::Provider` | The `llm` registry | `LlmPiAi` in `plugins.rs` |
| A JSON-RPC method | A branch in `server.rs` dispatch | — | `harness/plugins` |

Keep to the rules the rest of the code follows: no new crates, so the build
stays offline and auditable; config keys are camelCase and documented in
[docs/PLUGINS.md](docs/PLUGINS.md); every new behaviour gets a unit test,
and anything the model or a host can see gets a case in
`tests/end_to_end.rs`.

## Documentation

| Document | What it covers |
| --- | --- |
| [docs/LEVIATHAN.md](docs/LEVIATHAN.md) | Backends, tools, indexing, the briefing and recall, and memory; why they work the way they do |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Process model, threads, module map, the life of one turn |
| [docs/CORDIS.md](docs/CORDIS.md) | The plugin model: scopes, services, `inject`, effects, registries, the loader, patches, profiles |
| [docs/PLUGINS.md](docs/PLUGINS.md) | Each built-in plugin, its config keys, and its tools |
| [docs/EMBEDDING.md](docs/EMBEDDING.md) | Building a distribution, writing plugins and Leviathan backends, the host contract |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | Every JSON-RPC method, notification, event, and error message |
| [docs/SESSIONS.md](docs/SESSIONS.md) | Session log format, durability, ownership locks, resume, corruption handling |
| [docs/SECURITY.md](docs/SECURITY.md) | Zero dependencies, TLS through curl, the `!!js` subset, confinement, secrets |
| [docs/DIFFERENCES.md](docs/DIFFERENCES.md) | Exact differences from DeepSeek Harness, tool by tool |

## Source layout

```text
src/
  main.rs          the melchior binary: Distribution::melchior() through cli::main
  cli.rs           command line: headless task, --serve, --profile, --patch, --index, --memory
  distribution.rs  name, plugin registry, profiles, session directory
  server.rs        JSON-RPC dispatch, initialize and boot, prompt workers
  cordis/          Cordis port: mod.rs (App, Context, Scope, Registry), loader.rs (profiles, patches)
  plugins.rs       built-in plugins and profiles
  leviathan/       mod.rs: backends and the leviathan service and plugin; index.rs: briefing and recall;
                   memory.rs: agent memory; tools.rs: leviathan_* and memory_* tools, leviathan_index
  agent.rs         the agent loop, history rebuild, context compaction
  prompt.rs        system-prompt sections, turn hooks, turn-end hooks
  mcp.rs           MCP client over streamable HTTP and stdio, tool naming
  llm.rs           OpenAI-compatible streaming client
  session.rs       durable JSONL sessions with ownership locks
  tools/           fs, shell and jobs, web, goal, todo, plan, skill, subagent
  http.rs          HTTP/1.1 client (plain TCP; HTTPS through curl)
  json.rs yaml.rs jsexpr.rs util.rs rpc.rs runtime.rs proc.rs
tests/
  end_to_end.rs    the binary over stdio: through a gateway, with a real Leviathan, and headless
  fixtures/        a data-assistant patch
```

## License

MIT. See [LICENSE](LICENSE).
