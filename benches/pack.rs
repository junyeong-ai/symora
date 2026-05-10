//! Microbenchmarks for the `symora pack` engine.
//!
//! Fixture-based: each iteration runs against a deterministic in-memory
//! tempdir so results don't drift with the host repo. Run via:
//!     cargo bench --bench pack

use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use symora::infra::file_filter::FileFilter;
use symora::services::pack::{PackConfig, build_pack};

fn write_rust_file(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

fn small_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_rust_file(
        root,
        "a.rs",
        "use crate::b;\nuse crate::c;\npub fn a() {}\n",
    );
    write_rust_file(root, "b.rs", "use crate::c;\npub fn b() {}\n");
    write_rust_file(root, "c.rs", "pub fn c() {}\npub struct C {}\n");
    write_rust_file(
        root,
        "d.rs",
        "use crate::a;\npub fn d() {}\npub fn d2() -> i32 { 0 }\n",
    );
    dir
}

fn medium_fixture(file_count: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..file_count {
        let mut body = String::new();
        // Each file imports its three predecessors, creating a small dense graph.
        for j in i.saturating_sub(3)..i {
            body.push_str(&format!("use crate::file_{j};\n"));
        }
        for k in 0..5 {
            body.push_str(&format!("pub fn fn_{i}_{k}(x: i32) -> i32 {{ x + {k} }}\n"));
        }
        write_rust_file(root, &format!("file_{i}.rs"), &body);
    }
    dir
}

fn bench_pack(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack");

    let small = small_fixture();
    let small_root = small.path().to_path_buf();
    let small_filter = FileFilter::with_gitignore(&small_root);
    let cfg = PackConfig::default();

    group.bench_function("small_4_files", |b| {
        b.iter(|| {
            black_box(build_pack(&small_root, 4000, None, &small_filter, &cfg).unwrap());
        });
    });

    let medium = medium_fixture(50);
    let medium_root = medium.path().to_path_buf();
    let medium_filter = FileFilter::with_gitignore(&medium_root);

    group.bench_function("medium_50_files", |b| {
        b.iter(|| {
            black_box(build_pack(&medium_root, 4000, None, &medium_filter, &cfg).unwrap());
        });
    });

    group.bench_function("medium_50_files_focused", |b| {
        b.iter(|| {
            black_box(
                build_pack(&medium_root, 2000, Some("file_25"), &medium_filter, &cfg).unwrap(),
            );
        });
    });

    group.finish();
}

criterion_group!(benches, bench_pack);
criterion_main!(benches);
