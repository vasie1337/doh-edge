# doh-edge

DoH proxy on Cloudflare Workers. Forwards RFC 8484 queries (GET `?dns=` and POST `application/dns-message`) to `1.1.1.1`.

## Cache (tier 1: worker-local)

- Key: `https://doh-edge.cache/{qname}/{qtype}`, qname lowercased.
- QCLASS and EDNS(0) OPT records ignored for keying.
- TTL = min TTL across answer records, or 60s for empty/NXDOMAIN.
- On put: `Cache-Control: max-age=<ttl>` + `X-Fetched-At` stamp.
- On hit: decrement answer TTLs by elapsed seconds, rewrite TX ID to match request.
- `X-Cache-Hits` / `X-Cache-Misses` response headers are per-isolate, not global.
- `MISS`/`HIT` logged via `console_log!`.

## Cache (tier 2: regional Durable Object)

- One DO per continent. Worker routes L1 misses to `region:<continent>` (from `cf.continent`).
- DO state: in-memory `HashMap<(qname, qtype), (bytes, fetched_at, ttl)>`. No persistent storage.
- Request coalescing: N concurrent misses for the same key share 1 upstream fetch via a single-threaded async coalescer (`worker/src/coalesce.rs`, unit-tested).
- `X-Cache-Tier: L1 | L2-HIT | L2-MISS | UPSTREAM` response header.
- DO logs `DO-UPSTREAM <qname> <qtype>` only when the leader actually fires to `1.1.1.1`.

## Stale-while-revalidate (L2 only)

- Classify each entry as Fresh / StaleUsable / Expired. Stale window = `max(30s, ttl/10)`.
- StaleUsable: return stale bytes immediately, schedule a background refresh via `state.wait_until`.
- Refresh is coalesced via a `refresh_inflight` set — N concurrent stale hits → 1 upstream fetch.
- Expired: treated as a miss, blocks on upstream.
- Tier header: `L2-STALE` on stale-served responses.
- Logs: `L2-STALE-REFRESH-START` / `L2-STALE-REFRESH-DONE` / `L2-STALE-REFRESH-FAIL`.
- Debug headers (dev/testing): `x-debug-ttl-override: <secs>` on a MISS to force a short cache TTL; `x-debug-bypass-l1: 1` to skip the L1 cache; `x-debug-force-stale: 1` to classify as StaleUsable regardless of age.

## Observability

- Every query writes one event to the `doh_edge_queries` Analytics Engine dataset (fire-and-forget via `ctx.wait_until`).
- Schema: `index1 = qname`; `blob1..5 = tier, qtype_name, region, colo, rcode_name`; `double1..3 = latency_ms, upstream_latency_ms, response_bytes`. No client IPs.
- `/stats` renders an ASCII dashboard from 6 AE SQL queries (24h window). Auto-refreshes every 30s. ~30s ingestion lag is inherent to AE.
- Setup (one-time):
  ```
  npx wrangler secret put CLOUDFLARE_ACCOUNT_ID   # paste account id from dashboard
  npx wrangler secret put ANALYTICS_API_TOKEN     # paste token with Account Analytics: Read
  ```
