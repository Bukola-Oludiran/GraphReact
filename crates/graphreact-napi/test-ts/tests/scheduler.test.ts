import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Graph, Scheduler } from '../native.js';
import type { NodeContext } from '../native.js';

describe('Scheduler basics', () => {
  let graph: Graph;
  let scheduler: Scheduler;

  // Each test owns its own graph + scheduler; build the topology BEFORE
  // constructing the scheduler because `new Scheduler(graph)` moves the
  // internal graph state out of the JS-side Graph wrapper.
  beforeEach(() => {
    graph = new Graph();
  });

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // Some tests may not have started; tolerate.
    }
  });

  it('runs the component once on start and stores the manifest', () => {
    graph.addNode('n');
    scheduler = new Scheduler(graph);
    let calls = 0;
    scheduler.registerNode('n', {
      name: 'N',
      component: (ctx: NodeContext) => {
        calls += 1;
        expect(ctx.id).toBe('n');
        expect(ctx.props).toEqual({});
        expect(ctx.neighbors).toEqual([]);
        return { accepts: ['ping'], emits: ['pong'] };
      },
    });
    scheduler.start();
    expect(calls).toBe(1);
  });

  it('threads props through to ctx.props', () => {
    graph.addNode('n');
    scheduler = new Scheduler(graph);
    let captured: Record<string, unknown> | null = null;
    scheduler.registerNode(
      'n',
      {
        name: 'N',
        component: (ctx: NodeContext) => {
          captured = ctx.props;
          return null;
        },
      },
      { interval: 30, label: 'sensor' },
    );
    scheduler.start();
    expect(captured).toEqual({ interval: 30, label: 'sensor' });
  });

  it('exposes structural neighbors via ctx.neighbors', () => {
    graph.addNode('n');
    graph.addNode('m');
    graph.addEdge('n', 'm');
    scheduler = new Scheduler(graph);

    let neighborsSnapshot: string[] = [];
    scheduler.registerNode('n', {
      name: 'N',
      component: (ctx: NodeContext) => {
        neighborsSnapshot = ctx.neighbors;
        return null;
      },
    });
    scheduler.registerNode('m', {
      name: 'M',
      component: () => null,
    });
    scheduler.start();
    expect(neighborsSnapshot).toContain('m');
  });

  it('hasNode reflects registration', () => {
    graph.addNode('n');
    scheduler = new Scheduler(graph);
    expect(scheduler.hasNode('n')).toBe(false);
    scheduler.registerNode('n', { name: 'N', component: () => null });
    expect(scheduler.hasNode('n')).toBe(true);
  });
});
