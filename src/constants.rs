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

    /// Hard cap on file size pack will read. Keeps generated artefacts and
    /// vendored bundles from dominating the import graph.
    pub const PACK_MAX_FILE_BYTES: u64 = 256 * 1024;

    /// Maximum affected files surfaced by `impact`. Beyond this the
    /// command truncates and reports a count.
    pub const IMPACT_FILES_LIMIT: usize = 50;

    /// Default transitive depth for impact's blast radius. Each extra hop
    /// costs an LSP round-trip per caller; 1 is fast survey, 2-3 is for
    /// ranking blast radius.
    pub const IMPACT_DEFAULT_DEPTH: u32 = 1;

    /// Hard cap on impact's BFS depth, regardless of user input.
    pub const IMPACT_MAX_DEPTH: u32 = 3;

    /// Cap on callers fanned out per BFS frontier node. Stops a single
    /// hot spot (e.g., logger) from exploding the graph.
    pub const BLAST_RADIUS_MAX_CALLERS_PER_NODE: usize = 100;

    /// Concurrent file fan-out for `Store::index` (SQLite write contention
    /// + open file descriptors trade off here).
    pub const STORE_INDEX_CONCURRENCY: usize = 16;

    /// Hard cap on simultaneously running language servers.
    /// Keeps memory-bound monorepos from exhausting RAM / FDs.
    pub const LSP_MAX_CONCURRENT_SERVERS: usize = 12;

    /// Iteration cap for the bench command warm-loop.
    pub const BENCH_DEFAULT_ITERATIONS: usize = 50;

    /// Source-line window per chunk in `search semantic`. 30 ≈ a typical
    /// function body — small enough to embed precisely, large enough to
    /// carry context.
    pub const SEMANTIC_CHUNK_LINES: usize = 30;

    /// MCP HTTP transport default port.
    pub const MCP_HTTP_DEFAULT_PORT: u16 = 7700;
}

/// Environment variables Symora reads from the process environment.
/// Centralised so a typo is a compile error, not a silent miss.
pub mod env {
    /// Set to `"1"` to disable daemon usage for one invocation.
    pub const NO_DAEMON: &str = "SYMORA_NO_DAEMON";

    /// Internal: the workspace dispatcher sets this in spawned children
    /// to force compact JSON for the parent envelope.
    pub const FORMAT_OVERRIDE: &str = "SYMORA_FORMAT_OVERRIDE";
}
