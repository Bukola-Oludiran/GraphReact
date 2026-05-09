// Wall-clock benchmark of the legacy WASM linalg crate, run from Node.
//
// Loads the existing wasm-bindgen `--target web` artifact at
// `graphreact/src/core/algorithms/linalg/graphreact_linalg_bg.wasm`,
// initializes it with a precompiled Module (so we don't need fetch()
// or import.meta.url support in Node), and times the same operations
// the criterion bench measures on the Rust side. Numbers are
// per-iteration averages, in microseconds. Compare directly against
// criterion's reported `time:` row.
//
// Usage: node wasm_baseline.mjs

import { readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const linalgDir = resolve(
  here,
  "../../../../graphreact/src/core/algorithms/linalg",
);

const wasmBytes = readFileSync(resolve(linalgDir, "graphreact_linalg_bg.wasm"));
const wasmModule = await WebAssembly.compile(wasmBytes);

const glue = await import(resolve(linalgDir, "graphreact_linalg.js"));
await glue.default(wasmModule);

// Deterministic PRNG matching the Rust bench seeds in spirit (we don't
// need bitwise parity — just stable, comparable distributions).
function mulberry32(seed) {
  return function () {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = seed;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function randomMatrix(rows, cols, seed) {
  const rng = mulberry32(seed);
  const data = new Float64Array(rows * cols);
  for (let i = 0; i < data.length; i++) data[i] = rng() * 2 - 1;
  return data;
}

function randomSymmetric(n, seed) {
  const rng = mulberry32(seed);
  const data = new Float64Array(n * n);
  for (let r = 0; r < n; r++) {
    for (let c = r; c < n; c++) {
      const v = rng() * 2 - 1;
      data[r * n + c] = v;
      data[c * n + r] = v;
    }
  }
  return data;
}

function diagDominant(n, seed) {
  const a = randomMatrix(n, n, seed);
  for (let i = 0; i < n; i++) a[i * n + i] += n;
  return a;
}

function timeIt(label, iters, fn) {
  // Warm-up.
  for (let i = 0; i < Math.max(3, Math.floor(iters / 10)); i++) fn();
  const start = process.hrtime.bigint();
  for (let i = 0; i < iters; i++) fn();
  const end = process.hrtime.bigint();
  const totalNs = Number(end - start);
  const perOpUs = totalNs / iters / 1000;
  console.log(`${label.padEnd(28)} ${perOpUs.toFixed(3).padStart(10)} µs/op  (n=${iters})`);
  return perOpUs;
}

const out = {};

// gemm shapes match the criterion bench: 64×64×64, 128×128×128, 256×256×256.
{
  for (const sz of [64, 128, 256]) {
    const a = randomMatrix(sz, sz, 0x6e);
    const b = randomMatrix(sz, sz, 0x6f);
    const iters = sz <= 64 ? 5000 : sz <= 128 ? 2000 : 500;
    out[`gemm/${sz}x${sz}_x_${sz}x${sz}`] = timeIt(
      `gemm/${sz}x${sz}`,
      iters,
      () => {
        const c = glue.gemm(a, b, sz, sz, sz);
        // Touch the result so JIT can't elide it.
        if (c.length === 0) throw new Error();
      },
    );
  }
}

// normalize_rows shapes: 1000×16, 10000×32, 50000×64.
{
  for (const [rows, cols] of [
    [1000, 16],
    [10000, 32],
    [50000, 64],
  ]) {
    const a = randomMatrix(rows, cols, 0xab);
    const iters = rows <= 1000 ? 5000 : rows <= 10000 ? 1000 : 100;
    out[`normalize_rows/${rows}x${cols}`] = timeIt(
      `normalize/${rows}x${cols}`,
      iters,
      () => {
        const r = glue.normalize_rows(a, rows, cols);
        if (r.length === 0) throw new Error();
      },
    );
  }
}

// solve shapes: 64, 128, 256 (well-conditioned, n×1 RHS).
{
  for (const n of [64, 128, 256]) {
    const a = diagDominant(n, 0x50);
    const b = randomMatrix(n, 1, 0x51);
    const iters = n <= 64 ? 5000 : n <= 128 ? 2000 : 500;
    out[`solve/${n}`] = timeIt(`solve/${n}`, iters, () => {
      const x = glue.solve(a, b, n);
      if (x.length === 0) throw new Error();
    });
  }
}

// svd shapes: 64×64 k=8, 128×128 k=16, 256×128 k=16.
{
  for (const [rows, cols, k] of [
    [64, 64, 8],
    [128, 128, 16],
    [256, 128, 16],
  ]) {
    const a = randomMatrix(rows, cols, 0xa1);
    const iters = rows * cols <= 64 * 64 ? 500 : rows * cols <= 128 * 128 ? 100 : 50;
    out[`svd/${rows}x${cols}_k${k}`] = timeIt(
      `svd/${rows}x${cols}_k${k}`,
      iters,
      () => {
        const r = glue.svd(a, rows, cols, k);
        if (r.k !== k) throw new Error();
      },
    );
  }
}

// eigh shapes: 64×64 k=8, 128×128 k=16, 256×256 k=16.
{
  for (const [n, k] of [
    [64, 8],
    [128, 16],
    [256, 16],
  ]) {
    const a = randomSymmetric(n, 0xe1);
    const iters = n <= 64 ? 500 : n <= 128 ? 200 : 50;
    out[`eigh/${n}x${n}_k${k}`] = timeIt(`eigh/${n}x${n}_k${k}`, iters, () => {
      const r = glue.eigh(a, n, k);
      if (r.k !== k) throw new Error();
    });
  }
}

console.log("\nJSON:", JSON.stringify(out));
