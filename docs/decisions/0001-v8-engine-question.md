# 0001 — Do we actually need V8?

**Status:** DECIDED — option (i) soft-demote. Recorded 2026-05-01; decision flipped 2026-05-03; M17 third-engine slot reshaped from JVM to Go on 2026-06-15 (see Amendment below).
**Related:** Strategic plan at `~/.claude/plans/graphreact-rust-later-milestones.md` (replaces the lost `toasty-crafting-lantern.md`; reconstructed 2026-06-15) no longer features V8 as a first-party M17 deliverable. M12.3 is JSC-via-Zig (decided 2026-05-01; see `~/.claude/projects/.../memory/project_graphreact_m12_jsc_decision.md`).

## Amendment (2026-06-15) — M17 third-engine slot is Go, not JVM

The "JVM as primary first-party deliverable" framing below remains the historical record of the 2026-05-03 decision. As of 2026-06-15, the M17 third-engine slot has been pivoted to **Go** (cgo + c-shared, build-step codegen `.grx` → Go). The "JVM" wording in the Decision and Reasoning sections below is preserved for historical fidelity; **ADR-0005 will record the Java→Go pivot in full** and supersede the JVM commitment. See [`~/.claude/plans/graphreact-rust-later-milestones.md`](/Users/damioludiran/.claude/plans/graphreact-rust-later-milestones.md) §M17 for the current authoritative scope.

## Decision (2026-05-03)

**Soft-demote V8 — option (i).** GraphReact's engine-trait remains engine-agnostic so anyone can implement `graphreact-engine-v8` externally; we do not commit to shipping it as a first-party binary. M17's "additional engines" milestone re-shapes around **JVM as the primary first-party deliverable** (Kotlin/Scala/Clojure authorship — genuinely additive, not redundant with JSC). V8 is dropped from the named first-party engine list.

**Reasoning** (extends the 2026-05-01 recommendation):

1. **JSC-via-vendored-WebKit covers all 5 platforms.** The pivot rationale at M12.3 was "JSC handles JS just as well as V8 and avoids V8's tax." M12.3a (macOS system framework) + M12.3b (vendored WebKit for Linux/Windows) means the cross-platform-JS argument for V8 evaporates.
2. **Node-via-NAPI gives users V8 access today** (M6 / M6.5 path). A user who specifically needs V8 (e.g. for a Node-ecosystem dependency that won't work elsewhere) can embed graphreact-napi into a Node host and get V8 transitively.
3. **Phase 1.5's symbol-shim approach (M12.3b-phase1.5)** proves the JSC C-API + extern-C-shim model is robust enough that we don't need V8 as a hedge against JSC ABI fragility.
4. **Bun is converging on production-grade JSC.** WebKit is well-funded by Apple. Engine risk is low.
5. **Maintenance tax is real and recurring.** V8 ships every 6 weeks; pinning + retesting is forever-ongoing. Saving that tax frees engineering time for tracks that actually expand the polyglot story (JVM, Lua, .NET).
6. **M12.4 (Node-API compat for JSC bodies) — committed same day, 2026-05-03 — closes the original V8 motivation.** "JS-ecosystem reach" was always understood as backend NPM packages (Express, Prisma, ioredis, pg, AWS SDK, etc.). Pre-M12.4, neither JSC nor first-party V8 actually delivered NPM access without significant additional work. M12.4 commits to delivering that on JSC by reversing the M12.3 fork-and-strip exclusion of Bun's Node-compat layer. **After M12.4 ships, JSC + GraphReact = full backend NPM ecosystem access** — the original V8 framing's actual content. V8 becomes redundant rather than just maintenance-expensive. This reinforces the soft-demote, not weakens it. See [decision 0002 addendum 2026-05-03](0002-graphreact-is-backend-only.md) for the architectural framing.

## What "soft-demote" means in practice

- **Engine-trait stays engine-agnostic.** `graphreact-engine-zig`'s trait surface remains designed for any embedder. A community contributor can implement V8 against it without needing core changes.
- **No `crates/graphreact-engine-v8/` first-party crate.** The workspace structure list drops the placeholder. (If a community implementation ships, it lives in a sibling repo.)
- **No `grx-v8` binary.** The `--runtime=` flag won't list V8 in core builds.
- **CDP / debugger story routes through JSC** for M16. Bun's `Debugger.zig` is the worked reference (already cited in M16 plan).
- **M17 re-shapes** to **JVM-primary** + stretch (Lua via LuaJIT, .NET via CoreCLR). Same per-engine-binary pattern; different language ecosystem reach.

## When to revisit

The soft-demote is reversible. Triggers that would warrant flipping back:

- **Hard JSC regression** that takes >1 month to work around. Unlikely given Apple's engineering investment, but watch.
- **Real user-with-budget** asking for V8 specifically — not "would be nice", but "we'll pay for the maintenance commitment." If that materialises, pull V8 forward as a paid track or guide them to community-implement against the engine-trait.
- **A specific V8-only capability** emerging that JSC + JVM can't cover (e.g. some Node-ecosystem-only debugger feature). Hard to predict; would surface organically.

## What is NOT being decided here

- **JSC-via-Zig at M12.3.** Locked in 2026-05-01.
- **Engine-trait architecture.** Locked in M12.1.
- **JVM at M17.** Genuinely additive; remains scheduled.
- **B3 (Rust runtime path), Zig as bridge, per-engine binaries.** All locked in (rearchitecture memory 2026-04-30).
- **NAPI / M6 / M6.5 path.** Existing Node-embed users keep V8 access through napi-rs.

---

## Original options (preserved for context)

## The question

Given that JSC is the M12.3 engine and runs JS just as well as V8, do we
need V8 at all? M17 currently commits to maintaining V8 as a first-party
engine binary. Is that commitment worth its cost?

## What V8 has that JSC doesn't (for our use case)

- **Mindshare.** People know V8 because Node uses it. Marketing checkbox.
- **Chrome DevTools Protocol depth.** V8's CDP implementation is the
  most polished. But Bun proves you can wire CDP up to JSC too
  (`bun --inspect` opens a CDP-compatible debugger). Harder, not
  impossible.
- **Optimization on certain workloads.** TurboFan vs FTL — different
  shapes. JSC tends to be equal-or-faster on small workloads; V8 tends
  to be ahead on long-running optimization-heavy code. For
  graph-reactive backend code (lots of small message handlers, not
  numerics), JSC's profile is arguably *better* aligned.
- **Server-Linux familiarity.** People run Node on Linux. But Bun runs
  JSC on Linux fine.

## What V8 doesn't add (for us)

- **Running user JS** — JSC does that.
- **Component lifecycle, microtask integration, snapshot/bytecode
  startup, async semantics** — engine-trait abstracts all this; both
  engines satisfy it.
- **Cross-platform reach** — WebKit/JSC builds on macOS, Linux,
  Windows.

## Cost of "M17 V8"

- V8 ships every 6 weeks → engine-version-churn maintenance forever.
- ~60 MB binary, separate per-platform builds → distribution matrix
  doubles.
- WebKit/V8 source vendoring × build infra × CI permutations → real
  toil.

V8 at M17 is essentially: "we will commit to indefinitely maintaining a
60 MB engine binary we don't strictly need, primarily because some
users will recognize the name." Real cost, marginal value.

## Three options

### (i) Soft-demote — engine-trait supports V8, not first-party shipped

Document that anyone can build `graphreact-engine-v8` externally — the
trait is engine-agnostic. We don't promise to ship a V8 binary. M17's
slot stays "additional engines" but the named candidates become V8
(community), JVM (first-party), Lua, .NET. If real user demand for V8
specifically materializes later, pull it forward.

### (ii) Drop V8 entirely from the named milestone list

M17 becomes "JVM + Lua + .NET" or just "additional non-JS engines." If
anyone asks "what about V8?", point at the engine-trait and
Node-via-NAPI (which IS V8 in Node, accessible today through M6).

### (iii) Keep as-is

Maintain the V8 commitment. Pay the maintenance cost. Frame as
"hedging engine risk" — if JSC ever hits a hard regression, V8 is a
fallback.

## Recommendation (Claude, 2026-05-01) — adopted 2026-05-03

Lean **(i)**. Architectural support for V8 without the maintenance
promise. Frees M17's actual engineering time for additive engines
(JVM unlocks Kotlin/Scala/Clojure authorship; Lua unlocks LuaJIT's
niche; both are different from anything JSC provides).

Strongest argument *against* (i) is hedging engine risk. Counter:
WebKit is well-funded by Apple, Bun is reaching production-grade on
JSC, and Node-via-NAPI gives users V8 access today through M6 if they
need it. Fallback risk is low.

**This recommendation was adopted on 2026-05-03.** See decision block
at the top of this file.
