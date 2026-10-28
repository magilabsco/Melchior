# Security model

Melchior started as a replacement for DeepSeek Harness after a CVE in it: a
Node application with a large npm dependency tree that evaluates
configuration as JavaScript. The goal is a harness whose entire attack
surface is small enough to read.

## Supply chain: zero dependencies

- `Cargo.lock` lists one package: this crate. There are no crates, no build
  scripts, and no proc macros. `cargo build --locked --offline` cannot fetch
  anything.
- `#![forbid(unsafe_code)]` applies crate-wide, through `[lints.rust]` in
  `Cargo.toml`.
- JSON, YAML (the subset patches use), HTTP/1.1, SSE, MCP (HTTP and stdio),
  the `!!js` evaluator, glob matching, and date formatting are all
  implemented on `std`. Each parser has unit tests for its edge cases.

Two external programs are used:

- **curl**, for `https`. Implementing TLS without dependencies is not a
  responsible option; the system curl is maintained and patched by the
  operating system. Plain `http` (loopback gateways, tests) uses `std::net`
  directly.
- **leviathan**, when Leviathan runs locally. It is a separate binary with
  its own supply chain (Apache-2.0, Rust), started as a stdio MCP server or
  for `leviathan_index`. Nothing of it is linked into Melchior.

### How curl is invoked

- The URL, method, headers (including `Authorization`), and timeouts are
  written to curl's **stdin** as a `--config -` file. Nothing sensitive appears
  in `argv`, so `ps` and `/proc/*/cmdline` show only
  `curl --silent --show-error --no-buffer --include --config -`.
- The request body is staged in a `0600` file created with `O_EXCL` under
  `$TMPDIR`. It is deleted when the response is dropped, even on error or
  panic, by the `CurlGuard` destructor.
- curl's environment is cleared, except `PATH`, `HOME`, CA-bundle variables,
  and proxy variables. Model keys and gateway tokens are not in its
  environment.
- Header names and values containing CR, LF, or NUL are rejected before any
  request is built. Config values are quoted and escaped. A value with a line
  break is refused, so a header can't inject curl options.
- Every curl child is tracked, and killed on `shutdown` or harness exit.

## `!!js` in patch files

DeepSeek Harness evaluated `!!js` tags with a JavaScript engine, so a patch
file was code. Real-world patches use `!!js` only to read environment
variables:

```yaml
personaPrefix: !!js process.env.DSH_SYSTEM_PROMPT
models: !!js '[{ id: process.env.AGENT_MODEL }]'
Authorization: !!js '`Bearer ${process.env.GATEWAY_TOKEN}`'
```

`src/jsexpr.rs` evaluates exactly that language and nothing more:
- `process.env.X` and `process.env['X']`;
- string, number, boolean, `null`, and `undefined` literals;
- template literals;
- array and object literals;
- `||`, `??`, and parentheses.

Calls, member access on anything but `process.env`, assignment, other
globals, `require`, and `import` all fail with `unsupported !!js expression`
at load time. Tests assert that `require('child_process')`, `eval(...)`,
`process.exit()`, and similar are rejected. YAML anchors and aliases are
rejected too, which rules out "billion laughs" expansion. Parser nesting is
capped at depth 256.

## Secrets

| Secret | Where it goes |
| --- | --- |
| Model key (`OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, or the provider's `apiKeyEnv`) | Read when a request is built; sent to curl through stdin |
| Gateway tokens | Evaluated into an MCP client's headers at load time |
| Leviathan token (`LEVIATHAN_TOKEN`, or `tokenEnv`) | Sent as a bearer token to a remote Leviathan |
| Agent prompt (`DSH_SYSTEM_PROMPT`) | Becomes the persona |

None of these reach tool subprocesses. `Runtime::child_env` strips them, plus
any `apiKeyEnv` a provider names and any variable a plugin registers with
`add_secret_env`, from the environment of `bash`, `grep`, background jobs,
stdio MCP servers, and Leviathan. None are written to the session log; the
end-to-end test asserts this. Logs never print headers or request bodies.

## Confinement

Without `danger-full-access`:
- File tools refuse paths outside the workspace, after resolving symlinks
  through the deepest existing ancestor. `leviathan_index` applies the same
  check to every path it indexes; the index file itself is fixed by
  configuration.
- `bash` runs in its own Bubblewrap with a read-only root, a writable
  workspace, the harness home hidden, and no network. If Bubblewrap is
  missing, `bash` refuses rather than running unconfined.
- Leviathan and stdio MCP servers are operator-configured programs and run
  unconfined, like curl.

`danger-full-access` disables these checks. A host should grant it only to
trusted, supervised agents, and can add an outer layer of its own: running
the whole harness inside Bubblewrap or a container with only the agent's
workspace and home mounted, and disabling every tool a role doesn't need
with a patch.

## Data from the index is data

Leviathan recall and tool results are untrusted text: indexed documents can
contain anything, including instructions. They go into user-role content and
tool messages, never the system prompt. Even if the model is persuaded by
injected text, it can do no more than its tools allow. A host that routes
tools through a permission-checked gateway keeps that as the one point of
enforcement; Melchior's own Leviathan lookups can go through the same
gateway (a Leviathan backend, [EMBEDDING.md](EMBEDDING.md)).

Memory is the exception: it is a write that needs no approval, so it can
make an injection outlast its conversation. A planted instruction that
persuades the model to call `memory_remember` will be recalled into later
conversations. The journal can also carry injected text, since it copies the
opening of an answer. What limits this:
- With `namespace` set (or a gateway that forces it), writes reach only that
  namespace, so one agent can't write into another's memory, and memory
  can't change indexed data.
- Recalled memory arrives as user-role content in an `<agent_memory>` block
  that labels it as notes that can be out of date. It is never placed in the
  system prompt.
- Leviathan refuses anything that looks like a secret, so memory can't be
  used to stash or move credentials.
- `memory_forget` retires a bad memory, and the history it leaves can be
  audited. Turn memory off with `memory: false` on the `leviathan` entry, or
  turn off the journal alone with `journal: false` on `leviathan-memory`.

## Reporting

Report vulnerabilities privately through GitHub's private vulnerability
reporting on this repository. Don't open a public issue.
