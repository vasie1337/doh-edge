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

## observability: Analytics Engine

**AE over D1.** D1 is the tutorial answer but wrong here. The workload is append-only, high-volume, write-once-query-later telemetry. AE is purpose-built for that: writes are cheap (10M/day free tier, well under our traffic), aggregations run on ClickHouse over time windows. D1 would bottleneck on write and get expensive on the aggregations.

**Log everything, no sampling.** AE's adaptive sampling on the index (qname) preserves percentile accuracy on popular names. Sampling at write time would trade marginal cost for worse p99s. Not worth it.

**30s ingestion lag is the accepted tradeoff.** AE writes land in the SQL API after ~30s. The /stats page is "always 30s behind" — fine for a status dashboard, documented so it doesn't look like a bug.

**What we don't log.** No client IPs, even truncated. The DNS query itself is sensitive enough; leaking the pair (client, query) defeats the point of running a resolver you trust. No per-user rate limits or identification either. Retention is AE's default (~90 days); no explicit policy layered on top.

### observations

- L1 per-isolate counters bounce between requests because consecutive requests land on different isolates. Not a bug — property of the runtime.
- 204s → 197s TTL decrement across an 8s sleep confirmed in prod (example.com, A).
- Coalescing verified in prod: 15 concurrent requests for a fresh qname → 10 L1-MISSes land on the DO → exactly **1** `DO-UPSTREAM` log per batch (across 3 batches, 3 upstream fetches total for 45 requests).
- SWR verified in prod: prime with `x-debug-ttl-override: 3`, wait 4s, fire 20 concurrent bypass-L1 requests → **1** `DO-UPSTREAM` refresh, **1** `L2-STALE-REFRESH-START/DONE` pair.
- Real-traffic L2-HIT rate is near zero even though the DO caches correctly in isolation (6-probe test: 1 MISS, 5 HITs). The pattern: L1 (per edge machine, shared across isolates on that machine) absorbs all repeats from a sticky client, so the DO is rarely queried while still fresh for a given key. L1 and the DO are populated with the same TTL at the same time, so they tend to evict together — L1-miss usually coincides with DO-cold, producing L2-MISS, not L2-HIT. The situation that produces L2-HIT is "edge machine B sees a key that edge machine A already populated into the DO," which requires multi-user/multi-colo traffic to be common. Under a single-user workload this path is rare. Not a bug — a consequence of the cache topology plus traffic pattern.
- `upstream_ms` (actual fetch to 1.1.1.1) and `l2_roundtrip_ms` (full DO call) are now tracked separately via an `x-upstream-ms` header returned by the DO. Cold probe: l2_ms=14, upstream_ms=4 → DO RTT overhead ~10ms. Warm probe: l2_ms=6, upstream_ms=0.
- The `UPSTREAM` tier is a defensive fallback that only fires if the DO binding is missing/broken. Not expected in normal operation. `L2-MISS` is the "DO had to fetch upstream" path; `L2-HIT` is "DO served from its own memory cache."
