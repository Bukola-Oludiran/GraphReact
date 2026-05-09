// M6.6 — Event source contract tests (Rust-native sources).
//
// Sources are implemented in Rust (timer/delay in core; http/watch in
// the napi crate via tokio::net + notify). JS receives opaque
// `EventSource` handles; `useSource(handle, deps)` registers them.
// These tests pin the externally-observable behavior: timer ticks fire
// at cadence, delay fires once, HTTP server emits requests, file
// watch surfaces fs events, and stop cleanly tears each one down.

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  delaySource,
  httpSource,
  timerSource,
  useMessage,
  useSource,
  watchSource,
} from '../native.js';

const tick = (ms = 0): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

describe('Sources: built-in (timer / delay)', () => {
  let graph: Graph;
  let scheduler: Scheduler;

  beforeEach(() => {
    graph = new Graph();
    graph.addNode('n');
    scheduler = new Scheduler(graph);
  });

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('timerSource emits ticks at the configured interval', async () => {
    const ticks: number[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useMessage('tick', () => {
          ticks.push(Date.now());
        });
        useSource(timerSource(15), []);
        return { accepts: ['tick'] };
      },
    });
    await scheduler.startAsync();
    await tick(80);
    expect(ticks.length).toBeGreaterThanOrEqual(2);
    scheduler.stop();
    const stoppedAt = ticks.length;
    await tick(40);
    expect(ticks.length).toBe(stoppedAt);
  });

  it('delaySource emits a single timeout and self-completes', async () => {
    let timeouts = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useMessage('timeout', () => {
          timeouts += 1;
        });
        useSource(delaySource(15), []);
        return { accepts: ['timeout'] };
      },
    });
    await scheduler.startAsync();
    await tick(60);
    expect(timeouts).toBe(1);
  });

  it('exposes the source id getter', () => {
    const t = timerSource(100);
    expect(t.id).toMatch(/^timer-\d+$/);
    const d = delaySource(100);
    expect(d.id).toMatch(/^delay-\d+$/);
  });
});

describe('Sources: httpSource (Rust-native)', () => {
  let graph: Graph;
  let scheduler: Scheduler;
  // High fixed port. Tests share a process so collisions across `it`
  // blocks within this describe shouldn't happen — and we close
  // between tests via `scheduler.stop()`.
  const PORT = 39487;

  beforeEach(() => {
    graph = new Graph();
    graph.addNode('n');
    scheduler = new Scheduler(graph);
  });

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
    // Allow the OS a moment to release the listening port between
    // tests so consecutive runs don't EADDRINUSE.
    return tick(40);
  });

  it('emits a request event for each incoming HTTP request', async () => {
    const requests: { method: string; url: string; body: string | null }[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useMessage('request', (msg) => {
          const p = msg.payload as {
            method: string;
            url: string;
            body: string | null;
          };
          requests.push({ method: p.method, url: p.url, body: p.body });
        });
        useSource(httpSource(PORT), []);
        return { accepts: ['request'] };
      },
    });
    await scheduler.startAsync();
    // The Rust HTTP source spawns a tokio task that binds the port —
    // give it a beat to start listening before we hit it.
    await tick(40);

    const res = await fetch(`http://127.0.0.1:${PORT}/foo`, {
      method: 'POST',
      body: 'hello',
    });
    expect(res.ok).toBe(true);
    const responseJson = (await res.json()) as { status: string };
    expect(responseJson.status).toBe('ok');

    // Push fires through the cross-thread inbox; allow the scheduler
    // a couple of microtask flushes to deliver.
    await tick(60);
    expect(requests).toHaveLength(1);
    expect(requests[0].method).toBe('POST');
    expect(requests[0].url).toBe('/foo');
    expect(requests[0].body).toBe('hello');
  });
});

describe('Sources: watchSource (Rust-native)', () => {
  let graph: Graph;
  let scheduler: Scheduler;
  let dir: string;

  beforeEach(() => {
    graph = new Graph();
    graph.addNode('n');
    scheduler = new Scheduler(graph);
    dir = mkdtempSync(join(tmpdir(), 'graphreact-watch-'));
  });

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
    try {
      rmSync(dir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  it('emits change events when watched files mutate', async () => {
    const changes: string[][] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useMessage('change', (msg) => {
          const p = msg.payload as { paths: string[] };
          changes.push(p.paths);
        });
        useSource(watchSource(dir), []);
        return { accepts: ['change'] };
      },
    });
    await scheduler.startAsync();
    // notify watchers can take a beat to fully wire up.
    await tick(80);

    writeFileSync(join(dir, 'hello.txt'), 'world');
    // Flush. Some platforms are slower than others; we use 200ms.
    await tick(200);

    // We expect at least one change event referencing our file.
    const sawHello = changes.some((paths) =>
      paths.some((p) => p.endsWith('hello.txt')),
    );
    expect(sawHello).toBe(true);
  });
});
