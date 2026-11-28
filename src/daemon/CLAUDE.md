# src/daemon — Wire Layer Rules

The daemon serves CLI commands over a Unix socket so language-server sessions can be reused across invocations. A process that cannot address the daemon's socket — a home too deep for a Unix socket's path, or a `HOME` or `XDG_CONFIG_HOME` given relatively (`Unaddressable`) — or open its startup lock — a sandbox forbidding its directory — is wired in process instead (`DaemonClient::can_start_daemon`); invariant 3 is what makes that the same answer rather than a degraded one.

The daemon serves every project of its installation, so it runs in the installation's runtime directory, never in a project's: a working directory marks a tree as in use. Nothing it does may depend on its own working directory. A relative path resolves where a direct run resolves it, against the request's project root (a relative `PATH` entry); the paths every caller shares with the daemon must be absolute for one to run.

## Wire types are serialization-stable

Types in `wire.rs` (Symbol, Location, Diagnostic, …) are an external protocol. They:

- Use `String` for paths, not `PathBuf` — guarantees UTF-8 on the wire across platforms.
- Convert to/from `models::*` types via explicit `From` impls. Don't shortcut by serializing `models::*` directly.
- The ping handshake guarantees client and daemon were built from the same sources, features, target, and profile (`protocol::BUILD_ID`), so wire types evolve freely within a release — no cross-version compatibility shims. Optional fields still use `#[serde(skip_serializing_if = "Option::is_none")]` to keep payloads lean.

## Server cleanup

A daemon leaves its socket and pid files behind: the next daemon settles them when it claims the installation's path (`claim_socket`), under `daemon-<id>.bind.lock` and only after confirming nobody answers. Removing them at shutdown would give a slow teardown the power to unlink a successor's live socket, so liveness is always a connection attempt — never a path lookup — on both sides of the wire.

A daemon that has held no project and had no connection open for `idle_timeout` shuts down through `DaemonServer::shutdown`, and the client starts one on demand; work the daemon does outside a project or a connection does not keep it alive.

A daemon told to stop takes no new work at once: it releases its socket, reads no further request on an accepted connection, and answers one still waiting for a permit `DaemonStopping`. It then drains (`DaemonServer::drain`): its language servers keep running until every request it was working on is answered, so none is cancelled by their shutdown or cut off by the process exiting mid-answer. The drain lasts at most `DRAIN_LIMIT` (10 s); a request still running then is answered `DaemonStopping`, and those answers get `REFUSAL_GRACE` (1 s) to be written. Only then does the daemon checkpoint its stores, stop its servers and exit, so a stopped or replaced daemon can outlive its socket by that bound while its successor serves. The client maps `DaemonStopping`, like a lost connection, to `lsp_unavailable` with the retry hint, and still reads an answer cut off by the stream's end as a lost connection.

Project restart unloads only the addressed project's retained services and preserves its on-disk index. Active requests and outstanding server starts prevent restart or idle eviction. Release the projects-map lock before awaiting server shutdown so other projects remain serviceable.

## One command, one daemon

A command's requests that answer for it (`methods::binds_to_instance`) are answered by the daemon instance that answered its first: a CLI invocation and an MCP tool call each run in `client::one_command`. Every answer names its daemon's instance, which no other daemon process shares; a bound request carries the instance it is bound to, and any other daemon refuses it as `DaemonReplaced` before doing any of its work, which the client also makes of an answer from another instance. A bound command whose daemon no longer listens fails the same way and starts none, since a new one could only be another. A command spanning a restart would otherwise go on against the successor, whose sessions hold none of what its earlier answers rested on, and on a daemon several sessions share one session's restart would answer another's command with that mix. Lifecycle requests and the notes of files written address whichever daemon is serving. An unbound command's first request keeps the retry a request no daemon accepted gets.

Every request also carries its command's age (`request_age_ms`), and the daemon serves it in an `infra::request` scope begun that long before it arrived, returning the servers its queries noted as started since (`cold_servers`); the client notes them once the answer is bound. A server started during the command, by any of its requests, is then the same fact in both modes.

## Request timeouts

LSP operations use the project's retained `lsp.timeout_secs` budget. CLI/MCP establish a shared deadline, and daemon requests carry remaining milliseconds. Admission waits and dispatch preparation consume that allowance; expired queued requests do not start project work. The server also clamps it to the project's retained configuration. Language and operation multipliers do not extend it. Timeout errors retain their typed wire identity. An index build spends its caller's budget the same way where it carries one — a symbol search's — and none otherwise; it runs on its own task either way, so the budget bounds the answer, never the build. Other non-LSP store/lifecycle operations keep their separate bounds.

A caller can stop waiting for a pooled server start without cancelling other waiters. The start has its own bounded lifetime and must release its pool claim on failure. Pending LSP requests send cancellation and remove pending state when dropped.
