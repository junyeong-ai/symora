//! Crate-wide tunable constants.
//!
//! These are the conservative defaults Symora ships with. Each value has
//! a one-line rationale so future tuning can refer back to the original
//! intent. Per-project overrides live in `.symora/config.toml`.

/// Tuning knobs surfaced as defaults to the user.
pub mod defaults {
    /// Token budget for `context --with-bodies` section bodies. A
    /// bodies-bearing context response rides alongside the
    /// target/refs/section data the agent already pays for, and ~2000
    /// tokens ≈ 500 LOC — comfortably a dozen typical callee bodies at
    /// depth 1.
    pub const CONTEXT_BODY_TOKENS: usize = 2000;

    /// Maximum affected files surfaced by `impact`. Beyond this the
    /// command truncates and reports a count.
    pub const IMPACT_FILES_LIMIT: usize = 50;

    /// Default transitive depth for impact's blast radius. Each extra hop
    /// costs an LSP round-trip per caller; 1 is fast survey, 2-3 is for
    /// ranking blast radius.
    pub const IMPACT_DEFAULT_DEPTH: u32 = 1;

    /// Hard cap on the depth of any call-graph walk — impact's callers,
    /// callees' reachable set, and the call path — regardless of user input.
    pub const CALL_GRAPH_MAX_DEPTH: u32 = 3;

    /// Changed declarations `diff-impact` analyses before it stops and says
    /// so. Each costs a reference query, and another with `--callers`; a
    /// review-sized diff stays under it.
    pub const DIFF_IMPACT_MAX_SYMBOLS: usize = 50;

    /// Top-level directories and entrypoints `map summary` lists.
    pub const MAP_SUMMARY_LIMIT: usize = 10;

    /// Symbol depth `map file` shows: one level of members.
    pub const MAP_FILE_DEPTH: u32 = 1;

    /// Tests `map file` lists: enough for a module most of a suite imports
    /// to show where its tests are without the list filling the response.
    pub const MAP_FILE_TESTS_LIMIT: usize = 10;

    /// Cap on neighbours (callers or callees) fanned out per BFS frontier node,
    /// shared by the incoming (impact/blast-radius) and outgoing (callees)
    /// walks. Stops a single hot spot (e.g., a logger) from exploding the graph.
    pub const CALL_GRAPH_MAX_NEIGHBORS_PER_NODE: usize = 100;

    /// Concurrent file fan-out for `Store::index` (SQLite write contention
    /// + open file descriptors trade off here).
    pub const STORE_INDEX_CONCURRENCY: usize = 16;

    /// Reference counts `symbols --refs` and `diff-impact` keep in flight.
    /// Enough to overlap round-trips on a file of methods without queueing a
    /// file's worth of work inside a server that answers one at a time.
    pub const REFERENCE_COUNT_CONCURRENCY: usize = 8;

    /// Hard cap on simultaneously running language servers.
    /// Keeps memory-bound monorepos from exhausting RAM / FDs.
    pub const LSP_MAX_CONCURRENT_SERVERS: usize = 12;

    /// How long a stopping language server has to answer `shutdown` before
    /// it is killed without being sent `exit`: long enough for one that is
    /// alive, short enough that one that hangs does not hold a command's exit.
    pub const LSP_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// How long a language server has to exit once sent `exit` before it is
    /// killed with its group.
    pub const LSP_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// MCP HTTP transport default port.
    pub const MCP_HTTP_DEFAULT_PORT: u16 = 7700;
}

/// Environment variables Symora reads from the process environment.
/// Centralised so a typo is a compile error, not a silent miss.
pub mod env {
    /// Set to `"1"` to disable daemon usage for one invocation.
    pub const NO_DAEMON: &str = "SYMORA_NO_DAEMON";
}
