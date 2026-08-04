//! Microbenchmarks for the blast-radius helpers and reference classification.
//! Mock-LSP-driven `compute()` benches are intentionally omitted: the
//! tokio-rusqlite + LSP mock setup would dominate measurement noise. We
//! benchmark the pure analysis surface instead.

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use symora::cli::utils::RefsClassification;
use symora::models::symbol::Location;
use symora::services::TestScope;

fn make_locations(count: usize, root: &std::path::Path) -> Vec<Location> {
    (0..count)
        .map(|i| {
            let name = if i % 5 == 0 {
                format!("tests/test_{i}.rs")
            } else {
                format!("src/file_{}/sub_{}.rs", i % 7, i)
            };
            Location::point(root.join(name), (i % 1000) as u32 + 1, 1)
        })
        .collect()
}

fn bench_classify_refs(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify_refs");
    let root = PathBuf::from("/repo");
    let scope = TestScope::new();

    for size in [10usize, 100, 1000, 5000] {
        let refs = make_locations(size, &root);
        group.bench_function(format!("size_{size}"), |b| {
            b.iter(|| {
                let classified = RefsClassification::of(black_box(&refs), &scope);
                black_box(classified.total);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_classify_refs);
criterion_main!(benches);
