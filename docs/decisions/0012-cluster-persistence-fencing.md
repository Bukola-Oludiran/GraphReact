# 0012 — Cluster persistence uses immutable S3 objects and Postgres fencing

**Status:** Accepted — Phase A, 2026-08-13

## Context

Phase A requires active/passive recovery without allowing a paused or partitioned
former owner to commit after a standby takes over. Runtime blocks, snapshots,
deployment bundles, and artifact bytes are large immutable values; leases, heads,
indexes, and idempotency decisions require strongly consistent transactions.

## Decision

1. S3-compatible storage owns immutable runtime blocks, snapshots, bundles, and
   content-addressed artifact bytes. Postgres owns the deployment row, lease,
   monotonically increasing fencing token, runtime and snapshot heads, block
   index, and artifact metadata.
2. Every lease acquisition increments the fencing token. Renewal preserves it.
   A commit or snapshot update succeeds only while that token is current and its
   lease has not expired.
3. Writers upload the immutable object first, then advance the Postgres head in
   one transaction using both the expected prior sequence and fencing token as a
   compare-and-swap. The block index is inserted in the same transaction.
4. A failed compare-and-swap leaves an unreferenced object. Replay follows only
   Postgres-indexed objects, so such orphans are harmless and may be garbage
   collected later. Phase A does not delete them automatically.
5. Recovery verifies object SHA-256, deployment digest, format version, sequence
   range, and recorded fencing token before decoding records. Missing referenced
   objects or verification failures fail closed.
6. S3 credentials come only from the standard AWS environment/workload identity
   chain. Database URLs and credential-bearing values are redacted from `Debug`.

## Consequences

- Postgres is the linearization point; object storage never decides writer
  ownership.
- Takeover does not need to find or delete uploads from a former owner before
  becoming ready.
- The daemon polls for ownership before mounting, renews before scheduler work,
  and stops admission, effect dispatch, and scheduler progress on lease loss.
- Service-level conformance still requires Postgres and MinIO fixtures in CI;
  deterministic in-memory tests cover fencing and orphan safety on every build.
