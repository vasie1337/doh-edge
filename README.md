# doh-edge

Edge DNS-over-HTTPS resolver on Cloudflare Workers. Rust.

- endpoint: https://dns.vasie.dev/dns-query
- stats: https://dns.vasie.dev/stats

## what it does

- RFC 8484 DoH (GET `?dns=` and POST `application/dns-message`)
- L1 cache: Workers Cache API, per edge machine
- L2 cache: one Durable Object per continent, in-memory
- request coalescing at L2
- stale-while-revalidate at L2, window = max(30s, ttl/10)
- upstream is 1.1.1.1
- every query logged to Analytics Engine, dashboard at `/stats`

## response headers

- `x-cache-tier`: `L1`, `L2-HIT`, `L2-STALE`, `L2-MISS`
- `x-upstream-ms`: fetch time to 1.1.1.1 (0 on hit)
- `x-l2-ms`: full DO roundtrip (0 on L1)
- `x-cache-hits` / `x-cache-misses`: per-isolate counters

## cache key

`(qname, qtype)`, qname lowercased. QCLASS and EDNS(0) OPT are ignored. TTL is min TTL across answer records, 60s for NXDOMAIN.

## debug headers

- `x-debug-bypass-l1: 1`: skip L1, go straight to the DO
- `x-debug-ttl-override: <n>`: on a miss, store with TTL = n seconds
- `x-debug-force-stale: 1`: force StaleUsable classification

## setup

```
cd worker
npx wrangler deploy
npx wrangler secret put CLOUDFLARE_ACCOUNT_ID
npx wrangler secret put ANALYTICS_API_TOKEN   # Account Analytics: Read
```

## tests

```
cd worker && cargo test --lib
```

Covers the coalescer in `worker/src/coalesce.rs`.
