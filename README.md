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
