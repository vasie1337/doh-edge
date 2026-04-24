# design

## caching

Two tiers.

L1 is a Workers Cache API entry keyed on `https://doh-edge.cache/{qname}/{qtype}`. Per edge machine. Sub-ms lookup. Honors `Cache-Control: max-age` so eviction is automatic.

L2 is a Durable Object per continent, keyed by `region:<cf.continent>`. State is `HashMap<(qname, qtype), Entry>` in memory.

On hit at either tier: decrement answer TTLs by elapsed time, rewrite transaction ID to match the caller.

Why not:

- KV for L1: eventually consistent, tens of ms, slower than the upstream.
- DO for L1: every read is a network hop.
- In-isolate memory for L1: no eviction, no sharing across isolates, short lifetime.
- Global DO: best coalescing, worst latency, kills the edge point.
- DO per colo: no coalescing, splinters the cache.
- DO per region: chosen. 3-6 instances, 20-50 ms RTT.
- Persistent DO storage: DNS TTLs are short, warm-start value is marginal.

## coalescing

The DO's real job. `RefCell<HashMap<Key, Vec<oneshot::Sender<V>>>>`. First caller inserts an empty Vec (the "handling this" marker) and runs the fetch. Subsequent callers push a Sender and await. Leader broadcasts on completion.

Lookup and insert must be synchronous (no `.await` between) or two callers both decide they're the leader.

Unit-tested in `worker/src/coalesce.rs` (5 cases). Correctness here is load-bearing.

## stale-while-revalidate

L2 only. Window = max(30s, ttl/10).

Classify each entry as Fresh, StaleUsable, or Expired. StaleUsable returns old bytes immediately and schedules a background refresh via `state.wait_until`. Refresh scheduling is deduped by a `refresh_inflight: HashSet<Key>` so concurrent stale hits produce one refresh, not N.

If the DO is evicted mid-refresh the future may not complete. Acceptable for DNS.

## analytics: AE over D1

Workload is append-only, high volume, aggregated over time windows. AE fits; D1 bottlenecks on writes and is expensive on aggregations. Free tier is 10M writes/day, well above real traffic.

Schema per event:

- `index1`: qname
- `blob1..5`: tier, qtype_name, region, colo, rcode_name
- `double1..4`: latency_ms, upstream_ms, response_bytes, l2_roundtrip_ms

Writes are fire-and-forget via `ctx.wait_until`. A failed AE write does not affect the DNS response.

`/stats` queries the AE SQL API (ClickHouse dialect, `quantileWeighted(level)(col, _sample_interval)` for percentiles). Ingestion lag is ~30s, so the dashboard is that far behind.

No client IPs logged. No sampling at write time.

## observations

Single-region, single-user:

- 15 concurrent requests for a fresh qname: 10 L1-MISSes at the DO, 1 upstream fetch. Across 3 batches: 3 upstream fetches for 45 requests.
- SWR: prime with ttl=3, wait 4s, fire 20 concurrent bypass-L1 requests: 1 refresh.
- Tier distribution was ~35% L1 / ~65% L2-MISS, L2-HIT ~0, L2-STALE 0. DO caches correctly in isolation (6-probe burst: 1 MISS, 5 HITs). L1 absorbs repeats from sticky routing, so the DO is rarely asked for anything it already has. L1 and DO are populated at the same instant with the same TTL, so they tend to evict together.

Multi-region load (4 Fly VMs, IAD/NRT/GRU/SIN, 863 queries / 24h):

- Cache hit rate 71.7%. L1 53.8%, L2-HIT 8.2%, L2-STALE 9.7%, L2-MISS 28.3%.
- L2-MISS p99 607ms but upstream p95 only 45ms. The tail is DO roundtrip plus coalescing wait, not 1.1.1.1.
- L2-HIT p95 562ms: 6 of 8 slow HITs were in SIN (27 total requests). Low-traffic regions let the DO hibernate; waking it costs most of the tail. IAD (422 reqs) runs in the normal range.

Other:

- `x-cache-hits`/`x-cache-misses` headers are per-isolate. Real numbers are in AE.
- TTL decrement on the wire: 204s to 197s across an 8s sleep on `example.com A`.
