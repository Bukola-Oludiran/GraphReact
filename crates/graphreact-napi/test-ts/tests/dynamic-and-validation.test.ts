import { afterEach, describe, expect, it } from 'vitest';
import { Graph, Scheduler, useEffect, useState } from '../native.js';

describe('dynamic mutation: addNodeAtRuntime / removeNodeAtRuntime', () => {
  let scheduler: Scheduler;

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('adds a node after start() and evaluates it immediately', () => {
    const graph = new Graph();
    graph.addNode('seed');
    scheduler = new Scheduler(graph);
    scheduler.registerNode('seed', { name: 'Seed', component: () => null });
    scheduler.start();
    expect(scheduler.hasNode('runtime-add')).toBe(false);

    let evalCount = 0;
    scheduler.addNodeAtRuntime('runtime-add', {
      name: 'Added',
      component: () => {
        evalCount += 1;
        return null;
      },
    });
    expect(scheduler.hasNode('runtime-add')).toBe(true);
    expect(evalCount).toBe(1);
  });

  it('removeNodeAtRuntime drops cleanups and removes the node', () => {
    const graph = new Graph();
    graph.addNode('keep');
    scheduler = new Scheduler(graph);
    scheduler.registerNode('keep', { name: 'Keep', component: () => null });
    scheduler.start();

    let cleanedUp = 0;
    scheduler.addNodeAtRuntime('temp', {
      name: 'Temp',
      component: () => {
        useEffect(() => {
          return () => {
            cleanedUp += 1;
          };
        }, []);
        return null;
      },
    });
    expect(scheduler.hasNode('temp')).toBe(true);

    scheduler.removeNodeAtRuntime('temp');
    expect(scheduler.hasNode('temp')).toBe(false);
    expect(cleanedUp).toBe(1);
  });
});

describe('configurable maxProcessDepth', () => {
  it('reports default 100 and accepts a custom cap', () => {
    const graph = new Graph();
    const scheduler = new Scheduler(graph);
    expect(scheduler.maxProcessDepth).toBe(100);
    scheduler.setMaxProcessDepth(500);
    expect(scheduler.maxProcessDepth).toBe(500);
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('throws MaxDepthExceeded when the cap is hit', () => {
    const graph = new Graph();
    graph.addNode('flapper');
    const scheduler = new Scheduler(graph);
    scheduler.setMaxProcessDepth(5);
    scheduler.registerNode('flapper', {
      name: 'Flapper',
      component: () => {
        // Always re-dirty itself — guaranteed cascade.
        const s = useState(0);
        useEffect(() => {
          s.update((v) => (v as number) + 1);
          return undefined;
        }, [s.get()]);
        return null;
      },
    });
    expect(() => scheduler.start()).toThrow();
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });
});

describe('validateGraph', () => {
  it('warns about isolated nodes', () => {
    const graph = new Graph();
    graph.addNode('lonely');
    const scheduler = new Scheduler(graph);
    scheduler.registerNode('lonely', { name: 'Lonely', component: () => null });
    scheduler.start();
    const warnings = scheduler.validateGraph();
    expect(warnings.some((w) => w.message.includes('isolated'))).toBe(true);
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('warns when neighbors share no compatible message types', () => {
    const graph = new Graph();
    graph.addNode('a');
    graph.addNode('b');
    graph.addEdge('a', 'b');
    const scheduler = new Scheduler(graph);
    scheduler.registerNode('a', {
      name: 'A',
      component: () => ({ emits: ['foo'], accepts: ['ping'] }),
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: () => ({ emits: ['bar'], accepts: ['pong'] }),
    });
    scheduler.start();
    const warnings = scheduler.validateGraph();
    expect(warnings.some((w) => w.message.includes('no compatible message types'))).toBe(true);
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('clean graph produces no warnings', () => {
    const graph = new Graph();
    graph.addNode('a');
    graph.addNode('b');
    graph.addEdge('a', 'b');
    const scheduler = new Scheduler(graph);
    scheduler.registerNode('a', {
      name: 'A',
      component: () => ({ emits: ['ping'], accepts: ['pong'] }),
    });
    scheduler.registerNode('b', {
      name: 'B',
      component: () => ({ emits: ['pong'], accepts: ['ping'] }),
    });
    scheduler.start();
    expect(scheduler.validateGraph()).toEqual([]);
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });
});
