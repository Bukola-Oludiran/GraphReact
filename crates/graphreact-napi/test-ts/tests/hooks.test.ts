import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  emit,
  useEffect,
  useMessage,
  useState,
} from '../native.js';
import type { IncomingMessage, NodeContext } from '../native.js';

describe('Hooks: useState', () => {
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

  it('initializes state and round-trips reads', () => {
    let observed: unknown = undefined;
    scheduler.registerNode('n', {
      name: 'N',
      component: (_ctx: NodeContext) => {
        const count = useState(0);
        observed = count.get();
        return null;
      },
    });
    scheduler.start();
    expect(observed).toBe(0);
  });

  it('persists state across re-evaluations and triggers re-eval on set', () => {
    const observations: number[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: (_ctx: NodeContext) => {
        const count = useState(0);
        observations.push(count.get() as number);
        // First eval mounts useEffect, which fires once and sets the
        // counter — that must trigger a re-eval observing the new value.
        useEffect(() => {
          if (count.get() === 0) {
            count.set(7);
          }
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(observations).toEqual([0, 7]);
  });

  it('update() applies the JS-supplied updater function', () => {
    let observed = -1;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        const count = useState(10);
        useEffect(() => {
          if (count.get() === 10) {
            count.update((c) => (c as number) + 5);
          }
          return undefined;
        }, []);
        observed = count.get() as number;
        return null;
      },
    });
    scheduler.start();
    expect(observed).toBe(15);
  });
});

describe('Hooks: emit + useMessage', () => {
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

  it('delivers messages between connected nodes', () => {
    const received: IncomingMessage[] = [];
    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        useEffect(() => {
          emit('hello', { greeting: 'hi from a' });
          return undefined;
        }, []);
        return { emits: ['hello'] };
      },
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: () => {
        useMessage('hello', (msg) => {
          received.push(msg);
        });
        return { accepts: ['hello'] };
      },
    });
    scheduler.start();
    expect(received).toHaveLength(1);
    expect(received[0].from).toBe('a');
    expect(received[0].type).toBe('hello');
    expect(received[0].payload).toEqual({ greeting: 'hi from a' });
  });

  it('respects manifest accepts — refuses unlisted message types', () => {
    let aReceivedSomething = false;
    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        useMessage('foo', () => {
          aReceivedSomething = true;
        });
        return { accepts: ['foo'] };
      },
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: () => {
        useEffect(() => {
          // 'a' doesn't accept 'unrelated' — message must be dropped.
          emit('unrelated', {});
          return undefined;
        }, []);
        return { emits: ['unrelated'] };
      },
    });
    scheduler.start();
    expect(aReceivedSomething).toBe(false);
  });
});

describe('Hooks: useEffect', () => {
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

  it('runs once with empty deps and the cleanup runs on unmount', () => {
    let runs = 0;
    let cleanups = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        useEffect(() => {
          runs += 1;
          return () => {
            cleanups += 1;
          };
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(runs).toBe(1);
    expect(cleanups).toBe(0);

    scheduler.stop();
    expect(cleanups).toBe(1);
  });

  it('re-runs cleanup before re-running the effect when deps change', () => {
    const events: string[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        const tick = useState(0);
        useEffect(() => {
          // Snapshot the current value at effect-run time. The cleanup
          // closure captures this snapshot rather than reading the live
          // Signal, so its log entry reflects the effect's *own* tick.
          const t = tick.get();
          events.push(`run:${t}`);
          return () => {
            events.push(`cleanup:${t}`);
          };
        }, [tick.get()]);
        // Drive a single dep change after mount.
        useEffect(() => {
          if (tick.get() === 0) {
            tick.set(1);
          }
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(events).toEqual(['run:0', 'cleanup:0', 'run:1']);
  });
});
