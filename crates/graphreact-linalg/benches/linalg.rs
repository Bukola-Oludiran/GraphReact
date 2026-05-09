//! M1 acceptance benchmark — measures throughput for the five linalg
//! kernels on shapes representative of GraphReact's spectral workloads.
//!
//! Compare against the WASM reference numbers (TS+wasm-bindgen build of
//! the same algorithms): the M1 acceptance criterion is **≥3× faster**
//! per operation, on the assumption that the largest single saving is
//! the elimination of the `Float64Array` marshalling round-trip.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use faer::Mat;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use graphreact_linalg::{eigh, gemm, normalize_rows, solve, svd};

fn random_matrix(rows: usize, cols: usize, seed: u64) -> Mat<f64> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut m = Mat::<f64>::zeros(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            m[(r, c)] = rng.gen_range(-1.0..1.0);
        }
    }
    m
}

fn random_symmetric(n: usize, seed: u64) -> Mat<f64> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut m = Mat::<f64>::zeros(n, n);
    for r in 0..n {
        for c in r..n {
            let v = rng.gen_range(-1.0..1.0);
            m[(r, c)] = v;
            m[(c, r)] = v;
        }
    }
    m
}

fn bench_svd(c: &mut Criterion) {
    let mut group = c.benchmark_group("svd");
    for &(rows, cols, k) in &[(64usize, 64, 8), (128, 128, 16), (256, 128, 16)] {
        let a = random_matrix(rows, cols, 0xA1);
        group.throughput(Throughput::Elements((rows * cols) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{rows}x{cols}_k{k}")),
            &(rows, cols, k),
            |bencher, _| {
                bencher.iter(|| {
                    let d = svd(black_box(a.as_ref()), black_box(k)).expect("svd");
                    black_box(d);
                });
            },
        );
    }
    group.finish();
}

fn bench_eigh(c: &mut Criterion) {
    let mut group = c.benchmark_group("eigh");
    for &(n, k) in &[(64usize, 8), (128, 16), (256, 16)] {
        let a = random_symmetric(n, 0xE1);
        group.throughput(Throughput::Elements((n * n) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n}x{n}_k{k}")),
            &(n, k),
            |bencher, _| {
                bencher.iter(|| {
                    let d = eigh(black_box(a.as_ref()), black_box(k)).expect("eigh");
                    black_box(d);
                });
            },
        );
    }
    group.finish();
}

fn bench_gemm(c: &mut Criterion) {
    let mut group = c.benchmark_group("gemm");
    for &(m, k, n) in &[(64usize, 64, 64), (128, 128, 128), (256, 256, 256)] {
        let a = random_matrix(m, k, 0x6E);
        let b = random_matrix(k, n, 0x6F);
        group.throughput(Throughput::Elements((m * k * n) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{m}x{k}_x_{k}x{n}")),
            &(m, k, n),
            |bencher, _| {
                bencher.iter(|| {
                    let c = gemm(black_box(a.as_ref()), black_box(b.as_ref())).expect("gemm");
                    black_box(c);
                });
            },
        );
    }
    group.finish();
}

fn bench_solve(c: &mut Criterion) {
    let mut group = c.benchmark_group("solve");
    for &n in &[64usize, 128, 256] {
        // Build a diagonally-dominant (well-conditioned) `n×n` system.
        let mut a = random_matrix(n, n, 0x50);
        #[allow(clippy::cast_precision_loss)]
        let bump = n as f64;
        for i in 0..n {
            a[(i, i)] += bump;
        }
        let b = random_matrix(n, 1, 0x51);
        group.throughput(Throughput::Elements((n * n) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |bencher, _| {
            bencher.iter(|| {
                let x = solve(black_box(a.as_ref()), black_box(b.as_ref())).expect("solve");
                black_box(x);
            });
        });
    }
    group.finish();
}

fn bench_normalize(c: &mut Criterion) {
    let mut group = c.benchmark_group("normalize_rows");
    for &(rows, cols) in &[(1_000usize, 16), (10_000, 32), (50_000, 64)] {
        let a = random_matrix(rows, cols, 0xAB);
        group.throughput(Throughput::Elements((rows * cols) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{rows}x{cols}")),
            &(rows, cols),
            |bencher, _| {
                bencher.iter(|| {
                    let n = normalize_rows(black_box(a.as_ref())).expect("normalize");
                    black_box(n);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_gemm,
    bench_normalize,
    bench_solve,
    bench_svd,
    bench_eigh,
);
criterion_main!(benches);
