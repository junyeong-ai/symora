# src/daemon — Wire Layer Rules

The daemon serves CLI commands over a Unix socket so language-server sessions can be reused across invocations. A process that cannot address the daemon's socket — a home too deep for a Unix socket's path — or open its startup lock — a sandbox forbidding its directory — is wired in process instead (`DaemonClient::can_start_daemon`); invariant 3 is what makes that the same answer rather than a degraded one.

## Wire types are serialization-stable

Types in `wire.rs` (Symbol, Location, Diagnostic, …) are an external protocol. They:

- Use `String` for paths, not `PathBuf` — guarantees UTF-8 on the wire across platforms.
- Convert to/from `models::*` types via explicit `From` impls. Don't shortcut by serializing `models::*` directly.
- The ping handshake guarantees client and daemon were built from the same sources, features, target, and profile (`protocol::BUILD_ID`), so wire types evolve freely within a release — no cross-version compatibility shims. Optional fields still use `#[serde(skip_serializing_if = "Option::is_none")]` to keep payloads lean.

## Server cleanup

A daemon leaves its socket and pid files behind: the next daemon settles them when it claims the installation's path (`claim_socket`), under `daemon-<id>.bind.lock` and only after confirming nobody answers. Removing them at shutdown would give a slow teardown the power to unlink a successor's live socket, so liveness is always a connection attempt — never a path lookup — on both sides of the wire.

A daemon that has held no project and had no connection open for `idle_timeout` shuts down through `DaemonServer::shutdown`, and the client starts one on demand; work the daemon does outside a project or a connection does not keep it alive.

Project restart unloads only the addressed project's retained services and preserves its on-disk index. Active requests and outstanding server starts prevent restart or idle eviction. Release the projects-map lock before awaiting server shutdown so other projects remain serviceable.

## Request timeouts

LSP operations use the project's retained `lsp.timeout_secs` budget. CLI/MCP establish a shared deadline, and daemon requests carry remaining milliseconds. Admission waits and dispatch preparation consume that allowance; expired queued requests do not start project work. The server also clamps it to the project's retained configuration. Language and operation multipliers do not extend it. Timeout errors retain their typed wire identity. Non-LSP store/lifecycle operations keep their separate bounds.

A caller can stop waiting for a pooled server start without cancelling other waiters. The start has its own bounded lifetime and must release its pool claim on failure. Pending LSP requests send cancellation and remove pending state when dropped.
