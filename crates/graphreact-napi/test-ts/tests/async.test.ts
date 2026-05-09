// M6.5 — async cascade contract tests.
//
// The native bridge no longer blocks the V8 main thread on async
// effects. The exported `Scheduler` subclass drives the multi-round
// cascade in JS via `Promise.all`; these tests pin the externally
// observable contract.

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  emit,
  useEffect,
  useEffectAsync,
  useMessage,
  useState,
} from '../native.js';
import type { NodeContext } from '../native.js';

// Small deterministic delay — resolve on the next macrotask boundary so
// any V8 microtask processing has a chance to run between rounds.
const tick = (ms = 0): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

describe('Async: useEffectAsync basic', () => {
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

  it('runs an async effect without deadlock', async () => {
    let observed = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffectAsync(async () => {
          await tick();
          observed = 42;
        }, []);
        return null;
      },
    });
    await scheduler.startAsync();
    expect(observed).toBe(42);
  });

  it('awaits a Promise that updates state, then re-evaluates', async () => {
    const observations: number[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        const v = useState(0);
        observations.push(v.get() as number);
        useEffectAsync(async () => {
          if ((v.get() as number) === 0) {
            await tick();
            v.set(7);
          }
        }, [v.get()]);
        return null;
      },
    });
    await scheduler.startAsync();
    expect(observations).toEqual([0, 7]);
  });

  it('runs multiple async effects on the same node in parallel', async () => {
    const order: string[] = [];
    const start = Date.now();
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffectAsync(async () => {
          await tick(20);
          order.push('a');
        }, []);
        useEffectAsync(async () => {
          await tick(20);
          order.push('b');
        }, []);
        useEffectAsync(async () => {
          await tick(20);
          order.push('c');
        }, []);
        return null;
      },
    });
    await scheduler.startAsync();
    const elapsed = Date.now() - start;
    expect(order.sort()).toEqual(['a', 'b', 'c']);
    // Sequential would be ~60ms; parallel should land closer to ~20ms.
    // Use a generous upper bound (<50ms) to avoid CI flake.
    expect(elapsed).toBeLessThan(50);
  });
});

describe('Async: cleanups', () => {
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

  it('captures an async-resolved cleanup and runs it on unmount', async () => {
    let cleanups = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffectAsync(async () => {
          await tick();
          return () => {
            cleanups += 1;
          };
        }, []);
        return null;
      },
    });
    await scheduler.startAsync();
    expect(cleanups).toBe(0);
    scheduler.stop();
    expect(cleanups).toBe(1);
  });

  it('runs the captured cleanup before re-running on dep change', async () => {
    // Realistic ordering: cleanup of run N can only fire once run N's
    // Promise has resolved. We drive the dep change via a message AFTER
    // `startAsync` has settled (so run:0 has fully resolved + had its
    // cleanup captured) — then the dep change in `sendAsync` properly
    // sequences `cleanup:0` before `run:1`.
    const events: string[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        const tickState = useState(0);
        useMessage('bump', () => {
          tickState.set((tickState.get() as number) + 1);
        });
        useEffectAsync(async () => {
          const t = tickState.get();
          await Promise.resolve();
          events.push(`run:${t}`);
          return () => {
            events.push(`cleanup:${t}`);
          };
        }, [tickState.get()]);
        return { accepts: ['bump'] };
      },
    });
    await scheduler.startAsync();
    expect(events).toEqual(['run:0']);
    await scheduler.sendAsync('n', 'bump', {});
    expect(events).toEqual(['run:0', 'cleanup:0', 'run:1']);
  });

  it('mixes sync and async effects on the same node', async () => {
    const events: string[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffect(() => {
          events.push('sync-run');
          return () => {
            events.push('sync-cleanup');
          };
        }, []);
        useEffectAsync(async () => {
          await tick();
          events.push('async-run');
          return () => {
            events.push('async-cleanup');
          };
        }, []);
        return null;
      },
    });
    await scheduler.startAsync();
    expect(events).toEqual(['sync-run', 'async-run']);
    scheduler.stop();
    // Cleanup order is per-effect-slot reverse install order isn't
    // currently guaranteed by the runtime; just assert both fired.
    expect(events.filter((e) => e.endsWith('-cleanup')).sort()).toEqual([
      'async-cleanup',
      'sync-cleanup',
    ]);
  });
});

describe('Async: cross-node cascade', () => {
  let graph: Graph;
  let scheduler: Scheduler;

  beforeEach(() => {
    graph = new Graph();
    graph.addNode('a');
    graph.addNode('b');
    graph.addEdge('a', 'b');
    scheduler = new Scheduler(graph);
  });

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('sendAsync delivers a message and awaits handler-triggered async effects', async () => {
    const events: string[] = [];
    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        useMessage('ping', () => {
          events.push('a:ping');
          emit('pong', { from: 'a' });
        });
        return { accepts: ['ping'], emits: ['pong'] };
      },
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: (_ctx: NodeContext) => {
        const handled = useState(false);
        useMessage('pong', () => {
          handled.set(true);
        });
        useEffectAsync(async () => {
          if (handled.get()) {
            await tick();
            events.push('b:async-after-pong');
          }
        }, [handled.get()]);
        return { accepts: ['pong'] };
      },
    });
    await scheduler.startAsync();
    await scheduler.sendAsync('a', 'ping', { hello: true });
    expect(events).toEqual(['a:ping', 'b:async-after-pong']);
  });

  it('drives a multi-round cascade: async resolution updates state, triggers next round', async () => {
    const events: string[] = [];
    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        const phase = useState('idle');
        events.push(`a:${phase.get()}`);
        useEffectAsync(async () => {
          if (phase.get() === 'idle') {
            await tick();
            phase.set('working');
          } else if (phase.get() === 'working') {
            await tick();
            phase.set('done');
          }
        }, [phase.get()]);
        return null;
      },
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: () => null,
    });
    await scheduler.startAsync();
    expect(events).toEqual(['a:idle', 'a:working', 'a:done']);
  });
});

describe('Async: error handling', () => {
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

  it('a rejected async effect does not crash the scheduler', async () => {
    let runs = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        const tickState = useState(0);
        runs += 1;
        useEffectAsync(async () => {
          if (tickState.get() === 0) {
            await tick();
            tickState.set(1);
            throw new Error('boom');
          }
        }, [tickState.get()]);
        return null;
      },
    });
    await scheduler.startAsync();
    // Both runs should have occurred — rejection logged but not fatal.
    expect(runs).toBeGreaterThanOrEqual(2);
  });

  it('a non-Promise return from useEffectAsync is reported but not fatal', async () => {
    let observed = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        // @ts-expect-error — deliberate violation of the typed contract
        useEffectAsync(() => {
          observed = 1;
          return 42;
        }, []);
        return null;
      },
    });
    await scheduler.startAsync();
    // Effect body still ran; bridge logs a warning about the missing
    // Promise but doesn't unwind.
    expect(observed).toBe(1);
  });
});

describe('Async: sync-mode degradation', () => {
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

  it('useEffectAsync called from sync start() fires the body fire-and-forget', async () => {
    let started = false;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffectAsync(async () => {
          started = true;
          await tick();
        }, []);
        return null;
      },
    });
    scheduler.start(); // sync entry — Promise has no awaiter
    expect(started).toBe(true);
    // Drain microtasks so any unhandled-rejection warning isn't dragged
    // into the next test.
    await tick();
  });
});

describe('Async: scale', () => {
  it('parallelises async effects across many nodes via startAsync', async () => {
    const N = 50;
    const graph = new Graph();
    for (let i = 0; i < N; i += 1) graph.addNode(`n${i}`);
    const scheduler = new Scheduler(graph);
    let completed = 0;
    for (let i = 0; i < N; i += 1) {
      scheduler.registerNode(`n${i}`, {
        name: 'N',
        component: () => {
          useEffectAsync(async () => {
            await tick(15);
            completed += 1;
          }, []);
          return null;
        },
      });
    }
    const start = Date.now();
    await scheduler.startAsync();
    const elapsed = Date.now() - start;
    expect(completed).toBe(N);
    // Sequential would be N*15 ≈ 750ms; parallel should land well under
    // 100ms even on slow hosts.
    expect(elapsed).toBeLessThan(100);
    scheduler.stop();
  });
});
