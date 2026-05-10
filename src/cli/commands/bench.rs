use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::detect_exported;
use crate::cli::errors::OutputError;
use crate::cli::utils::{TestMatcher, classify_refs};
use crate::error::LspError;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::{Language, Location};
use crate::services::pack::{PackConfig, build_pack};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Measure end-to-end latency for the LSP-less hot paths so a CI \n\
                       run can detect regressions on your monorepo. Output is JSON, \n\
                       so it composes with `jq` and other agent tooling.\n\
                       \n\
                       Example: symora bench --iterations 100 --format compact"
)]
pub struct BenchArgs {
    /// How many iterations per benchmark. Higher = lower noise.
    #[arg(long, default_value_t = crate::constants::defaults::BENCH_DEFAULT_ITERATIONS)]
    pub iterations: usize,

    /// Skip the pack benchmark (it's the most expensive one).
    #[arg(long)]
    pub no_pack: bool,
}

#[derive(Debug, Serialize)]
struct BenchOutput {
    iterations: usize,
    rust_version: &'static str,
    project_root: String,
    results: Vec<BenchResult>,
}

#[derive(Debug, Serialize)]
struct BenchResult {
    name: &'static str,
    workload: String,
    samples: usize,
    min_us: u64,
    p50_us: u64,
    p95_us: u64,
    max_us: u64,
    mean_us: u64,
}

pub async fn execute(args: BenchArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let root = ctx.root().to_path_buf();
    let iterations = args.iterations.max(1);

    let mut results = Vec::new();

    if !args.no_pack {
        let filter = FileFilter::with_gitignore(&root);
        // Disable the on-disk cache so every iteration measures the cold
        // path. With cache enabled the first call writes pack-cache.db and
        // every subsequent call hits a warm cache, biasing the percentile
        // distribution toward best-case latency.
        let cfg = PackConfig {
            use_cache: false,
            ..PackConfig::default()
        };
        results.push(measure(
            "pack_build_cold",
            "build_pack(tokens=4000, focus=None, cache=off) on this repo",
            iterations,
            || {
                build_pack(&root, 4000, None, &filter, &cfg).expect("pack");
            },
        ));
    }

    results.push(measure(
        "classify_refs_1k",
        "classify_refs over 1000 synthetic Locations",
        iterations,
        bench_classify_refs(&root),
    ));

    results.push(measure(
        "error_classify_lsp",
        "OutputError::from(LspError::ServerError) over a 4-variant fixture",
        iterations,
        bench_error_classify(),
    ));

    results.push(measure(
        "detect_exported_rust",
        "detect_exported across Rust + Go + Python + TS",
        iterations,
        bench_detect_exported(),
    ));

    ctx.print_success(BenchOutput {
        iterations,
        rust_version: env!("CARGO_PKG_RUST_VERSION"),
        project_root: root.display().to_string(),
        results,
    });

    Ok(())
}

fn measure<F>(name: &'static str, workload: &str, iterations: usize, mut f: F) -> BenchResult
where
    F: FnMut(),
{
    // Warm-up: 3 iterations not measured (CPU caches, allocator state).
    // `black_box` on the closure forces the optimizer to treat each call
    // as opaque so it can't constant-fold a no-op iteration.
    for _ in 0..3.min(iterations) {
        f();
        std::hint::black_box(());
    }

    let mut samples: Vec<Duration> = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        std::hint::black_box(());
        samples.push(start.elapsed());
    }

    samples.sort();
    let min = samples.first().copied().unwrap_or_default();
    let max = samples.last().copied().unwrap_or_default();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    let mean: Duration = samples.iter().sum::<Duration>() / iterations as u32;

    BenchResult {
        name,
        workload: workload.to_string(),
        samples: iterations,
        min_us: min.as_micros() as u64,
        p50_us: p50.as_micros() as u64,
        p95_us: p95.as_micros() as u64,
        max_us: max.as_micros() as u64,
        mean_us: mean.as_micros() as u64,
    }
}

fn bench_classify_refs(root: &std::path::Path) -> impl FnMut() {
    let matcher = TestMatcher::new();
    let refs: Vec<Location> = (0..1000)
        .map(|i| {
            let path = if i % 5 == 0 {
                root.join(format!("tests/test_{i}.rs"))
            } else {
                root.join(format!("src/file_{}/sub_{}.rs", i % 7, i))
            };
            Location::point(path, (i % 1000) as u32 + 1, 1)
        })
        .collect();
    let root = root.to_path_buf();
    move || {
        let c = classify_refs(&refs, &root, None, None, &matcher);
        std::hint::black_box(c.total);
    }
}

fn bench_error_classify() -> impl FnMut() {
    move || {
        for code in [-32601, -32603, -32801, -32000] {
            let e = LspError::ServerError {
                code,
                message: match code {
                    -32601 => "method textDocument/typeDefinition not found".into(),
                    -32603 => "Invalid position: line 9999 out of bounds".into(),
                    -32801 => "content modified".into(),
                    _ => "opaque internal".into(),
                },
            };
            std::hint::black_box(OutputError::from(e));
        }
    }
}

fn bench_detect_exported() -> impl FnMut() {
    move || {
        std::hint::black_box(detect_exported("pub fn process()", Language::Rust));
        std::hint::black_box(detect_exported("func (h *Handler) Process()", Language::Go));
        std::hint::black_box(detect_exported("def process():", Language::Python));
        std::hint::black_box(detect_exported(
            "export function process()",
            Language::TypeScript,
        ));
    }
}
