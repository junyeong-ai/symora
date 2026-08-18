# src/daemon — Wire Layer Rules

The daemon serves CLI commands over a Unix socket so language-server sessions can be reused across invocations.

## Wire types are serialization-stable

Types in `wire.rs` (Symbol, Location, Diagnostic, …) are an external protocol. They:

- Use `String` for paths, not `PathBuf` — guarantees UTF-8 on the wire across platforms.
- Convert to/from `models::*` types via explicit `From` impls. Don't shortcut by serializing `models::*` directly.
- The ping handshake guarantees client and daemon were built from the same sources, features, target, and profile (`protocol::BUILD_ID`), so wire types evolve freely within a release — no cross-version compatibility shims. Optional fields still use `#[serde(skip_serializing_if = "Option::is_none")]` to keep payloads lean.

## Server cleanup

A daemon leaves its socket and pid files behind: the next daemon settles them when it claims the path (`claim_socket`), under `daemon.bind.lock` and only after confirming nobody answers. Removing them at shutdown would give a slow teardown the power to unlink a successor's live socket, so liveness is always a connection attempt — never a path lookup — on both sides of the wire.

## Request timeouts

Each request is bounded by a per-request `tokio::time::timeout` sized per language/method (`estimate_request_timeout` in `server/connection.rs`). A timed-out request returns a structured error; there is no separate cancellation channel.
