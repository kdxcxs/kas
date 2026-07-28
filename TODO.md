# Performance follow-ups

- Replace the process-wide `Mutex<Store>` and synchronous database calls with an async-aware store facade and a bounded connection pool.
- Support bounded multi-inflight or batched Driver deliveries with explicit backpressure instead of one delivery per connection.
- Avoid rewriting complete Resource JSON documents when only one Driver observation changes; use native partial JSON updates where the backend supports them.
- Cache compiled RBAC decisions and invalidate them incrementally when Role or role-binding Link Resources change.
- Move PostgreSQL to native `jsonb` storage and indexes, and use a real connection pool rather than the compatibility worker.
- Add retention and pagination policies for the append-only `events` table.
- Reduce relationship-driver selector work with compiled selectors and endpoint/Relation-specific indexes.
- Extend the end-to-end benchmark so convergence measures every expected Driver fanout, not only owning-Driver completion.
- Make benchmark interruption terminate the complete child process group and always write a partial report on failure or cancellation.
- Add per-phase benchmark timeouts plus queue-depth, delivery-latency, database-query, and reconcile-throughput metrics.
- Renew long-running benchmark Driver credentials (or configure a benchmark-scoped TTL) so scale runs cannot stall after the current one-hour credential lifetime.
