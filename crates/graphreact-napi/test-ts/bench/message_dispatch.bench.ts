// NAPI parity benchmark — paired with `bench_message_dispatch.rs` in
// graphreact-core. Same workload: inject `MESSAGES` external `tick`
// messages at a producer; producer forwards each to a consumer
// (one-hop). Total observed deliveries = `MESSAGES * 2`.
//
// Run via: `npm run bench` (in this dir, or via npx vitest run --config ...).
// Output line is parsed by `cargo xtask bench` for the M6 ≤2× gate.

import { describe, expect, it } from 'vitest';
import { Graph, Scheduler, emit, useMessage } from '../native.js';

const MESSAGES = 250_000;
const WARMUP_MESSAGES = 10_000;

describe('NAPI message-dispatch bench', () => {
  it(`forwards ${MESSAGES} messages and reports throughput`, () => {
    const graph = new Graph();
    graph.addNode('producer');
    graph.addNode('consumer');
    graph.addEdge('producer', 'consumer');
    const scheduler = new Scheduler(graph);
    scheduler.setMaxProcessDepth(MESSAGES + 1024);

    let producerSeen = 0;
    let consumerSeen = 0;

    scheduler.registerNode('producer', {
      name: 'Producer',
      component: () => {
        useMessage('tick', () => {
          producerSeen += 1;
          emit('tick', null, { to: ['consumer'], ttl: 8 });
        });
        return { accepts: ['tick'], emits: ['tick'] };
      },
    });
    scheduler.registerNode('consumer', {
      name: 'Consumer',
      component: () => {
        useMessage('tick', () => {
          consumerSeen += 1;
        });
        return { accepts: ['tick'], emits: [] };
      },
    });
    scheduler.start();

    // Warmup so V8 JIT / inline caches stabilize before timing.
    for (let i = 0; i < WARMUP_MESSAGES; i++) {
      scheduler.send('producer', 'tick', null);
    }
    const warmupP = producerSeen;
    const warmupC = consumerSeen;

    const begin = process.hrtime.bigint();
    for (let i = 0; i < MESSAGES; i++) {
      scheduler.send('producer', 'tick', null);
    }
    // `send()` already drives a process_batch on each call. The bench
    // measures the cumulative cost of that loop.
    const elapsedNs = Number(process.hrtime.bigint() - begin);
    const total = producerSeen - warmupP + (consumerSeen - warmupC);
    const perMsgNs = total > 0 ? elapsedNs / total : 0;
    const throughput = elapsedNs > 0 ? (total / elapsedNs) * 1e9 : Infinity;

    // Format chosen to match the Rust bench so the xtask runner can grep
    // both outputs identically.
    console.log(
      `[napi bench] injected=${MESSAGES} delivered=${total} ` +
        `elapsed=${(elapsedNs / 1e6).toFixed(3)}ms ` +
        `per_msg=${Math.round(perMsgNs)}ns ` +
        `throughput=${Math.round(throughput).toLocaleString()} msg/sec`,
    );

    expect(producerSeen).toBe(MESSAGES + WARMUP_MESSAGES);
    expect(consumerSeen).toBe(MESSAGES + WARMUP_MESSAGES);

    scheduler.stop();
  });
});
