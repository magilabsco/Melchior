# Embedding Melchior in a product

There are three ways to put Melchior behind a product, from least to most
code:

| | You write | You get |
| --- | --- | --- |
| **Drive the binary** | A client for the stdio JSON-RPC protocol ([PROTOCOL.md](PROTOCOL.md)) and patch files | Melchior as shipped. Any DeepSeek Harness SDK client works unchanged. |
| **Patch it** | Cordis patch files: persona, model, disabled tools, your MCP gateway, Leviathan settings | The same binary, configured per agent ([CORDIS.md](CORDIS.md)) |
| **Build a distribution** | A small crate that depends on `melchior` and adds plugins | Your own binary with your own plugins, profiles and conventions, and the same command line |

Build a distribution when something can't be expressed as configuration: a
Leviathan backend for your own gateway, a tool that calls your API, a turn
hook with your business rules, a different session directory.

## A distribution

```toml
# Cargo.toml
[dependencies]
melchior = { path = "../vendor/melchior" }   # or a git dependency
```

```rust
// src/main.rs
use std::sync::Arc;
use melchior::cordis::loader::Entry;

fn main() -> std::process::ExitCode {
    let mut dist = melchior::Distribution::melchior().named("acme-agent", env!("CARGO_PKG_VERSION"));
    dist.registry.register(Arc::new(acme::AcmePlugin));
    let mut entries = dist.profile("default").unwrap();
    entries.insert(0, Entry::new("acme"));
    dist.set_profile("default", entries);
    melchior::cli::main(dist)
}
```

[`Distribution`](../src/distribution.rs) carries:

| Field | Default | Meaning |
| --- | --- | --- |
| `name`, `version` | `melchior`, the crate version | `serverInfo` in `initialize`, `--version`, and CLI messages |
| `registry` | every built-in plugin | Plugins that entries can name |
| profiles | `default` (also `sdk`, `headless`), `minimal` | `profile(name)`, `set_profile(name, entries)` |
| `default_profile` | `default` | Used when `--profile` is not given |
| `sessions_dir` | `melchior/sessions` | Session logs, relative to the home directory |
| `home_dir_name` | `.melchior` | Headless home under `$HOME` when `MELCHIOR_HOME` is unset |

Melchior has no dependencies, so a distribution that adds none builds
offline from a vendored copy: `cargo build --locked --offline`.

## Plugins

A plugin implements [`cordis::Plugin`](../src/cordis/mod.rs): a name, the
services it waits for, and `apply`, which registers things through its
context. Everything it registers is removed again when its scope is disposed.

| To add | Implement | Register into | Built-in example |
| --- | --- | --- | --- |
| A tool | `tools::Tool` | `tools` (via `tools::register`) | `ToolWeb`, `tools/web.rs` |
| A Leviathan backend | `leviathan::Backend` | `leviathan-backends`, then `backend: <name>` on the `leviathan` entry | `McpBackend`, `leviathan/mod.rs` |
| A system-prompt section | a `prompt::Section` closure | `prompt.sections` | `leviathan/briefing` |
| Context for each message | a `TurnHook` | `turn-hooks` | `leviathan/recall` |
| Work after each answer | a `TurnEndHook` | `turn-end-hooks` | `leviathan-memory/journal` |
| A model provider | `llm::Provider` | `llm` | `LlmPiAi`, `plugins.rs` |
| Secrets tools must not see | — | `Runtime::add_secret_env` | `leviathan` (`tokenEnv`) |

### A Leviathan backend for your gateway

If your product already serves Leviathan through its own permission-checked
gateway, write a backend so the briefing, recall and memory journal go
through the same checks and audit trail as the model's own calls. Reuse
`McpBackend` to find the gateway's tools and adapt only the calls:

```rust
use std::sync::Arc;
use melchior::json::{self, Json};
use melchior::leviathan::{Backend, McpBackend};

pub struct Gateway { inner: McpBackend }

impl Backend for Gateway {
    fn label(&self) -> String { "acme gateway".into() }
    fn has(&self, tool: &str) -> bool { self.inner.has(tool) }
    fn public_name(&self, tool: &str) -> Option<String> { self.inner.public_name(tool) }
    fn call(&self, tool: &str, args: Json) -> Result<Json, String> {
        let (server, raw) = self.inner.locate(tool).ok_or("not granted")?;
        let out = server.call(&raw.name, &args);
        if out.is_error { return Err(out.content); }
        json::parse(&out.content).map_err(|e| e.to_string())   // adapt your gateway's result shape here
    }
}
```

Register it from your plugin (`backends.add(ctx, "acme", Box::new(...))`) and
set `backend: acme` on the `leviathan` entry of your profile.

## The host contract

Whatever drives the binary over stdio relies on this:

| The host does | Melchior provides |
| --- | --- |
| Launches `<bin> --profile sdk --patch <file>...` (or `--serve`) with an explicit environment | The DeepSeek Harness command line |
| Sets `MELCHIOR_HOME` (or `DSH_HOME`) | Sessions in `<home>/<sessions_dir>`; nothing else is read from home in server mode |
| `initialize {cwd, provider, model}` | `serverInfo` and a load report: active, disabled, unknown, pending and failed entries, and the tool list |
| Prompts a stored session, gets `session "<id>" already exists`, calls `session/resume`, retries | That exact message, and resume semantics ([SESSIONS.md](SESSIONS.md)) |
| Waits for `agent/inbox/spliced` with its `messageId`, reads until `session.status idle` | Those events, in that order ([PROTOCOL.md](PROTOCOL.md)) |
| Sends `shutdown` and closes stdin | Exits at once and reaps every child process, including stdio MCP servers and local Leviathan |

Environment Melchior reads:

| Variable | Meaning |
| --- | --- |
| `MELCHIOR_HOME` / `DSH_HOME` | Harness home. Required for `--serve`. |
| `MELCHIOR_PERMISSION_MODE` / `DSH_PERMISSION_MODE` | `danger-full-access` turns off workspace confinement |
| `MELCHIOR_MODEL`, `MELCHIOR_PROVIDER` | Headless runs only (default `openrouter`) |
| `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL` | Model access, unless a patch names other variables |
| `LEVIATHAN_INDEX`, `LEVIATHAN_MEMORY`, `LEVIATHAN_URL`, `LEVIATHAN_TOKEN`, `LEVIATHAN_BIN` | Leviathan ([LEVIATHAN.md](LEVIATHAN.md)) |
| `MELCHIOR_LOG` | `error`, `warn`, `info` (default) or `debug`; logs go to stderr |
| `MELCHIOR_CURL` | Path to curl (default `/usr/bin/curl`, then `curl` on `PATH`) |

Anything a patch reads through `!!js process.env.NAME` is up to the host.

## Vendoring

To build a distribution from an archive of your own repository (no network,
no submodules), copy Melchior's `Cargo.toml`, `Cargo.lock`, `src/` and
`docs/` into it with a script that records the commit and a tree hash, and
have CI fail when the copy was edited in place. Change Melchior upstream and
re-run the script; never patch the copy.
