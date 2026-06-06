# src/daemon — Wire Layer Rules

The daemon serves CLI commands over a Unix socket so language-server sessions can be reused across invocations.

## Wire types are serialization-stable

Types in `wire.rs` (Symbol, Location, Diagnostic, …) are an external protocol. They:

- Use `String` for paths, not `PathBuf` — guarantees UTF-8 on the wire across platforms.
- Convert to/from `models::*` types via explicit `From` impls. Don't shortcut by serializing `models::*` directly.
- The ping version handshake guarantees client and daemon come from the same binary, so wire types evolve freely within a release — no cross-version compatibility shims. Optional fields still use `#[serde(skip_serializing_if = "Option::is_none")]` to keep payloads lean.

## Server cleanup

The socket file is removed explicitly on shutdown (`remove_file` in `server/mod.rs`), not via `Drop`. A process killed before that runs can leave a stale socket — `daemon start` runs a liveness check before deciding to (re)spawn, and the server then removes any stale socket immediately before `UnixListener::bind`.

## Request timeouts

Each request is bounded by a per-request `tokio::time::timeout` sized per language/method (`estimate_request_timeout` in `server/connection.rs`). A timed-out request returns a structured error; there is no separate cancellation channel.
