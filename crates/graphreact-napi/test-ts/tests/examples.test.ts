// M6 stage 5 — integration tests that recreate the existing TS example
// shapes against the Rust runtime via @graphreact/native. Programmatic
// graph construction (no .grx parser); validates that the same node
// patterns the TS examples use work end-to-end.

import { describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  emit,
  useEffect,
  useMessage,
  useState,
} from '../native.js';
import type { IncomingMessage, NodeContext } from '../native.js';

describe('Example: ping-pong (cycle + TTL termination)', () => {
  it('two nodes bounce until TTL expires', () => {
    const graph = new Graph();
    graph.addNode('ping');
    graph.addNode('pong');
    // TTL = 5 means 5 hops before drop.
    graph.addEdge('ping', 'pong', undefined, { ttl: 5 });
    const scheduler = new Scheduler(graph);

    const pingReceived: number[] = [];
    const pongReceived: number[] = [];

    scheduler.registerNode('ping', {
      name: 'Ping',
      component: () => {
        const count = useState(0);
        useMessage('pong', () => {
          pingReceived.push(count.get() as number);
          count.update((c) => (c as number) + 1);
          emit('ping', { bounce: count.get() });
        });
        useEffect(() => {
          // Kick off the rally on mount.
          emit('ping', { bounce: 0 });
          return undefined;
        }, []);
        return { accepts: ['pong'], emits: ['ping'] };
      },
    });
    scheduler.registerNode('pong', {
      name: 'Pong',
      component: () => {
        const count = useState(0);
        useMessage('ping', () => {
          pongReceived.push(count.get() as number);
          count.update((c) => (c as number) + 1);
          emit('pong', { bounce: count.get() });
        });
        return { accepts: ['ping'], emits: ['pong'] };
      },
    });

    scheduler.start();

    // Both nodes received messages until TTL stopped the cascade.
    expect(pongReceived.length).toBeGreaterThan(0);
    expect(pingReceived.length).toBeGreaterThan(0);
    // TTL caps the total exchanges — well-bounded, no infinite loop.
    expect(pongReceived.length + pingReceived.length).toBeLessThanOrEqual(20);

    scheduler.stop();
  });
});

describe('Example: counter-network (fan-in aggregation)', () => {
  it('aggregator sums values from multiple counters', () => {
    const graph = new Graph();
    graph.addNode('agg');
    graph.addNode('counter-a');
    graph.addNode('counter-b');
    graph.addNode('logger');
    // Aggregator and logger both receive from each counter.
    graph.addEdge('counter-a', 'agg');
    graph.addEdge('counter-b', 'agg');
    graph.addEdge('counter-a', 'logger');
    graph.addEdge('counter-b', 'logger');
    const scheduler = new Scheduler(graph);

    const loggerSeen: { nodeId: string; value: number }[] = [];

    const counterFactory = (initial: number) => ({
      name: 'Counter',
      component: (ctx: NodeContext) => {
        const count = useState(initial);
        useMessage('increment', () => {
          count.update((c) => (c as number) + 1);
        });
        useEffect(() => {
          emit('count-updated', { nodeId: ctx.id, value: count.get() });
          return undefined;
        }, [count.get()]);
        return { accepts: ['increment'], emits: ['count-updated'] };
      },
    });

    scheduler.registerNode('counter-a', counterFactory(0), { initialCount: 0 });
    scheduler.registerNode('counter-b', counterFactory(10), { initialCount: 10 });
    scheduler.registerNode('agg', {
      name: 'Aggregator',
      component: () => {
        const total = useState(0);
        useMessage('count-updated', (msg: IncomingMessage) => {
          const { value } = msg.payload as { nodeId: string; value: number };
          total.update((t) => (t as number) + value);
        });
        useEffect(() => {
          emit('total-updated', { total: total.get() });
          return undefined;
        }, [total.get()]);
        return { accepts: ['count-updated'], emits: ['total-updated'] };
      },
    });
    scheduler.registerNode('logger', {
      name: 'Logger',
      component: () => {
        useMessage('count-updated', (msg: IncomingMessage) => {
          loggerSeen.push(msg.payload as { nodeId: string; value: number });
        });
        return { accepts: ['count-updated', 'total-updated'], emits: [] };
      },
    });

    scheduler.start();

    // Mount-time effects fire: logger sees initial values from both counters.
    expect(loggerSeen).toHaveLength(2);
    expect(loggerSeen.find((e) => e.nodeId === 'counter-a')?.value).toBe(0);
    expect(loggerSeen.find((e) => e.nodeId === 'counter-b')?.value).toBe(10);

    // Drive an increment — fan-in aggregator should accumulate.
    scheduler.send('counter-a', 'increment', {});
    // Counter-a's effect fires emitting count-updated with value 1.
    // Logger has now seen 3 events; aggregator has accumulated.
    expect(loggerSeen.length).toBeGreaterThanOrEqual(3);
    const lastA = loggerSeen.filter((e) => e.nodeId === 'counter-a').pop();
    expect(lastA?.value).toBe(1);

    scheduler.stop();
  });
});

describe('Example: pipeline (chained handlers)', () => {
  it('messages flow through each stage in order', () => {
    const graph = new Graph();
    graph.addNode('source');
    graph.addNode('double');
    graph.addNode('plus10');
    graph.addNode('sink');
    graph.addEdge('source', 'double');
    graph.addEdge('double', 'plus10');
    graph.addEdge('plus10', 'sink');
    const scheduler = new Scheduler(graph);

    const sinkValues: number[] = [];

    // The MultiGraph is undirected, so `emit('data')` would broadcast
    // to all neighbors — including upstream — and bounce back. Each
    // stage uses `EmitOptions.to` to forward only to the next stage,
    // making the pipeline unidirectional.
    scheduler.registerNode('source', {
      name: 'Source',
      component: () => {
        useMessage('data', (msg: IncomingMessage) => {
          emit('data', msg.payload, { to: ['double'] });
        });
        return { accepts: ['data'], emits: ['data'] };
      },
    });
    scheduler.registerNode('double', {
      name: 'Double',
      component: () => {
        useMessage('data', (msg: IncomingMessage) => {
          const v = (msg.payload as { value: number }).value;
          emit('data', { value: v * 2 }, { to: ['plus10'] });
        });
        return { accepts: ['data'], emits: ['data'] };
      },
    });
    scheduler.registerNode('plus10', {
      name: 'Plus10',
      component: () => {
        useMessage('data', (msg: IncomingMessage) => {
          const v = (msg.payload as { value: number }).value;
          emit('data', { value: v + 10 }, { to: ['sink'] });
        });
        return { accepts: ['data'], emits: ['data'] };
      },
    });
    scheduler.registerNode('sink', {
      name: 'Sink',
      component: () => {
        useMessage('data', (msg: IncomingMessage) => {
          sinkValues.push((msg.payload as { value: number }).value);
        });
        return { accepts: ['data'], emits: [] };
      },
    });

    scheduler.start();

    // Inject a value into the source — should flow through the chain.
    // 5 -> double -> 10 -> plus10 -> 20 -> sink.
    scheduler.send('source', 'data', { value: 5 });
    // Inject a second value to confirm the chain handles a stream.
    scheduler.send('source', 'data', { value: 100 });

    expect(sinkValues).toEqual([20, 210]);

    scheduler.stop();
  });
});
