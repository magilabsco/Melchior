# Differences from DeepSeek Harness

Melchior can stand in for DeepSeek Harness wherever a host drives it through
the SDK protocol and patch files. It is not a clone of every DeepSeek Harness
feature. Where behaviour differs, it is deliberate.

## Same

- The CLI (`--profile sdk`, `--patch`), the stdio JSON-RPC protocol, its
  events, and its error messages.
- Plugin entry ids and patch semantics, including `!!js` for everything
  real-world patches do (reading environment variables).
- `DSH_HOME`, `DSH_PERMISSION_MODE`, and `DSH_SYSTEM_PROMPT` (through a patch)
  still work; `MELCHIOR_HOME` and `MELCHIOR_PERMISSION_MODE` are the native
  names.
- Tool names and argument names: `read`, `write`, `edit`, `glob`, `grep`,
  `bash`, the `job_*` tools, `web_fetch`, `skill`, `subagent`, the goal
  tools, `todo_write`, and `exit_plan_mode`.
- MCP tool naming (`mcp__<server>__<tool>`) and the
  `@deepseek-ai/dsh-mcp-client` package name.
- With every workspace tool disabled, the tool set is exactly `create_goal`,
  `exit_plan_mode`, `get_goal`, `todo_write`, and `update_goal`, plus MCP
  tools.

## Added

- **Leviathan at the centre** ([LEVIATHAN.md](LEVIATHAN.md)): an index
  briefing in the system prompt, recall on every message, first-class search
  tools, `leviathan_index` to index new data, and memory across
  conversations. DeepSeek Harness has none of these.
- **MCP over stdio** as well as streamable HTTP.
- **A tool-name map** in the system prompt. Agent prompts say `files.query`;
  the model sees which function that is.
- **A load report** in the `initialize` result and from `harness/plugins`:
  active, disabled, unknown, pending, and failed entries, and the final tool
  list.
- **One forced answer.** If the model stops on tool output without
  answering, the harness asks once more, with tools disabled.
- **Distributions.** Products build their own binary from the `melchior`
  crate with their own plugins and profiles ([EMBEDDING.md](EMBEDDING.md)).

## Not implemented

| DeepSeek Harness | Why not |
| --- | --- |
| `interrupt_agent`, `list_agents`, `send_message`, `subagent_fork` (`tool-subagent-control`, `tool-subagent-list-agents`, `tool-subagent-fork`) | Concurrent, addressable agent swarms. Melchior runs one agent per process; `subagent` covers delegation. |
| `workflow`, `tool-ralph` | Long-running orchestration belongs to the host |
| `web_search` | Needs a search provider and account; `web_fetch` is available |
| `read_image` | The model request carries text only |
| `tool-pwsh`, `tool-plugin-manager`, `config-editor`, `workspace-dependencies`, `skill-office`, `tool-working-directory`, `llm-deepseek-account` | Not applicable on Linux, or self-modifying configuration that a hardened harness shouldn't allow |
| Arbitrary `!!js` | Replaced by the safe subset ([SECURITY.md](SECURITY.md#js-in-patch-files)) |
| Non-OpenAI provider APIs (`api: anthropic-messages`, and so on) | OpenAI-compatible APIs (OpenRouter, DeepSeek, local servers) cover the models Melchior targets. Others can be added as `Provider` implementations. |

## Sessions

DeepSeek Harness's session files are not read. Melchior stores sessions in
`<home>/melchior/sessions/` in its own format ([SESSIONS.md](SESSIONS.md)), so
neither format can be misread as the other, and DeepSeek Harness files are
never modified.

The consequence: a conversation begun under DeepSeek Harness continues under
Melchior with an empty history, because the id is new to this store.
Conversations begun under Melchior keep their full history across restarts,
upgrades, and crashes.
