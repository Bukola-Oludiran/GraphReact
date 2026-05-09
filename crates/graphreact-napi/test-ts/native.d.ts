// Hand-written types for the test harness — covers the surface the
// vitest contract tests exercise. Once `@napi-rs/cli` is wired in we'll
// regenerate this from napi-derive macros instead.

export interface NodeContext {
  id: string;
  props: Record<string, unknown>;
  neighbors: string[];
  myDepth: number;
  myPath: string[];
  children: { id: string; component: string }[];
}

export interface NodeDefinition {
  name: string;
  component: (ctx: NodeContext) => PermissionManifest | null | undefined;
}

export interface PermissionManifest {
  accepts?: string[];
  emits?: string[];
}

export interface IncomingMessage {
  from: string;
  type: string;
  payload: unknown;
  ttl: number;
}

export interface EmitOptions {
  to?: string[];
  ttl?: number;
  edgeKey?: string;
  direct?: string;
}

export interface ValidationWarning {
  level: 'warn' | 'error';
  message: string;
}

export interface EdgeTuple {
  source: string;
  target: string;
  key: string;
  attrs: Record<string, unknown>;
}

export interface ComposeChildSpec {
  id: string;
  definition: NodeDefinition;
  props?: Record<string, unknown>;
  connectTo?: string[];
}

// --- Native classes ---

export class Graph {
  constructor();
  readonly order: number;
  readonly size: number;
  nodes(): string[];
  addNode(id: string, attrs?: Record<string, unknown>): void;
  hasNode(id: string): boolean;
  removeNode(id: string): void;
  getNodeData(id: string): Record<string, unknown>;
  setNodeData(id: string, attrs: Record<string, unknown>): void;
  addEdge(u: string, v: string, key?: string, attrs?: Record<string, unknown>): string;
  hasEdge(u: string, v: string, key?: string): boolean;
  getEdgeData(u: string, v: string, key?: string): Record<string, unknown>;
  setEdgeData(u: string, v: string, key: string | null | undefined, attrs: Record<string, unknown>): void;
  removeEdge(u: string, v: string, key?: string): void;
  edgeKeys(u: string, v: string): string[];
  neighbors(id: string): string[];
  degree(id: string): number;
  edges(): EdgeTuple[];
  edgesOf(id: string): EdgeTuple[];
}

/// One Promise captured during an async round, paired with the slot id
/// the TS-side driver routes resolved cleanup functions back through.
export interface PendingAsyncEffect {
  promise: Promise<unknown>;
  cleanupId: number;
}

/// Result of one synchronous evaluation round, surfaced to the TS-side
/// async driver. `pendingPromises` is the set of in-flight
/// `useEffectAsync` Promises captured this round; `hasMoreWork` is true
/// iff resolution may produce more dirty state.
export interface AsyncRoundResult {
  pendingPromises: PendingAsyncEffect[];
  hasMoreWork: boolean;
}

/// Native scheduler — sync surface plus the round-based async protocol.
/// The exported `Scheduler` from `native.ts` extends this with
/// JS-implemented `startAsync` / `processBatchAsync` / `sendAsync` /
/// `stopAsync` methods that drive the multi-round cascade.
export class Scheduler {
  constructor(graph: Graph);
  start(): void;
  processBatch(): void;
  send(target: string, type: string, payload: unknown): void;
  stop(): void;
  hasNode(id: string): boolean;
  setMaxProcessDepth(max: number): void;
  readonly maxProcessDepth: number;
  validateGraph(): ValidationWarning[];
  // Live-graph query accessors (M6.6) — expose the scheduler's internal
  // graph for `useGraphQuery` and other introspection. The JS-held
  // `Graph` wrapper is moved-from at scheduler construction, so these
  // are the source of truth.
  graphNodes(): string[];
  graphNeighbors(id: string): string[];
  graphHasEdge(u: string, v: string, key?: string): boolean;
  graphGetNodeData(id: string): Record<string, unknown>;
  addNodeAtRuntime(
    id: string,
    definition: NodeDefinition,
    props?: Record<string, unknown>,
    connectTo?: string[],
  ): void;
  removeNodeAtRuntime(id: string): void;
  replaceComponent(id: string, definition: NodeDefinition): void;
  runStartRound(): AsyncRoundResult;
  runBatchRound(): AsyncRoundResult;
  runSendRound(target: string, type: string, payload: unknown): AsyncRoundResult;
  runStopRound(): AsyncRoundResult;
  installAsyncCleanup(cleanupId: number, cleanupFn: () => void): void;
  /// Install the cross-thread source-wakeup callback. The exported
  /// `Scheduler` subclass calls this in its constructor; consumers of
  /// the raw native class can install their own.
  installSourceWakeup(callback: () => void): void;
  registerNode(
    id: string,
    definition: NodeDefinition,
    props?: Record<string, unknown>,
    height?: number,
  ): void;
}

export class Signal {
  get(): unknown;
  set(value: unknown): void;
  update(updater: (current: unknown) => unknown): void;
}

export class Reducer {
  readonly state: unknown;
  dispatch(action: unknown): void;
}

export class MonitorRef {
  readonly id: number;
}

export class SpawnChildHandle {
  call(spec: ComposeChildSpec): void;
}

export class UnmountSelfHandle {
  call(): void;
}

export class ComposeHandle {
  call(specs: ComposeChildSpec[]): void;
}

export class AcceptEdgeHandle {
  call(targetId: string): void;
}

// --- Event sources (M6.6, Rust-native) ---
//
// Factories return opaque `EventSourceHandle` instances wrapping a
// Rust-side `Arc<dyn EventSource>` — JS only sees `id`.
// `useSource(handle, deps)` registers them; the source's start/stop
// run on the scheduler's tokio runtime. HTTP/watch are implemented
// Rust-side (tokio::net + notify) so they survive the M12 standalone
// CLI (no Node). Class is `EventSourceHandle` to avoid collision with
// Node 18+'s built-in `EventSource` global.

export class EventSourceHandle {
  readonly id: string;
}

export function timerSource(intervalMs: number): EventSourceHandle;
export function delaySource(delayMs: number): EventSourceHandle;
export function httpSource(port: number): EventSourceHandle;
export function watchSource(path: string): EventSourceHandle;

// --- Top-level functions ---

export function napiSmoke(): string;
export function version(): string;

export function useState(initial: unknown): Signal;
export function useReducer(reducer: (state: unknown, action: unknown) => unknown, initial: unknown): Reducer;
export function emit(type: string, payload: unknown, options?: EmitOptions): void;
export function useMessage(type: string, handler: (msg: IncomingMessage) => void): void;
export function useEffect(callback: () => (() => void) | void, deps: unknown[]): void;
export function useEffectAsync(callback: () => Promise<unknown>, deps: unknown[]): void;
export function useMemo(factory: () => unknown, deps: unknown[]): unknown;
// `useGraphQuery` is a JS-side wrapper in `native.ts`, not a native
// export. Its declaration lives there.
export function useCallback<F extends (...args: unknown[]) => unknown>(callback: F, deps: unknown[]): F;
export function useNeighbors(): string[];
export function useTopology(
  handler: (event: unknown) => void,
  filter?: { kinds?: string[]; neighborsOnly?: boolean },
): void;
/// Subscribe the active node to an event source. The source's `start`
/// runs on mount and on each dep change; `stop` runs before each
/// restart and on unmount. Source instances are reused across dep
/// changes (stop+start cycle).
export function useSource(source: EventSourceHandle, deps: unknown[]): void;

export function useMonitor(targetId: string): MonitorRef | null;
export function useDemonitor(monitor: MonitorRef): void;
export function useLink(targetId: string): boolean;
export function useUnlink(targetId: string): void;
export function useGroupJoin(group: string): void;
export function useGroupLeave(group: string): void;

export function useSpawnChild(): SpawnChildHandle;
export function useUnmountSelf(): UnmountSelfHandle;
export function useCompose(): ComposeHandle;
export function useAcceptEdge(): AcceptEdgeHandle;

// Algorithms (subset).
export function bfsNodes(graph: Graph, source: string): string[];
export function dfsNodes(graph: Graph, source: string): string[];
export function hasCycle(graph: Graph): boolean;
export function findCycle(graph: Graph): string[] | null;
export function connectedComponents(graph: Graph): string[][];
export function isConnected(graph: Graph): boolean;
export function numberOfConnectedComponents(graph: Graph): number;
export function shortestPath(graph: Graph, source: string, target: string): string[] | null;
export function areConnected(graph: Graph, u: string, v: string): boolean;
