import { describe, expect, it } from 'vitest';
import { Graph, napiSmoke, version } from '../native.js';

describe('@graphreact/native — module load', () => {
  it('loads the cdylib and reports a version', () => {
    expect(napiSmoke()).toBe('graphreact-napi alive');
    expect(typeof version()).toBe('string');
    expect(version()).not.toBe('');
  });
});

describe('Graph CRUD', () => {
  it('adds nodes + edges and reports order/size', () => {
    const g = new Graph();
    expect(g.order).toBe(0);
    expect(g.size).toBe(0);

    g.addNode('a');
    g.addNode('b');
    g.addNode('c');
    expect(g.order).toBe(3);
    expect(g.nodes()).toEqual(['a', 'b', 'c']);

    g.addEdge('a', 'b');
    g.addEdge('b', 'c');
    expect(g.size).toBe(2);
    expect(g.hasEdge('a', 'b')).toBe(true);
    expect(g.hasEdge('a', 'c')).toBe(false);
    expect(g.neighbors('b').sort()).toEqual(['a', 'c']);
  });

  it('rejects edges that reference unknown nodes only at the right places', () => {
    const g = new Graph();
    g.addNode('a');
    expect(() => g.removeEdge('a', 'missing')).toThrow();
  });

  it('round-trips node attributes', () => {
    const g = new Graph();
    g.addNode('s', { kind: 'sensor', interval: 30 });
    expect(g.getNodeData('s')).toEqual({ kind: 'sensor', interval: 30 });
  });
});
