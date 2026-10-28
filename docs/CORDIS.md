# Cordis in Rust

DeepSeek Harness is assembled from Cordis plugins: every capability (a model
provider, a tool, the system prompt, the MCP client) is a plugin that can be
configured, disabled, or added by a patch file, without touching code. That
composability is the part of DeepSeek Harness worth keeping, so
`src/cordis/` ports the model to Rust. It is about 600 lines with no
dependencies.

## Concepts

| Concept | JavaScript Cordis | Here |
| --- | --- | --- |
| Application | `Context` root | `App` (cheap to clone; shared state behind `Arc`) |
| Plugin | `{ name, inject, apply(ctx, config) }` | `trait Plugin { name(), inject(), apply(&Context) }` |
| Scope | Fiber/`EffectScope` per plugin | `Scope`: id, entry id, config, injects, effects, active flag |
| Context | `ctx` bound to a scope | `Context { app, scope }`; everything done through it is owned by its scope |
| Service | `ctx.provide('x')` / `ctx.x` | `ctx.provide("x", Arc<T>)` / `ctx.inject::<T>("x")` (typed via `Any` downcast) |
| Effect | `ctx.effect(() => dispose)` | `ctx.effect(move \|\| ...)`, run in reverse order on dispose |
| Events | `ctx.on` / `ctx.emit` | `ctx.on(name, handler)` / `ctx.emit(name, &Json)`; listeners die with their scope |
| Collections | services that track contributors | `Registry<T>`: entries are removed when the scope that added them is disposed |

### Scopes and disposal

`ctx.plugin(&plugin, entry_id, config)` creates a child scope, checks that
every service in `inject()` exists, and runs `apply`. If `apply` fails, the
half-built scope is disposed immediately, so a failed plugin leaves nothing
behind. The child is disposed together with its parent.

`App::dispose(scope)` does the following, in order:

1. Marks the scope inactive. Disposing twice is a no-op.
2. Runs its effects, newest first.
3. Withdraws every service the scope provided, and its event listeners.
4. **Disposes every active scope that injected a withdrawn service**, recursively.

Step 4 is Cordis's reactive dependency rule: a plugin never runs with a
missing dependency. For example, disposing the plugin that provides `jobs`
also disposes `tool-bash` if it injected `jobs`.

### Registries

Tools, LLM providers, prompt sections, and turn hooks are all `Registry<T>`
values provided as core services. `registry.add(&ctx, key, item)` inserts an
item and registers an effect on `ctx` that removes it. A plugin therefore
never has to undo what it registered; disposing its scope does that. Keys are
unique, so two plugins cannot register a tool with the same name.

```rust
pub struct ToolWeb;

impl Plugin for ToolWeb {
    fn name(&self) -> &'static str { "tool-web" }
    fn inject(&self) -> &'static [&'static str] { &["tools"] }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let tools = ctx.inject::<ToolRegistry>("tools")?;
        tools::register(&tools, ctx, Box::new(web::WebFetch))   // removed automatically on dispose
    }
}
```

## The loader

`src/cordis/loader.rs` turns a profile and patches into running plugins.

### Entries and profiles

An `Entry` is `{ id, name, config, disabled }`. The `id` is unique in the
list. The `name` selects the plugin implementation; by default it is the id.
A profile is an ordered list of entries. The built-in ones
(`plugins::profiles`) are:

```text
default (also sdk, headless):
                 system-prompt, llm-pi-ai, llm-deepseek, leviathan, leviathan-memory,
                 tool-goal, tool-todo, plan-mode, tool-fs, tool-str-replace-editor,
                 tool-fs-search, tool-jobs, tool-bash, tool-web, tool-skill, tool-subagent,
                 agent-loop
minimal:         system-prompt, llm-pi-ai, llm-deepseek, agent-loop
```

A distribution can replace these or add its own ([EMBEDDING.md](EMBEDDING.md)).
Order matters: plugins start in profile order, and their prompt sections
appear in that order.

Plugin names resolve through `PluginRegistry::resolve`. That tries the exact
name, then an alias, then the name with a `@deepseek-ai/dsh-` or
`@melchior/` prefix stripped. So `@deepseek-ai/dsh-mcp-client` in a DeepSeek
Harness tool patch loads `mcp-client`.

### Patches

A patch file is YAML: a list of operations, applied in order.

```yaml
- id: system-prompt                 # merge config into an entry (deep merge; arrays replace)
  config:
    personaPrefix: !!js process.env.DSH_SYSTEM_PROMPT
- id: tool-bash                     # disable (or re-enable with false)
  disabled: true
- id: some-entry                    # rename the implementation
  name: '@melchior/other-plugin'
- insert:                           # add entries at the end, or before:/after: an id
    - id: mcp-tools
      name: '@deepseek-ai/dsh-mcp-client'
      config: { serverName: tools, url: http://127.0.0.1:8000/v1/agents/a/mcp }
  after: agent-loop
- remove: tool-web                  # drop an entry entirely
```

A YAML mapping, as in `settings.yaml`, is read as `entry-id: config` merges.

Rules that make patches safe to share between harness versions:

- Disabling an id the harness doesn't have is silently fine, so DeepSeek
  Harness patches that disable plugins Melchior lacks (`tool-pwsh`,
  `tool-workflow`) load unchanged.
- Configuring an unknown id is a warning, reported in the load report, not an
  error.
- Inserting a duplicate id, a malformed operation, or an unknown
  `before`/`after` anchor is an error. `initialize` fails with the file name.

Patch values may use three tags:

| Tag | Meaning |
| --- | --- |
| `!!js <expr>` (or `!js`) | A *safe subset* of JavaScript; see [SECURITY.md](SECURITY.md#js-in-patch-files) |
| `!env NAME` | The variable's value, or null if unset |

Anchors and aliases (`&a`, `*a`) and duplicate keys are rejected.

Headless runs also apply `<home>/settings.yaml` and
`<home>/cordis.patch.yml`, before any `--patch` files. Server runs never read
the home directory for configuration: the host passes everything explicitly.

### Activation

`loader::start(root, registry, entries)` runs in passes:

1. Disabled entries are recorded and skipped. Entries naming an unknown
   plugin are recorded as `unknown` and skipped.
2. Each pass walks the remaining entries in profile order and applies every
   entry whose injected services all exist.
3. Passes repeat until nothing changes. Entries still waiting are reported as
   `pending`, with the missing services listed.

Because core services exist before activation, most plugins start in the
first pass and in profile order, which keeps prompt sections and tool order
deterministic. `agent-loop` is last, so it sees every LLM provider.
Everything ends up in the load report:

```json
{"active":[{"id":"system-prompt","plugin":"system-prompt"}, ...],
 "disabled":["tool-bash", ...], "unknown":[], "pending":[], "failed":[],
 "entries":[...], "tools":["create_goal","exit_plan_mode","get_goal","todo_write","update_goal", ...],
 "warnings":[]}
```

## Writing a plugin

1. Implement `Plugin` in `src/plugins.rs` (or its own module).
2. List the services it needs in `inject()`.
3. In `apply`, contribute through registries and declare cleanup with
   `ctx.effect`. Read settings from `ctx.config()`.
4. Register it in `plugins::registry()`. Add it to a profile if it should be
   on by default; otherwise users can add it with an `insert` patch.

The `leviathan` plugin (`src/leviathan.rs`) is a complete example. It
provides a service, adds a prompt section and a turn hook, and depends on
`mcp`, `prompt`, and `turn-hooks`.
