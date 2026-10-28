# Sessions

A session is one conversation. It survives harness restarts, image upgrades,
and crashes, and it is owned by at most one harness process at a time.

The session log is the exact, complete record of that conversation and the
only thing the harness rebuilds a conversation from. What carries over
*between* conversations is the agent's Leviathan memory, which is a separate,
deliberately lossy store of short claims; see
[LEVIATHAN.md](LEVIATHAN.md#where-conversation-history-lives).

## Layout

```text
$MELCHIOR_HOME/
  melchior/sessions/<session-id>/
    events.jsonl     append-only history, one record per line
    owner.lock       held with an exclusive OS file lock (flock) while open
```

The directory is the distribution's `sessions_dir` ([EMBEDDING.md](EMBEDDING.md)).
Its prefix keeps these files apart from DeepSeek Harness's own session files
when both share a home directory, so the two formats can never be confused. Session ids are 1–128 characters of `[A-Za-z0-9._-]`, and never
`.` or `..`, so an id can't escape the directory.

## Records

Each line is a single JSON object:

```json
{"v":1,"seq":3,"ts":"2026-10-25T21:04:05.120Z","type":"message/assistant","data":{...}}
```

`seq` starts at 1 and strictly increases. The first record is always
`session/created` with the session id.

| `type` | `data` |
| --- | --- |
| `session/created` | `{sessionId, harness: "melchior/0.2.0"}` |
| `message/user` | `{id, content}` |
| `message/context` | `{messageId, source, text}`: context attached to that user message, such as Leviathan recall |
| `message/assistant` | `{id, text, toolCalls:[{id, name, arguments}], model, finishReason, usage, reasoningDetails?}` |
| `tool/result` | `{toolCallId, name, content, isError}` |
| `turn/end` | `{reason:{kind, message?}, usage}` |
| `state` | `{key, value}`: the latest value per key wins (`goal`, `todos`, `plan`) |

What is **not** stored:
- The system prompt is rebuilt on every request, so an updated agent prompt
  takes effect immediately in old sessions.
- Streaming deltas.
- Credentials.

## Durability

Every record is written with a single `write` and then `fdatasync`ed before
the harness acts on it. The user message is durable before the model is
called. An assistant message with tool calls is durable before any tool runs.
A tool result is durable before the next model request.

Committed bytes are **never rewritten**. The one repair is a final line with
no terminating newline, which can only be a write interrupted by a crash. It
is truncated on resume, and a warning is logged.

## Ownership

| Operation | Rule |
| --- | --- |
| `session/prompt` on a new id | Creates the directory with an atomic `mkdir`, takes the lock, and writes `session/created` |
| `session/prompt` on a stored id that isn't open here | Refused with `session "<id>" already exists`; the caller must resume deliberately |
| `session/resume` | Takes the lock *before* reading the file, then validates every line |
| Lock held by another process | `already owned by another harness process` |
| Open in this process | `already open or opening` (also covers two resumes racing) |

The lock is an OS lock on an open file descriptor. It is released when the
process exits for any reason, including `SIGKILL`, so a crashed harness never
strands a session. A host that stops an agent's old process before starting
a new one never contends for the lock in normal operation.

## Resume and validation

A history is accepted only if:
- it is UTF-8;
- every committed line parses;
- every line has `v: 1`, a `type`, and an increasing `seq`;
- and the first line creates this session id.

Anything else fails with `history is corrupt (line N: ...)`, and the file is
left exactly as it was, however often resume is retried.

## Rebuilding the conversation

`agent::build_messages` turns records into the provider's message list:

- `message/user` becomes a user message. If `message/context` records exist
  for that message id, the content becomes a list of text parts: the user's
  text first, then each context.
- `message/assistant` becomes an assistant message with `tool_calls`.
  `reasoning_details` is sent back only for the current turn, which is what
  OpenRouter reasoning models expect.
- `tool/result` becomes a tool message. Error results are prefixed with
  `Error:`.
- A tool call with no recorded result (the process died while the tool ran)
  gets a synthetic result. It says the call was interrupted and may or may
  not have taken effect. The rebuilt sequence is therefore always valid, and
  the model is told the truth.

If the request would exceed `contextChars` (600,000 characters by default),
compaction first elides old tool outputs, then drops the oldest whole
exchanges. The system prompt and the latest user exchange are always kept.
The session log itself is never compacted.

Subagent sessions are ephemeral: in memory only, never written to disk.
