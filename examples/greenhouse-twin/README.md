# Digital Greenhouse Twin

This deterministic Go example makes the `.grx` graph the topology, executable
automation, safety map, and observer source for a greenhouse simulation. It is
a demonstration and fault-testing model, not a safety-certified controller.

## Run the system

From the runtime repository:

```sh
cargo run -p graphreact-cli --bin grx -- cluster daemon \
  --dev-loopback --path examples/greenhouse-twin/greenhouse.grx --runtime go
```

`--dev-loopback` is restricted to a single loopback node. It creates an
ephemeral cookie and mTLS identity in memory and starts the observer on
`127.0.0.1:9090`. Production cluster flags remain mandatory without it.

For the complete browser showcase, from the sibling
`graphreact-visualizer` repository run:

```sh
npm run demo:greenhouse
```

The launcher builds `grx` when needed, exports a source-free topology asset,
starts the daemon, waits for readiness, and serves the UI. Prerequisites are
Node 20+, Go, and Rust.

## Three-minute story

1. Select **Stuck irrigation valve**.
2. Step five simulated five-minute intervals, or press **Run**.
3. Soil drops below 30%, so normal automation opens irrigation.
4. At 36%, the controller's ordinary close command is ignored.
5. After three complete open ticks, the independent safety edge trips,
   force-closes and locks out the valve, and protects `crop-zone-a`.

The five versioned external call pairs are declared only by `twin-gateway`:
`get_snapshot/twin_snapshot`, `set_scenario/scenario_set`,
`set_clock/clock_set`, `step/step_complete`, and `reset/reset_complete`.
Observer telemetry continues to omit payloads; the UI reads domain state only
through `get_snapshot`.

The standard-library-only core works without a network. The separate
`../greenhouse-twin-ai` example optionally explains sanitized snapshots with
the OpenAI Responses API and has no route back to greenhouse controls.
