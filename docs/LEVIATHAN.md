# Leviathan: the data layer at the centre of every turn

Melchior is built for agentic work over datasets far larger than a context
window: ticket histories, logs, CRM and ERP exports, maintenance records,
database tables. [Leviathan](https://github.com/elstongun/leviathan) is how
it reaches them. Leviathan turns records of any size (CSV/TSV, JSON/JSONL,
SQLite, or any database CLI's output) into a ranked full-text index that
answers a question in a few hundred tokens with cited result cards, whether
it holds a thousand records or a million.

Melchior makes that index part of every turn rather than a tool the model may
or may not think to call:

| | What the model gets |
| --- | --- |
| **Briefing** | A system-prompt section describing what is indexed: record and group counts, the largest groups, date range, filter fields, and how to search. When nothing is indexed yet, how to index. |
| **Recall** | Before the first model call of a turn, the user's message is searched and the top cards arrive with it as `<workspace_recall>`. |
| **Tools** | `leviathan_search`, `leviathan_get`, `leviathan_resolve_group`, `leviathan_describe`, and `leviathan_index` to index new data the user points at. |
| **Memory** | The agent's own notes across conversations: a briefing on the first message, recall on every message, a journal of answered questions, and `memory_remember` / `memory_recall` / `memory_forget`. See [Memory](#memory). |

The index is what the data says. Memory is what the agent learned while
working with the user.

## Connecting to Leviathan

Leviathan is its own program (Apache-2.0, `cargo install leviathan-index`;
0.2 or newer for memory and `serve`), so Melchior keeps no dependencies and
talks to it over MCP. The `leviathan`
plugin picks one **backend**:

| Backend | Selected when | How it connects |
| --- | --- | --- |
| `local` | `index` or `memory` is set, or `LEVIATHAN_INDEX` / `LEVIATHAN_MEMORY`, or `--index` / `--memory` on the command line | Spawns `leviathan --index <file> --memory=<file> mcp` as a stdio MCP server |
| `remote` | `url` is set, or `LEVIATHAN_URL` | MCP over HTTP to `leviathan serve --http` (`<url>/mcp`), with `Authorization: Bearer $LEVIATHAN_TOKEN` |
| `mcp` | An `mcp-client` server exposes Leviathan's tools | Calls them through that server: tools named `leviathan.search` (or `leviathan_search`) on any server, or `search` on a server whose name contains `leviathan` |
| implicit `local` | Nothing above, and `leviathan` is on `PATH` | A per-workspace index and memory under `<home>/leviathan/`, started on the first turn only if no other backend answers |
| anything else | A plugin registered it in `leviathan-backends` | Whatever that plugin does (a host's permission-checked gateway, for example) |

`backend: auto` (the default) prefers a configured `local` or `remote`, then
a backend another plugin registered, then a discovered `mcp` server, then the
implicit local index. Set `backend` to a name to use exactly that one.

```sh
# One task over one dataset: index it, then ask.
leviathan index tickets.csv --id id --group customer --date created --index tickets.db
melchior --index tickets.db --memory memory.db "Which customers hit the SSO login loop after 4.2?"

# Or let the agent index it: with leviathan on PATH, nothing needs configuring.
melchior "Index ./exports and tell me which machines had repeat bearing failures"

# A shared index and memory on another machine.
LEVIATHAN_URL=https://leviathan.internal LEVIATHAN_TOKEN=... melchior --serve
```

In a patch file:

```yaml
- id: leviathan
  config:
    index: data/leviathan.db        # relative to the workspace
    memory: /srv/agents/memory.db   # or false to run without memory
    namespace: team-ops             # memory namespace forced on every memory call
```

A local or remote backend that is unreachable at start is retried before each
turn, at most every 15 seconds. If a local server exits, the next call
restarts it.

## Tools

With a `local` or `remote` backend, Leviathan's own tools are registered as
first-class tools. Their descriptions and schemas are Leviathan's, so the
`leviathan_search` description includes a summary of the data actually
indexed. They answer in Leviathan's compact text cards.

| Tool | Leviathan tool | Notes |
| --- | --- | --- |
| `leviathan_search` | `search` | Ranked full-text search with `group`, `where`, `since`/`until`, `sort`, paging |
| `leviathan_get` | `get` | One complete record by id |
| `leviathan_resolve_group` | `resolve_group` | A loose name ("acme") to exact groups |
| `leviathan_describe` | `describe` | Fields, groups, filter values, dates |
| `memory_recall`, `memory_remember`, `memory_forget`, `memory_describe` | `recall`, `remember`, `forget`, `memory_describe` | Present when memory is on; `ns` is forced when `namespace` is set |
| `leviathan_index` | `leviathan index` (CLI) | Local backend only. See below. |

Leviathan lists its data tools only once an index exists, so before anything
is indexed the agent sees `leviathan_index` and the memory tools. After
indexing, Melchior restarts the server and the search tools appear in the
same turn.

With an `mcp` backend the model calls the server's tools under their MCP
names (`mcp__<server>__leviathan_search`); the briefing and recall use the
same tools.

### Automatic indexing

Before the first model call of a session, Melchior indexes the workspace's
large data files itself (`autoIndex`, on by default; files of at least
`autoIndexMinBytes`, 1 MB): no model turn, no tokens, and the first message
already gets a briefing on the data and recall. Leviathan infers the mapping.

- One index holds one record shape, so the largest set of files that share a
  shape (CSV header, or JSON keys) is indexed; the briefing names the others.
- Only an index Melchior manages is written: the implicit per-workspace
  index, or a configured `index` that does not exist yet. An existing
  configured index is never rebuilt behind the user's back.
- Rebuilds are cheap: Leviathan skips a build when the sources did not change.
- The inferred mapping has no `group`; the agent can re-index with
  `leviathan_index` when scoped search by entity would help.

### `leviathan_index`

Builds or refreshes the local index from files the agent can reach:

```json
{"paths": ["exports/tickets.csv"], "id": "Ticket ID", "group": ["customer_id"], "date": ["created_at"], "filter": ["status", "priority"]}
```

- Fields are Leviathan paths (`a.b`, `items[].name`). Without them Leviathan
  infers a mapping; `config` points at a `leviathan.toml` instead.
- `format` and `sql` cover stdin-less sources: `sqlite` with a query, or a
  forced format.
- Builds are atomic and skipped when nothing changed (`force` rebuilds).
- Paths are confined to the workspace unless the agent has full access.
  The index file is fixed by configuration; the agent can't write elsewhere.
- Runs up to `indexTimeoutSeconds` (an hour by default).

## Briefing

Before each model request, the `leviathan/briefing` prompt section describes
the index. It calls `describe` and caches the result for
`briefingTtlSeconds`, 120 by default, so a long tool loop doesn't hammer it.
The plugin sits right after the model providers in the default profile, so
the briefing comes before the sections that change mid-conversation (goal,
task list) and the prompt prefix stays cacheable.

```text
# Workspace index (Leviathan)
It holds 1204 work orders across 9 lines, dated 2026-01-02 to 2026-10-24.
Largest lines: Line 4 (line-4): 640; Line 2 (line-2): 212; ...
Exact-match filters for `where`: status (open, closed); priority (high, low).
Use `leviathan_search` for documents, notes, and free text across every source (ranked, with record ids).
It ORs your words and ranks by relevance, so search with a few specific terms rather than a full sentence.
Resolve a loose source or entity name with `leviathan_resolve_group` first. Open a full record with `leviathan_get`.
Treat the index as the way into the data: it answers in a few hundred tokens whatever its size, so search it
before reading files, and never read or grep a large dataset whole. Index new data the user gives you with `leviathan_index`.
Each user message may arrive with a <workspace_recall> block: ... cite record ids or source names ...
```

Every line comes from `describe`: record and group nouns (`about.record`,
`about.group`), counts, date range, largest groups, and filter fields with
their most common values. The model learns the real shape of this data, not a
generic description. The advice on how to search follows from how Leviathan
ranks: it ORs the query terms, minus stopwords, and ranks by BM25.

When nothing is indexed and `leviathan_index` is available, the briefing
says so and tells the model to index data before working with it. Without
`leviathan_index`, an empty index is reported as empty and recall is skipped.

## Recall

The `leviathan/recall` turn hook runs once per top-level turn, after the user
message is persisted and before the first model call:

1. **Query.** The user's text, with a leading bracketed context line (such
   as a host's `[Today is …]` preface) removed, whitespace collapsed, capped
   at 400 characters. Messages shorter than `minQueryChars` (8) are skipped.
2. **Search.** `search {query, limit: recallLimit}` (5 by default).
3. **Filter.** Cards with `relevance < minRelevance` (0.15) are dropped.
   Leviathan's relevance is relative to the best match, so this removes the
   long tail without a corpus-specific threshold.
4. **Attach.** The cards are rendered as a `<workspace_recall>` block,
   persisted as `message/context` linked to the user message, and sent as a
   second text part of that message:

```text
<workspace_recall source="leviathan" shown="3" matches="27">
Retrieved automatically from the workspace index for this message. It may be incomplete or partly irrelevant; prefer exact queries for numbers, and cite record ids.
1. Filler 3 bearing failure [id: wo-17] (Line 4, 2026-09-30)
   status: open · machine: Filler 3 · priority: high
   Replaced the drive-side bearing on filler 3 after vibration alarms…
2. ...
(24 more matches; page through them with `leviathan_search`.)
</workspace_recall>
```

### Why it works this way

- **In the user message, not the system prompt.** Recall belongs to one
  question. Keeping it there leaves the system prompt stable, so it can be
  cached across turns, and lets old turns keep the context they were
  answered with.
- **Persisted with the turn.** A resumed session rebuilds exactly what the
  model saw. The search is never re-run against an index that has changed.
- **Doesn't replace tools.** The model still searches, filters and opens
  records itself. Recall gives it a running start and record ids to cite.
- **Cheap when irrelevant.** A greeting is skipped. A question the index
  can't answer attaches nothing, because of the relevance floor.
- **Fails open.** Any error (server down, tool not granted, a non-JSON
  result) means no recall. It is logged at `info`, and the turn proceeds.

## Memory

### Where conversation history lives

| Store | What it holds | Who reads it |
| --- | --- | --- |
| Session log, `<home>/melchior/sessions/<id>/events.jsonl` | The exact conversation: every message, tool call, result, and attached context, append-only | Melchior, to rebuild and resume that one conversation ([SESSIONS.md](SESSIONS.md)) |
| Leviathan memory (its own SQLite file) | Short claims: what the agent was told or decided, plus one note per answered question | Every later conversation that uses the same memory and namespace |

The session log stays the source of truth for a conversation. Memory is not a
copy of it: Leviathan merges restatements, replaces superseded values, caps
each claim at 400 characters, and refuses secrets, so it can't rebuild a
transcript and isn't meant to. What it does well is give a new conversation
the few things from earlier ones that matter, within a token budget.

### How memory gets written

- **By the model.** The `# Memory (Leviathan)` prompt section tells it to
  save durable things the user says (a preference, a decision and why, a
  correction, a standing fact) one claim at a time, with a subject and key
  for anything that can change. Writing the same subject and key again
  replaces the old value, and the old one stays in history.
- **By the harness (the journal).** After each answered top-level turn
  (`turn/end` with reason `stop`), the `leviathan-memory/journal` turn-end
  hook saves one memory:

  ```text
  kind: event · importance 2 · tags [conversation] · source conversation:<session id> · expires 90d
  Asked: "What caused the Line 4 capper outage in September?". Answered: Cause: a cracked chuck spindle bearing (PN ZK-4471-B) …
  ```

  The answer part is the opening prose of the answer, with headings,
  tables, and code dropped. No model is called to summarise. The write runs
  on a background thread, so it never delays or fails the turn; shutdown
  waits up to five seconds for writes still in flight. Short messages,
  unanswered turns, and sessions whose id starts with a `journalSkip` prefix
  (for example evaluation runs) are not journaled.

### How memory gets read

The `leviathan-memory/recall` turn hook runs after `leviathan/recall` and
attaches one `<agent_memory>` block to the user message, persisted as
`message/context` (source `leviathan-memory`):

```text
<agent_memory source="leviathan">
Your own notes from earlier conversations in this workspace (current values only). They can be out of date: …
Standing memories:
[preference] Sarah Chen · downtime_summary.format: Sarah Chen wants every weekly downtime summary delivered as a table with one row per line — never prose. (2026-10-26 · m01m5v8ecef3b0w5k)
Matching this message:
[event] Asked: "What caused the Line 4 capper outage in September, …". Answered: Cause: a cracked chuck spindle bearing … (2026-10-26 · m01m5v8egjdb8ecfj)
</agent_memory>
```

- **Standing memories** appear on the first message of a session only:
  `recall` with no query returns pinned and most important current memories,
  spread across subjects. Journal events are left out, because they would
  crowd out durable facts.
- **Matching this message** is `recall {query}` with the same text as
  workspace recall. Journal events take part here, which is how "have we
  looked into this before?" finds an earlier conversation. It is skipped
  when everything it found is already among the standing memories.
- The lines are rendered by Melchior from Leviathan's JSON, in Leviathan's
  own line format. Leviathan's text rendering is not used because it ends
  with advice naming tools by Leviathan's own names.

### Isolation and safety

- **Namespaces.** With `namespace` set, every memory call (the hooks, the
  journal, and the model's own tool calls) carries that `ns`, replacing any
  the model sent. A host that shares one Leviathan between agents should set
  it per agent, or force it in its own gateway.
- **Secrets are refused** by Leviathan on every write (keys, tokens, JWTs,
  credentials in URLs, high-entropy strings). A refused journal write is
  logged and skipped.
- **Memory is a note, not data.** The block's header tells the model to
  re-check numbers in the index. Memory says what was said, not what is true
  now.
- **Fails open.** Without memory tools, or with Leviathan down, there is no
  memory context and no journal; the turn runs as before.

## Tuning

Patch the `leviathan` entry (all keys in [PLUGINS.md](PLUGINS.md#leviathan)):

```yaml
- id: leviathan
  config:
    recallLimit: 8
    minRelevance: 0.25
    briefingTtlSeconds: 30
```

Turn either half off with `briefing: false` or `recall: false`, or disable
the plugin with `disabled: true`. Memory is tuned on its own entry:

```yaml
- id: leviathan-memory
  config:
    journal: false        # keep briefing and recall, stop the per-turn notes
    recallBudget: 600
```

## Verifying it

`tests/end_to_end.rs` runs the real binary three ways:

- **Through a gateway.** A loopback MCP server re-exports Leviathan as
  `leviathan.*` beside a tool of its own. The test asserts the briefing leads
  the system prompt, recall is the second part of the user message (without
  the leading context line), memory briefing and recall share one
  `<agent_memory>` block, no namespace is sent unless configured, the gateway
  saw the bearer token on every request, and the turn was journaled before
  `shutdown` returned.
- **With a real local Leviathan** (`LEVIATHAN_BIN` or `leviathan` on `PATH`;
  skipped otherwise). The agent indexes a CSV with `leviathan_index`,
  `leviathan_search` appears in the same turn and finds the record, the next
  message is briefed on the indexed data and gets recall, and the memory file
  is written.
- **Headless, with nothing configured.** `leviathan` on `PATH` is enough: the
  agent indexes the workspace's data into the implicit per-workspace index
  and answers from search.
