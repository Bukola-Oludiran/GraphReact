// Loader: `require()`s the cdylib that `cargo xtask napi-build` produces
// and re-exports its surface as named ES module exports.
//
// The hand-written types in `native.d.ts` describe the shape of the
// loaded module — kept side-by-side until `@napi-rs/cli` regen lands.
//
// ## Layered architecture
//
// 1. **Native (Rust)** — the `index.node` cdylib. Exposes the synchronous
//    scheduler surface, hooks (including the Rust-native event source
//    factories `timerSource` / `delaySource` / `httpSource` /
//    `watchSource` and `useSource`), graph CRUD, and the round-based
//    async protocol (`runStartRound` / `runBatchRound` /
//    `runSendRound` / `runStopRound` + `installAsyncCleanup`).
//
// 2. **JS-side `Scheduler` subclass** below — adds the async-cascade
//    drivers (`startAsync` / `processBatchAsync` / `sendAsync` /
//    `stopAsync`) and tracks the active scheduler / node id during
//    component evaluation so JS-side hooks (notably `useGraphQuery`)
//    can find them.
//
// 3. **JS-side hooks** below — only `useGraphQuery` lives here (a thin
//    wrapper around `useMemo` + the scheduler-as-graph-view). Sources
//    moved Rust-side in M6.6 because JS-stdlib shims (`node:http`,
//    `node:fs`) wouldn't survive M12's standalone V8-via-Zig CLI
//    (no Node).

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import type * as Native from './native.js';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const binaryPath = resolve(here, 'index.node');

// eslint-disable-next-line @typescript-eslint/no-var-requires
const native = require(binaryPath) as typeof Native;

// ---------------------------------------------------------------------------
// Async cascade driver (M6.5)
// ---------------------------------------------------------------------------

/// Drive a round-based async protocol to quiescence.
/// Each iteration awaits any captured Promises, captures any returned
/// cleanup functions back into the native scheduler, then runs the next
/// `runBatchRound`. Loops until a round captures no Promises.
async function driveAsync(
  scheduler: Native.Scheduler,
  initialResult: Native.AsyncRoundResult,
): Promise<void> {
  let result = initialResult;
  while (result.pendingPromises.length > 0) {
    const entries = result.pendingPromises;
    // Settle all Promises in parallel. `allSettled` so one rejection
    // doesn't abort the cascade — rejections are surfaced as warnings,
    // matching the "unhandled rejection logs" semantics of sync effect
    // errors elsewhere in the bridge.
    const settled = await Promise.allSettled(entries.map((e) => e.promise));
    settled.forEach((outcome, i) => {
      if (outcome.status === 'fulfilled') {
        const value = outcome.value;
        if (typeof value === 'function') {
          scheduler.installAsyncCleanup(
            entries[i].cleanupId,
            value as () => void,
          );
        }
      } else {
        // Match the eprintln! convention used elsewhere in the bridge.
        // eslint-disable-next-line no-console
        console.error(
          '[graphreact-napi] async effect rejected:',
          outcome.reason,
        );
      }
    });
    result = scheduler.runBatchRound();
  }
}

// ---------------------------------------------------------------------------
// Active context tracking — JS-side equivalent of CURRENT_NODE
// ---------------------------------------------------------------------------
//
// `useGraphQuery` needs the scheduler reference at hook-time so the
// queryFn it memoises can read live-graph accessors. Rust-side hooks
// already get this via thread-locals (`CURRENT_NODE`); the JS side
// installs its own equivalent globals during component invocation
// via the `Scheduler` subclass below.

let currentScheduler: Scheduler | null = null;

function withActiveContext<T>(scheduler: Scheduler, body: () => T): T {
  const prev = currentScheduler;
  currentScheduler = scheduler;
  try {
    return body();
  } finally {
    currentScheduler = prev;
  }
}

function wrapDefinition(
  scheduler: Scheduler,
  def: Native.NodeDefinition,
): Native.NodeDefinition {
  return {
    name: def.name,
    component: (ctx: Native.NodeContext) =>
      withActiveContext(scheduler, () => def.component(ctx)),
  };
}

// ---------------------------------------------------------------------------
// Scheduler subclass — async cascade + active-context wiring
// ---------------------------------------------------------------------------

/// Public `Scheduler` — extends the native class with JS-driven async
/// methods AND wraps every component definition so JS-side hooks
/// (`useGraphQuery`) can find the active scheduler.
class Scheduler extends native.Scheduler {
  constructor(graph: Native.Graph) {
    super(graph);
    // Source-wakeup bridge: when a Rust-native source pushes an event
    // from a tokio worker thread, it invokes a threadsafe-function
    // that lands here on the V8 main thread. We respond by draining
    // through the async cascade (`processBatchAsync`). Errors are
    // logged; we never propagate (the wake-up is a best-effort
    // signal, not a request whose result the caller awaits).
    this.installSourceWakeup(() => {
      this.processBatchAsync().catch((err: unknown) => {
        // eslint-disable-next-line no-console
        console.error('[graphreact-napi] source wakeup error:', err);
      });
    });
  }

  registerNode(
    id: string,
    definition: Native.NodeDefinition,
    props?: Record<string, unknown>,
    height?: number,
  ): void {
    super.registerNode(id, wrapDefinition(this, definition), props, height);
  }

  addNodeAtRuntime(
    id: string,
    definition: Native.NodeDefinition,
    props?: Record<string, unknown>,
    connectTo?: string[],
  ): void {
    super.addNodeAtRuntime(
      id,
      wrapDefinition(this, definition),
      props,
      connectTo,
    );
  }

  replaceComponent(id: string, definition: Native.NodeDefinition): void {
    super.replaceComponent(id, wrapDefinition(this, definition));
  }

  async startAsync(): Promise<void> {
    return driveAsync(this, this.runStartRound());
  }

  async processBatchAsync(): Promise<void> {
    return driveAsync(this, this.runBatchRound());
  }

  async sendAsync(target: string, type: string, payload: unknown): Promise<void> {
    return driveAsync(this, this.runSendRound(target, type, payload));
  }

  async stopAsync(): Promise<void> {
    return driveAsync(this, this.runStopRound());
  }
}

export { Scheduler };

// ---------------------------------------------------------------------------
// JS-side hooks — `useGraphQuery` only (sources are Rust-native)
// ---------------------------------------------------------------------------

/// `useGraphQuery(queryFn, deps)` — read-only query against the live
/// graph, memoised on `deps`. The query function receives the
/// scheduler-as-graph-view (carries `graphNodes` / `graphNeighbors` /
/// `graphHasEdge` / `graphGetNodeData` accessors).
///
/// The TS reference exposes the underlying graph object directly; we
/// expose the scheduler-as-view because the JS-held `Graph` wrapper is
/// moved-from at scheduler construction. Both APIs serve the same
/// purpose: cache an expensive structural lookup across re-evaluations.
export function useGraphQuery<T>(
  queryFn: (graph: Scheduler) => T,
  deps: unknown[],
): T {
  const scheduler = currentScheduler;
  if (!scheduler) {
    throw new Error(
      'useGraphQuery: no active scheduler context — must be called from inside a node component body',
    );
  }
  const captured = scheduler;
  return native.useMemo(() => queryFn(captured), deps) as T;
}

// ---------------------------------------------------------------------------
// Native pass-through exports
// ---------------------------------------------------------------------------

export const {
  Graph,
  Signal,
  Reducer,
  MonitorRef,
  SpawnChildHandle,
  UnmountSelfHandle,
  ComposeHandle,
  AcceptEdgeHandle,
  EventSourceHandle,
  napiSmoke,
  version,
  useState,
  useReducer,
  emit,
  useMessage,
  useEffect,
  useEffectAsync,
  useMemo,
  useCallback,
  useNeighbors,
  useTopology,
  useSource,
  useMonitor,
  useDemonitor,
  useLink,
  useUnlink,
  useGroupJoin,
  useGroupLeave,
  useSpawnChild,
  useUnmountSelf,
  useCompose,
  useAcceptEdge,
  timerSource,
  delaySource,
  httpSource,
  watchSource,
  bfsNodes,
  dfsNodes,
  hasCycle,
  findCycle,
  connectedComponents,
  isConnected,
  numberOfConnectedComponents,
  shortestPath,
  areConnected,
} = native;

export default native;
