# design

## tier 1: Workers Cache API

- Per-edge, sub-ms lookup. Hit never leaves the machine.
- Honors `Cache-Control: max-age`, so eviction is automatic.
- Not shared across edge machines. Fine — misses fall through to tier 2.

Alternatives:

- **KV** — eventually consistent, ~tens of ms. Slower than the upstream it's meant to shortcut.
- **DO** — network round-trip per read. Right for tier 2 (shared state), wrong for the hot path.
- **In-isolate memory** — no eviction, no sharing, short lifetime. OK for counters, not the cache.

## tier 2: Durable Object (regional resolver)

Primary job: **request coalescing**. N concurrent misses for the same key ⇒ 1 upstream query. Caching is a secondary benefit.

### routing

One DO per region. Name = `region:<region>`, derived from `request.cf.continent` (e.g. `EU`, `NA`, `AS`). Worker picks the DO on L1 miss.

- **Global DO** — best coalescing, worst latency. Kills the edge pitch.
- **DO per colo** — zero extra latency, no coalescing. Splinters cache.
- **DO per region** — ~3–6 DOs. ~20–50ms to regional DO. Chosen default.
- **DO per (region, key-hash)** — scales for hot keys. v2.

### storage

Memory only (`HashMap<String, (Bytes, Instant)>`). DNS TTLs are short; warm-start value is marginal vs. the complexity of `state.storage` write-through. v2 if needed.

### coalescing

DO runtime is single-threaded-async per instance, so no real locks. Pattern:

- `RefCell<HashMap<Key, Vec<oneshot::Sender<Bytes>>>>`
- First caller for a key inserts empty Vec (the "I'm handling this" marker), fires upstream.
- Subsequent callers push a Sender, await its Receiver.
- On completion: lock, drain waiters, send bytes to all.
- Lookup→insert must be synchronous (no `.await` between) or two handlers race as "first".

Unit-test the coalescer in isolation.

### non-goals for this slice

Persistent storage, prefetching, cross-DO metrics, multi-upstream failover. Each is its own slice.

### stale-while-revalidate

At the DO only. L1 skips SWR — already sub-ms, not the bottleneck.

- Classify: Fresh / StaleUsable / Expired. Stale window = `max(30s, ttl/10)`.
- StaleUsable returns stale bytes and schedules a background `state.wait_until` refresh.
- Refresh is coalesced via a `refresh_inflight` HashSet (separate from the request coalescer): the key is inserted before scheduling, removed when the refresh completes. Concurrent StaleUsable requests see the key and skip.
- If the DO is evicted mid-refresh, the `wait_until` future may not complete — acceptable loss for DNS.

### observations

- L1 per-isolate counters bounce between requests because consecutive requests land on different isolates. Not a bug — property of the runtime.
- 204s → 197s TTL decrement across an 8s sleep confirmed in prod (example.com, A).
- Coalescing verified in prod: 15 concurrent requests for a fresh qname → 10 L1-MISSes land on the DO → exactly **1** `DO-UPSTREAM` log per batch (across 3 batches, 3 upstream fetches total for 45 requests).
- SWR verified in prod: prime with `x-debug-ttl-override: 3`, wait 4s, fire 20 concurrent bypass-L1 requests → **1** `DO-UPSTREAM` refresh, **1** `L2-STALE-REFRESH-START/DONE` pair.
