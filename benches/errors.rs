//! Microbenchmarks for the error-classification path. These are short
//! enough that we want low-noise measurements before tuning the regex /
//! match arms in `OutputError::From<LspError>`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use symora::cli::errors::OutputError;
use symora::error::LspError;

fn bench_lsp_error_classification(c: &mut Criterion) {
    let mut group = c.benchmark_group("error_classify");

    group.bench_function("server_error_not_found_msg", |b| {
        b.iter(|| {
            let e = LspError::ServerError {
                code: -32603,
                message: "file not found at path X".into(),
            };
            black_box(OutputError::from(e));
        });
    });

    group.bench_function("server_error_method_not_found_code", |b| {
        b.iter(|| {
            let e = LspError::ServerError {
                code: -32601,
                message: "method textDocument/typeDefinition not found".into(),
            };
            black_box(OutputError::from(e));
        });
    });

    group.bench_function("server_error_unknown_internal", |b| {
        b.iter(|| {
            let e = LspError::ServerError {
                code: -32000,
                message: "some opaque internal failure happened".into(),
            };
            black_box(OutputError::from(e));
        });
    });

    group.bench_function("feature_not_supported_passthrough", |b| {
        b.iter(|| {
            let e = LspError::FeatureNotSupported {
                language: symora::models::symbol::Language::Rust,
                server: "rust-analyzer".into(),
                feature: "callHierarchy".into(),
                suggestion: "Use 'symora refs'".into(),
            };
            black_box(OutputError::from(e));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_lsp_error_classification);
criterion_main!(benches);
