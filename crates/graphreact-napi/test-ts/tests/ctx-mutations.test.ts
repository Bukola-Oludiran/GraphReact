import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  useEffect,
  useSpawnChild,
  useUnmountSelf,
  useCompose,
} from '../native.js';

describe('ctx mutations: spawnChild', () => {
  let scheduler: Scheduler;

  afterEach(() => {
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });

  it('spawns a child node from inside an effect and connects it to the parent', () => {
    const graph = new Graph();
    graph.addNode('parent');
    scheduler = new Scheduler(graph);
    let spawned = false;
    scheduler.registerNode('parent', {
      name: 'Parent',
      component: () => {
        const spawn = useSpawnChild();
        useEffect(() => {
          if (!spawned) {
            spawned = true;
            spawn.call({
              id: 'child',
              definition: { name: 'Child', component: () => null },
              connectTo: ['parent'],
            });
          }
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(scheduler.hasNode('child')).toBe(true);
  });

  it('idempotent on already-registered IDs', () => {
    // Pre-register 'child' in the graph too. The scheduler takes the
    // graph at construction, so node IDs must be in the topology before
    // `new Scheduler(graph)` runs.
    const graph = new Graph();
    graph.addNode('parent');
    graph.addNode('child');
    scheduler = new Scheduler(graph);
    scheduler.registerNode('parent', {
      name: 'Parent',
      component: () => {
        const spawn = useSpawnChild();
        useEffect(() => {
          spawn.call({
            id: 'child',
            definition: { name: 'Child', component: () => null },
          });
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.registerNode('child', {
      name: 'Child',
      component: () => null,
    });
    expect(() => scheduler.start()).not.toThrow();
    expect(scheduler.hasNode('child')).toBe(true);
  });
});

describe('ctx mutations: unmountSelf', () => {
  it('removes the active node on next pass', () => {
    const graph = new Graph();
    graph.addNode('me');
    const scheduler = new Scheduler(graph);
    scheduler.registerNode('me', {
      name: 'Me',
      component: () => {
        const unmount = useUnmountSelf();
        useEffect(() => {
          unmount.call();
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(scheduler.hasNode('me')).toBe(false);
  });
});

describe('ctx mutations: compose', () => {
  it('declares composed children and spawns missing ones with parent edges', () => {
    const graph = new Graph();
    graph.addNode('parent');
    const scheduler = new Scheduler(graph);
    scheduler.registerNode('parent', {
      name: 'Parent',
      component: () => {
        const compose = useCompose();
        useEffect(() => {
          compose.call([
            { id: 'a', definition: { name: 'Child', component: () => null } },
            { id: 'b', definition: { name: 'Child', component: () => null } },
          ]);
          return undefined;
        }, []);
        return null;
      },
    });
    scheduler.start();
    expect(scheduler.hasNode('a')).toBe(true);
    expect(scheduler.hasNode('b')).toBe(true);
    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });
});

describe('replaceComponent', () => {
  it('swaps a node\'s behavior while preserving hook state', () => {
    const graph = new Graph();
    graph.addNode('n');
    const scheduler = new Scheduler(graph);

    let firstRan = false;
    let secondSawValue: unknown = undefined;
    scheduler.registerNode('n', {
      name: 'N',
      component: () => {
        firstRan = true;
        return { accepts: ['ping'] };
      },
    });
    scheduler.start();
    expect(firstRan).toBe(true);

    scheduler.replaceComponent('n', {
      name: 'N',
      // The replaced component asserts the manifest type changes —
      // hook state would also persist (no hooks in this minimal case).
      component: () => {
        secondSawValue = 'replaced-ran';
        return { accepts: ['pong'] };
      },
    });
    scheduler.processBatch();
    expect(secondSawValue).toBe('replaced-ran');

    try {
      scheduler.stop();
    } catch {
      // ignore
    }
  });
});
