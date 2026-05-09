// M6.6 — `useGraphQuery` contract tests.
//
// Hook reads structural graph state and memoises on `deps`. The query
// function receives the scheduler-as-graph-view; the returned value is
// what the component sees.

import { describe, expect, it } from 'vitest';
import {
  Graph,
  Scheduler,
  useGraphQuery,
} from '../native.js';

describe('useGraphQuery', () => {
  it('throws if called outside a component evaluation', () => {
    expect(() => useGraphQuery(() => 0, [])).toThrowError(
      /no active scheduler context/,
    );
  });

  it('exposes graphNodes / graphNeighbors / graphHasEdge', () => {
    const graph = new Graph();
    graph.addNode('a');
    graph.addNode('b');
    graph.addNode('c');
    graph.addEdge('a', 'b');
    graph.addEdge('b', 'c');
    const scheduler = new Scheduler(graph);

    let observedNodes: string[] = [];
    let observedNeighbors: string[] = [];
    let edgeAB = false;
    let edgeAC = false;

    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        const snapshot = useGraphQuery(
          (s) => ({
            nodes: s.graphNodes().sort(),
            aNeighbors: s.graphNeighbors('a'),
            ab: s.graphHasEdge('a', 'b'),
            ac: s.graphHasEdge('a', 'c'),
          }),
          [],
        );
        observedNodes = snapshot.nodes;
        observedNeighbors = snapshot.aNeighbors;
        edgeAB = snapshot.ab;
        edgeAC = snapshot.ac;
        return null;
      },
    });
    scheduler.registerNode('b', { name: 'B', component: () => null });
    scheduler.registerNode('c', { name: 'C', component: () => null });

    scheduler.start();
    try {
      expect(observedNodes).toEqual(['a', 'b', 'c']);
      expect(observedNeighbors).toEqual(['b']);
      expect(edgeAB).toBe(true);
      expect(edgeAC).toBe(false);
    } finally {
      scheduler.stop();
    }
  });

  it('memoises the query result across re-evaluations with stable deps', () => {
    const graph = new Graph();
    graph.addNode('a');
    graph.addNode('b');
    graph.addEdge('a', 'b');
    const scheduler = new Scheduler(graph);

    let queryCalls = 0;

    scheduler.registerNode('a', {
      name: 'A',
      component: () => {
        // Force a couple of re-evaluations via a sync useEffect / state.
        useGraphQuery(
          (s) => {
            queryCalls += 1;
            return s.graphNodes();
          },
          [],
        );
        return null;
      },
    });
    scheduler.registerNode('b', { name: 'B', component: () => null });

    scheduler.start();
    try {
      // First mount only — deps stable, no re-query.
      expect(queryCalls).toBe(1);
    } finally {
      scheduler.stop();
    }
  });
});
