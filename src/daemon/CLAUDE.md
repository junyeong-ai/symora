# src/daemon — Wire Layer Rules

The daemon serves CLI commands over a Unix socket so language-server sessions can be reused across invocations.

## Wire types are serialization-stable

Types in `wire.rs` (Symbol, Location, Diagnostic, …) are an external protocol. They:

- Use `String` for paths, not `PathBuf` — guarantees UTF-8 on the wire across platforms.
- Convert to/from `models::*` types via explicit `From` impls. Don't shortcut by serializing `models::*` directly.
- Add new fields with `#[serde(skip_serializing_if = "Option::is_none")]` and a `Default` so older clients still parse.

## Server cleanup

`Drop` on the server removes the socket file. If the process is killed before `Drop` runs, a stale socket can remain — `daemon start` handles this by checking liveness before binding.

## Request cancellation

Cancellation is best-effort. If the receiver of a oneshot has dropped, sending a response is a no-op (`let _ = sender.send(_)` is correct here, not a bug).
