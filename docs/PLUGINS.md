# Built-in plugins

Entry ids match DeepSeek Harness wherever a DeepSeek Harness plugin with the
same role exists, so existing patch files keep working. "Injects" lists the
services a plugin waits for (see [CORDIS.md](CORDIS.md)).

## Core services

These are provided by the server on the root scope, before any plugin starts.

| Service | Type | Purpose |
| --- | --- | --- |
| `runtime` | `Runtime` | Workspace, home, provider, model, permission mode, notifier |
| `sessions` | `SessionStore` | Durable sessions ([SESSIONS.md](SESSIONS.md)) |
| `prompt` | `PromptService` | Persona plus ordered sections, rebuilt on every request |
| `tools` | `Registry<Box<dyn Tool>>` | Tools offered to the model, sorted by name |
| `turn-hooks` | `Registry<TurnHook>` | Run once at the start of each top-level turn and may attach context |
| `turn-end-hooks` | `Registry<TurnEndHook>` | Run once after each top-level turn's `turn/end` is durable, with the question and the final answer. Failed turns are not reported. |
| `llm` | `Registry<Box<dyn Provider>>` | Model providers, keyed by provider id |
| `mcp` | `McpHub` | Connected MCP servers and their raw tool lists |
| `leviathan-backends` | `Registry<Box<dyn leviathan::Backend>>` | Ways to reach Leviathan, keyed by name ([LEVIATHAN.md](LEVIATHAN.md#connecting-to-leviathan)) |

Plugins provide more services: `agent` (`agent-loop`), `jobs` (`tool-jobs`),
`leviathan` (`leviathan`), and `leviathan-memory` (`leviathan-memory`).

## Prompt and model

### `system-prompt`

Injects `prompt` and `runtime`.

| Key | Default | Meaning |
| --- | --- | --- |
| `personaPrefix` (or `persona`) | `""` | The agent's system prompt, e.g. `!!js process.env.DSH_SYSTEM_PROMPT` as DeepSeek Harness hosts pass it |
| `environment` | `true` | Adds `# Environment`: today's date, the workspace path (only when file or shell tools exist), and a note on parallel tool calls |

### `llm-pi-ai`

Injects `llm` and `runtime`. It registers one OpenAI-compatible provider per
key of `providers`.

```yaml
- id: llm-pi-ai
  config:
    providers:
      openrouter:
        apiKeyEnv: DEEPSEEK_API_KEY          # env var holding the key; it is hidden from tools
        baseURL: !!js process.env.DEEPSEEK_BASE_URL
        api: openai-completions              # the only supported api
        models: !!js '[{ id: process.env.AGENT_MODEL }]'
        headers: { X-Extra: value }          # optional
        maxAttempts: 3                       # 1-6
        idleTimeoutSeconds: 180              # longest gap between streamed bytes
        timeoutSeconds: 600                  # whole request
```

For `openrouter`, `baseURL` defaults to `https://openrouter.ai/api/v1` and
`apiKeyEnv` to `OPENROUTER_API_KEY`. Requests to openrouter.ai also send
OpenRouter's app attribution (`HTTP-Referer`, `X-Title: Melchior`) unless
`headers` sets them.

Requests use `stream: true` with `stream_options.include_usage`. Tool calls,
reasoning text, and `reasoning_details` are reassembled from deltas. A
provider that answers without streaming is parsed as one complete response.

### `llm-deepseek`

Injects `llm`. It registers `deepseek-official`: `https://api.deepseek.com`
(or `DEEPSEEK_BASE_URL`) with `DEEPSEEK_API_KEY`.

### `agent-loop`

Injects `runtime`, `llm`, `tools`, `prompt`, `turn-hooks`, and
`turn-end-hooks`, and provides `agent`. It selects the provider named in `initialize` and fails if none is
registered.

| Key | Default | Meaning |
| --- | --- | --- |
| `maxRounds` | 40 | Model rounds per turn before the model is made to answer. Tools added during a turn (by `leviathan_index`, for example) are offered from the next round. |
| `contextChars` | 600000 | Request budget for compaction |
| `toolOutputChars` | 40000 | Tool output kept for the model, per call |
| `streamDeltas` | true | Emit `assistant/delta` and `assistant/reasoning` |

## Tools

| Entry | Tools | Notes |
| --- | --- | --- |
| `tool-goal` | `get_goal`, `create_goal`, `update_goal` | A durable session goal with optimistic revisions. An active goal is shown in the prompt. |
| `tool-todo` | `todo_write` | Replaces the task list (`pending`, `in_progress`, `completed`). An open list is shown in the prompt. |
| `plan-mode` | `exit_plan_mode` | Records a plan in session state |
| `tool-fs` | `read`, `write` | `read`: numbered lines, `offset`/`limit`, binary files refused, 4 MB cap. `write` creates parent directories. |
| `tool-str-replace-editor` | `edit` | `old_string` must match exactly once unless `replace_all` is set |
| `tool-fs-search` | `glob`, `grep` | `glob` supports `** * ? [] {}` and skips `.git`, `node_modules`, `target`, and similar directories. `grep` uses ripgrep, falling back to `grep -rn`. |
| `tool-jobs` | `job_list`, `job_output`, `job_kill` | Provides `jobs`. `job_output` takes `wait_seconds` (max 600) and returns as soon as the job exits. Background jobs are killed when the scope is disposed. |
| `tool-bash` | `bash` | `command`, `timeout_ms` (default 120 s, max 600 s), and `run_in_background` if `tool-jobs` is active. With `tool-jobs`, a command still running at its timeout is not killed: it continues as a background job and the result names it. Without `tool-jobs` it is killed. |
| `tool-web` | `web_fetch` | GET with up to 5 redirects. HTML is reduced to text; `max_chars` defaults to 40,000. |
| `tool-skill` | `skill` | Loads `SKILL.md` from `<home>/skills/*`, `<workspace>/.skills/*`, or `dirs` in the config. The prompt lists the available skills. |
| `tool-subagent` | `subagent` | Runs a task in a fresh, in-memory session with the same tools, minus `subagent`. Emits `subagent.started` and `subagent.finished`. |

### Workspace confinement

Without full access:
- File tools resolve every path, canonicalising the deepest existing ancestor
  so symlinks are followed. They refuse anything outside the workspace with
  `sandbox: file access denied: <path> is outside the workspace`.
- `bash` runs under Bubblewrap with a read-only `/`, a writable workspace, a
  private `/tmp`, the harness home hidden behind a tmpfs, and no network. If
  Bubblewrap is missing, `bash` refuses rather than running unconfined.

With `MELCHIOR_PERMISSION_MODE=danger-full-access` (or DeepSeek Harness's
`DSH_PERMISSION_MODE`), none of this applies. Give it only to trusted,
supervised agents.

A data assistant usually needs none of the workspace tools: a patch that
disables every workspace, shell, web, skill, and subagent tool leaves the
goal, todo, and plan tools, Leviathan, and the host's gateway tools
(`tests/fixtures/data-assistant.patch.yml` is an example).

## Integrations

### `mcp-client` (alias `@deepseek-ai/dsh-mcp-client`)

Injects `tools`, `mcp`, `prompt`, `turn-hooks`, and `runtime`. Add one entry
per server, usually from a patch.

| Key | Meaning |
| --- | --- |
| `serverName` | Required. `[A-Za-z0-9_-]{1,32}`; becomes the tool prefix. |
| `transport` | `streamable-http` (or `http`), or `stdio`. Defaults to `stdio` when `command` is set. |
| `url` | HTTP: the MCP endpoint |
| `headers` | HTTP: sent on every request, e.g. `Authorization: !!js '`Bearer ${process.env.GATEWAY_TOKEN}`'` |
| `command`, `args` | stdio: the server program and its arguments |
| `env` | stdio: extra environment. The server otherwise gets Melchior's environment without its credentials. |
| `cwd` | stdio: working directory, relative to the workspace (default: the workspace) |
| `toolCallTimeoutMs` | Default 180000 |

Protocol details:
- On start, the client sends `initialize` (protocol `2025-03-26`), then
  `notifications/initialized`, then pages through `tools/list`.
- Each call is a `tools/call`. HTTP responses may be plain JSON or an SSE
  stream; stdio messages are newline-delimited JSON, and a server's `ping`
  requests are answered.
- HTTP: the `Mcp-Session-Id` and `MCP-Protocol-Version` headers are
  honoured, and a 404 on a call (the server restarted) triggers one reconnect
  and a retry. stdio: a server that exits or stops answering within the
  timeout is stopped, and the next call restarts it.
- If the server is unreachable at start, the entry stays active and retries
  before each turn, at most every 15 s. stdio servers are stopped when the
  harness shuts down.

Tool naming:
- Tools are exposed as `mcp__<serverName>__<tool>`, with characters outside
  `[A-Za-z0-9_-]` replaced by `_`. For example, `leviathan.search` on server
  `tools` becomes `mcp__tools__leviathan_search`.
- A 12-hex-digit hash is appended only if two names would collide or the name
  would exceed 64 characters.
- Each description starts with the raw name (`[leviathan.search] ...`), and a
  `# Tool names` prompt section maps dotted names to function names. Agent
  prompts written as "call files.query" therefore work as written.

### `leviathan`

Injects `runtime`, `mcp`, `prompt`, `turn-hooks`, `tools`, and
`leviathan-backends`, and provides `leviathan`. See
[LEVIATHAN.md](LEVIATHAN.md).

Connection (each falls back to the environment variable shown):

| Key | Default | Meaning |
| --- | --- | --- |
| `backend` | `auto` | `auto`, `local`, `remote`, `mcp`, or a backend another plugin registered |
| `index` | `LEVIATHAN_INDEX` | Local index file, relative to the workspace. Selects the `local` backend. |
| `memory` | `LEVIATHAN_MEMORY` | Local memory file; `false` turns memory off for the local backend |
| `url` | `LEVIATHAN_URL` | `leviathan serve --http` base URL (or its `/mcp` endpoint). Selects `remote`. |
| `token` | `$tokenEnv` | Bearer token for `remote` |
| `tokenEnv` | `LEVIATHAN_TOKEN` | Variable holding the token; hidden from tools |
| `bin` | `LEVIATHAN_BIN`, else `leviathan` | The Leviathan executable for `local` |
| `autoStart` | true | With nothing configured and `bin` on `PATH`, keep a per-workspace index and memory in `<home>/leviathan/` |
| `server`, `prefix` | any, `leviathan.` / `leviathan_` | For `mcp`: which server, and the prefix its Leviathan tools carry |
| `namespace` | none | Memory namespace forced on every memory call |
| `timeoutSeconds` | 60 | Per call to a local or remote Leviathan |

Behaviour:

| Key | Default | Meaning |
| --- | --- | --- |
| `tools` | true | Register Leviathan's tools as `leviathan_*` / `memory_*` (local and remote backends) |
| `indexTool` | true | Offer `leviathan_index` (local backends) |
| `autoIndex` | true | Before the first model call, index the workspace's large data files (CSV/TSV, JSON Lines, JSON) with an inferred mapping: the implicit index, or a configured index file that does not exist yet |
| `autoIndexMinBytes` | 1048576 | Smallest file `autoIndex` considers |
| `indexTimeoutSeconds` | 3600 | Longest an indexing run may take |
| `briefing` | true | Index summary in the system prompt |
| `recall` | true | Search with each user message and attach the top records |
| `recallLimit` | 5 | Records to attach (1–20) |
| `minRelevance` | 0.15 | Drop weaker matches (Leviathan's relevance is 0–1, relative to the best match) |
| `minQueryChars` | 8 | Skip recall for very short messages ("hi", "thanks") |
| `briefingTtlSeconds` | 120 | How long a `describe` result is reused |

### `leviathan-memory`

Injects `leviathan`, `prompt`, `turn-hooks`, and `turn-end-hooks`, and
provides `leviathan-memory`. See [LEVIATHAN.md](LEVIATHAN.md#memory). It does
nothing unless the selected backend has Leviathan's memory tools.

| Key | Default | Meaning |
| --- | --- | --- |
| `briefing` | true | Attach the memory briefing to the first message of each session |
| `recall` | true | Recall memories matching each user message |
| `journal` | true | After each answered turn, save one `event` memory of the question and the opening of the answer |
| `guide` | true | `# Memory (Leviathan)` prompt section: when to remember and forget |
| `briefingBudget` | 500 | Token budget for the briefing (50–4000) |
| `recallBudget` | 400 | Token budget for each recall (50–4000) |
| `minQueryChars` | 8 | Skip recall and journaling for very short messages |
| `journalImportance` | 2 | Importance of journal events (1–5); durable memories default to 3 |
| `journalExpires` | `90d` | Expiry for journal events; `""` keeps them until pruned |
| `journalSkip` | `[]` | Session-id prefixes that are never journaled (evaluation runs, for example) |
