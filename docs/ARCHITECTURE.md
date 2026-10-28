# Architecture

## Process model

Melchior runs in one of two modes:

- **Headless:** `melchior "task"` runs one turn in an in-memory session and
  prints the answer on stdout.
- **Server:** `melchior --serve` (or `--profile sdk`, as DeepSeek Harness is
  launched) serves one agent over stdio. A host typically starts one process
  per agent, keeps it while the agent is in use, and stops it when the agent
  changes or goes idle.

```text
melchior --serve --patch agent.patch.yml --patch /workspace/.tools.patch.yml
   stdin  ◀── JSON-RPC requests, one per line
   stdout ──▶ responses and notifications, one per line (nothing else is ever written to stdout)
   stderr ──▶ logs: "2026-10-26T03:55:57.203Z info [server] initialized: ..."
```

Child processes: curl for HTTPS, `bash` and its jobs, stdio MCP servers, and
a local `leviathan mcp` when Leviathan runs locally. All are tracked and
killed on shutdown.

Threads:

| Thread | Does |
| --- | --- |
| main | Reads stdin, dispatches requests, answers them in order. Never blocks on the model. |
| `turn-<sessionId>` | One per running prompt: runs the agent loop, then exits. |
| short-lived helpers | Read a child process's output (shell tools, curl stderr). |

Writes to stdout go through one `Notifier` with a mutex, so a response and an
event never interleave within a line. The process exits when it receives
`shutdown` or stdin reaches EOF; on the way out it disposes every plugin
scope (killing background jobs) and kills every tracked child process
(`proc::kill_all`), so a harness never leaks a curl or bash process.

## Boot

`initialize` is the only point where configuration is read:

1. **Entries.** At startup, `cli::main` builds the plugin entry list from the
   distribution's profile (`default` unless `--profile` names another),
   applies each `--patch` file in order (see [CORDIS.md](CORDIS.md)), then
   `--index` / `--memory`. A patch error is printed to stderr and returned as
   the `initialize` error.
2. **Runtime.** `initialize` validates `cwd`, reads `MELCHIOR_HOME` (or
   `DSH_HOME`), and builds the `Runtime`. That holds the workspace, home,
   provider, model, reasoning effort, max tokens, and permission mode. The
   permission mode is `FullAccess` only if `MELCHIOR_PERMISSION_MODE` (or
   `DSH_PERMISSION_MODE`) is `danger-full-access`.
3. **Core services.** The server provides `runtime`, `sessions`, `prompt`,
   `tools`, `turn-hooks`, `turn-end-hooks`, `llm`, `mcp`, and
   `leviathan-backends` on the root scope.
4. **Plugins.** `loader::start` activates every enabled entry, in profile
   order, once the services it injects exist. `leviathan` connects to its
   backend early, so its tools are in the report. `agent-loop` comes last
   and combines everything into the `agent` service.
5. **Report.** The response carries `serverInfo` plus a load report: active,
   disabled, unknown, pending, and failed entries, and the final tool list.
   `harness/plugins` returns the same report later.

## Module map

```text
   main.rs ── cli.rs ── distribution.rs (name, plugins, profiles)
                 │
            server.rs ─────────────────── rpc.rs (Notifier)
                 │
   ┌─────────────┼───────────────┐
cordis/      session.rs      plugins.rs ── built-in profiles and plugins
(App, Scope,     │               │
 Registry,       │      ┌────────┼─────────┬──────────┬───────────────────────────┐
 loader)         │   agent.rs  prompt.rs  tools/*   mcp.rs ◀── uses ── leviathan/ (mod: backends + service,
                 │      │                    │     (HTTP, stdio)             index: briefing + recall,
                 └──────┤                    │                               memory, tools)
                        ▼                    ▼
                     llm.rs ── http.rs ── curl (https) / TcpStream (http)
     shared: json.rs  yaml.rs  jsexpr.rs  util.rs  runtime.rs  proc.rs
```

## The life of one turn

The host calls `session/prompt {sessionId, contentBlocks}`.

1. **Admission** (main thread). The text blocks are joined. If the session is
   open, it is used. If it exists on disk but is not open, the call fails with
   `session "<id>" already exists`; the host then calls `session/resume` and
   retries. If it doesn't exist, it is created. A session already running a
   prompt is rejected as busy. The response `{messageId}` is sent, and a
   worker thread starts.
2. **User message.** The worker emits `session.status running` and persists
   `message/user`, fsyncing it before anything else happens. It then emits
   `agent/inbox/spliced` with `inserted[0].id == messageId`, which is the
   receipt DeepSeek Harness clients wait for.
3. **Turn hooks.** Hooks run for top-level turns only, not for subagents.
   - `leviathan/refresh` reconnects Leviathan if needed (and starts the
     implicit local index on first use).
   - `leviathan/recall` searches the index with the user's text.
   - `leviathan-memory/recall` attaches the agent's memory: the briefing on
     a session's first message, and memories matching the user's text.
   - `mcp/<server>/reconnect` lists a gateway's tools if that failed at boot.
   - A hook that returns context gets it persisted as `message/context`,
     linked to the message id, and emitted as `context/added`.
4. **Model rounds.** Each round:
   1. The system prompt is rebuilt from the persona plus every section, in
      profile order: the environment, the Leviathan briefing, the memory
      guide, the active goal, the task list, skills, and the tool-name map.
      Tools added by the previous round (indexing data adds search) are
      offered from this round on.
   2. The message history is rebuilt from the session log, compacted to the
      context budget, and the request is streamed. Deltas go out as
      `assistant/delta` and `assistant/reasoning`.
   3. The assistant message is persisted (with its tool calls and reasoning
      details) and emitted as `assistant/message`.
   4. Each tool call runs in order. The harness emits `tool/call`, executes the
      call (catching panics), and persists and emits `tool/result`.
5. **Ending.** The loop ends with reason `stop`, `max_rounds`, `cancelled`, or
   `error`. If the model stops on tool output without writing an answer, it
   is asked once more, with tools disabled, to answer. At the round cap, it is
   forced to answer the same way. `turn/end {reason:{kind}, usage}` is
   persisted and emitted.
6. **Turn-end hooks** (top-level turns that didn't fail).
   `leviathan-memory/journal` hands one `event` memory of the question and
   answer to a background thread. Then `session.status idle`.

A client takes the last `assistant/message` as the reply and stops reading at
`idle`.

### What the model sees

```text
system:    <persona>

           # Environment
           Today is Sunday, October 25, 2026. ...

           # Workspace index (Leviathan)
           It holds 1,204 records across 9 sources, dated 2026-01-02 to 2026-10-24. ...

           # Memory (Leviathan)
           You keep a private memory that lasts across conversations ... `memory_remember` ...
user:      [ "<the user's message>", "<workspace_recall ...> 1. ... </workspace_recall>",
             "<agent_memory ...> Standing memories: ... Matching this message: ... </agent_memory>" ]
assistant: tool_calls: leviathan_search {"query": "...", "group": "..."}
tool:      <Leviathan's result cards>
assistant: <answer>
```

## Failure behaviour

| Failure | Behaviour |
| --- | --- |
| Model HTTP 408/409/425/429/5xx, connect/TLS/timeout | Retried, up to `maxAttempts` (3 by default), with backoff and `retry-after`; `assistant/retry` is emitted |
| Model error after retries | A transient `assistant/message` explaining the error, then `turn/end` with kind `error` |
| Tool error | Returned to the model as `Error: ...`; the turn continues |
| Tool panic | Caught; reported to the model as a harness bug |
| Process killed mid-turn | Committed records survive. Resume synthesises "interrupted" results for unfinished tool calls, so the next request is valid |
| Torn final log line | Truncated on resume; nothing else is ever rewritten |
| Any other corrupt line | Resume refuses with "corrupt"; the file is not touched |
| Gateway unreachable at boot | Retried before each turn, at most every 15 s |
| Leviathan empty or not granted | No briefing or recall; nothing else changes |
| Memory not enabled, not granted, or down | No memory context and no journal; nothing else changes |
