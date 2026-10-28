# SDK protocol

JSON-RPC 2.0 over stdio, with one compact JSON object per line in each
direction. This is the contract of DeepSeek Harness's `--profile sdk`, so
DeepSeek Harness SDK clients drive Melchior unchanged. Request ids may be
strings or numbers and are echoed back unchanged.

## Launch

```text
melchior --serve [--profile NAME] [--patch FILE]... [--index FILE] [--memory FILE]
melchior --profile sdk [--patch FILE]...        (as DeepSeek Harness is launched)
```

The harness reads only the environment it is given; hosts usually pass an
explicit allowlist. See [EMBEDDING.md](EMBEDDING.md#the-host-contract) for
the variables.

## Methods

### `initialize`

```json
→ {"jsonrpc":"2.0","id":"1","method":"initialize",
   "params":{"cwd":"/workspace","provider":"openrouter","model":"deepseek/deepseek-chat-v3.1",
             "reasoningEffort":"low","maxTokens":4096}}
← {"jsonrpc":"2.0","id":"1","result":{
     "serverInfo":{"name":"melchior","version":"0.2.0","melchior":"0.2.0"},
     "protocolVersion":1,
     "capabilities":{"sessions":{"resume":true,"cancel":true}},
     "plugins":{ ...load report, see CORDIS.md... }}}
```

`cwd`, `provider`, and `model` are required. `reasoningEffort` becomes
`reasoning.effort` on the model request, and `maxTokens` becomes
`max_tokens`. `initialize` may be called only once. `serverInfo.name` and
`version` are the distribution's; `melchior` is the Melchior version inside it.

It fails (-32603) when:
- neither `MELCHIOR_HOME` nor `DSH_HOME` is set;
- a patch is invalid;
- no provider named `provider` is configured;
- or a plugin fails to start. The message lists each failed entry and its error.

### `session/prompt`

```json
→ {"id":"2","method":"session/prompt","params":{"sessionId":"s1","contentBlocks":[{"type":"text","text":"..."}]}}
← {"id":"2","result":{"messageId":"msg-1a2b..."}}
```

The response comes back as soon as the prompt is admitted; the turn runs
afterwards and reports through notifications. Text blocks are joined with
newlines. Image and other blocks become a short placeholder, because the
model is not sent images.

| Situation | Result |
| --- | --- |
| Session open and idle | Admitted |
| Session doesn't exist | Created, then admitted |
| Session stored but not open in this process | `-32603 session "s1" already exists`: call `session/resume`, then prompt again. DeepSeek Harness clients match this exact text. |
| Session running a prompt | `-32603 session "s1" is busy with another prompt` |
| Invalid id (anything but 1–128 of `[A-Za-z0-9._-]`, or `.`/`..`) | `-32602 invalid session id` |
| No text | `-32602 the prompt has no text` |

### `session/resume`

```json
→ {"id":"3","method":"session/resume","params":{"sessionId":"s1"}}
← {"id":"3","result":{"sessionId":"s1","records":14}}
```

This opens a stored session in this process, after validating its whole
history. It never contacts the model and never replays input.

| Error (-32603) | When |
| --- | --- |
| `session "s1" not found` | Nothing is stored under that id. Nothing is created. |
| `session "s1" is already open or opening` | This process already has it open. |
| `session "s1" is already owned by another harness process` | Another live process holds its lock. |
| `session "s1" history is corrupt (...); refusing to resume it` | A committed line is not a valid record. The file is left byte-for-byte intact. |

### Other methods

| Method | Params | Result |
| --- | --- | --- |
| `session/cancel` | `{sessionId}` | `{cancelled: bool}`. Stops at the next model chunk or tool boundary, and kills a running shell command. Also accepted as a notification. |
| `session/close` | `{sessionId}` | `{closed: bool}`. Releases the session's lock. Refused while the session is busy. |
| `session/list` | none | `{open: [...], stored: [...]}` |
| `harness/plugins` | none | The load report from `initialize` |
| `ping` | none | `{}` |
| `shutdown` | none | `{}`. Then the process disposes plugins, kills child processes, and exits 0. |

Unknown methods return -32601. Malformed lines return -32700 or -32600 with a
null id. Closing stdin has the same effect as `shutdown`.

## Notifications

### `session.status`

```json
{"jsonrpc":"2.0","method":"session.status","params":{"sessionId":"s1","status":"running"}}
{"jsonrpc":"2.0","method":"session.status","params":{"sessionId":"s1","status":"idle"}}
```

Each prompt produces exactly one `running` and, after `turn/end`, exactly one
`idle`. The Python client stops reading a run at `idle`.

### `session.event`

```json
{"jsonrpc":"2.0","method":"session.event",
 "params":{"sessionId":"s1","event":{"id":"evt-...","type":"assistant/message","ts":"2026-10-25T21:04:05.120Z","data":{...}}}}
```

| `type` | `data` | Notes |
| --- | --- | --- |
| `agent/inbox/spliced` | `{inserted:[{id, role:"user", content:[{type:"text",text}]}]}` | `inserted[0].id` is the `messageId`; the client's receipt |
| `context/added` | `{source:"leviathan", chars}` | A turn hook attached context to the message |
| `assistant/delta` | `{text}` | Streamed answer text |
| `assistant/reasoning` | `{text}` | Streamed reasoning, if the model returns it |
| `assistant/retry` | `{attempt, error}` | A model request is being retried |
| `assistant/message` | `{message:{id, role:"assistant", content:[{type:"text",text}], toolCalls?}, transient?}` | One per model round. **The last one is the reply.** `transient: true` marks a harness-written message, such as an error explanation. |
| `tool/call` | `{id, name, arguments}` | `arguments` is the raw JSON string |
| `tool/result` | `{id, name, isError, durationMs, content}` | `content` is truncated to 2,000 characters here; the model sees up to `toolOutputChars` |
| `turn/end` | `{reason:{kind, message?}, usage}` | `kind`: `stop`, `max_rounds`, `cancelled`, `error`. `usage`: `modelCalls`, `promptTokens`, `completionTokens`, `totalTokens`, `cost` |

### Subagents

```json
{"method":"subagent.started","params":{"parentSessionId":"s1","childSessionId":"sub-...","toolCallId":"call_1","description":"..."}}
{"method":"subagent.finished","params":{"parentSessionId":"s1","childSessionId":"sub-...","reason":"stop"}}
```

The child's `session.event` and `session.status` notifications carry its own
`sessionId`. The Python client links them to the parent through
`subagent.started`.
