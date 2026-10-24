//! Crate-wide tunable constants.
//!
//! These are the conservative defaults Symora ships with. Each value has
//! a one-line rationale so future tuning can refer back to the original
//! intent. Per-project overrides live in `.symora/config.toml`.

/// Tuning knobs surfaced as defaults to the user.
pub mod defaults {
    /// Token budget for `symora pack`. ~4000 ≈ 1000 LOC of average code,
    /// which fits comfortably alongside an agent's working context.
    pub const PACK_TOKENS: usize = 4000;

    /// Cap on top-level symbols per file in a pack response. Prevents a
    /// single 10k-LOC file from drowning out neighbouring context.
    pub const PACK_SYMBOLS_PER_FILE: usize = 12;

    /// Token budget for `context --with-bodies` section bodies. Half of
    /// PACK_TOKENS: a bodies-bearing context response rides alongside the
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

    /// Symbol depth and related-file count `map file` shows: one level of
    /// members, and enough neighbours to find a file's tests and siblings.
    pub const MAP_FILE_DEPTH: u32 = 1;
    pub const MAP_FILE_RELATED_LIMIT: usize = 8;

    /// Cap on neighbours (callers or callees) fanned out per BFS frontier node,
    /// shared by the incoming (impact/blast-radius) and outgoing (callees)
    /// walks. Stops a single hot spot (e.g., a logger) from exploding the graph.
    pub const CALL_GRAPH_MAX_NEIGHBORS_PER_NODE: usize = 100;

    /// Concurrent file fan-out for `Store::index` (SQLite write contention
    /// + open file descriptors trade off here).
    pub const STORE_INDEX_CONCURRENCY: usize = 16;

    /// Reference queries `symbols --refs` keeps in flight against one server.
    /// Enough to overlap round-trips on a file of methods without queueing a
    /// file's worth of work inside a server that answers one at a time.
    pub const SYMBOL_REFS_CONCURRENCY: usize = 8;

    /// Hard cap on simultaneously running language servers.
    /// Keeps memory-bound monorepos from exhausting RAM / FDs.
    pub const LSP_MAX_CONCURRENT_SERVERS: usize = 12;

    /// MCP HTTP transport default port.
    pub const MCP_HTTP_DEFAULT_PORT: u16 = 7700;
}

/// Environment variables Symora reads from the process environment.
/// Centralised so a typo is a compile error, not a silent miss.
pub mod env {
    /// Set to `"1"` to disable daemon usage for one invocation.
    pub const NO_DAEMON: &str = "SYMORA_NO_DAEMON";
}
