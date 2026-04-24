mod coalesce;
mod dns;
mod http;
mod resolver_do;

pub use resolver_do::Resolver;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http::{dns_response, fetch_upstream, now_ms};
use std::sync::atomic::{AtomicU64, Ordering};
use worker::*;

const DO_BINDING: &str = "RESOLVER";
const FETCHED_AT_HEADER: &str = "x-fetched-at";

// Per-isolate counters. Not global: each edge machine runs multiple isolates,
// and each isolate restart resets the counts. Useful for eyeball debugging via
// response headers, not for real hit-rate measurement.
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let region = region_from(&req);

    let query: Vec<u8> = match req.method() {
        Method::Get => {
            let url = req.url()?;
            let Some((_, dns)) = url.query_pairs().find(|(k, _)| k == "dns") else {
                return Response::error("missing ?dns=", 400);
            };
            URL_SAFE_NO_PAD
                .decode(dns.trim_end_matches('=').as_bytes())
                .map_err(|e| Error::RustError(format!("bad base64url: {e}")))?
        }
        Method::Post => req.bytes().await?,
        _ => return Response::error("method not allowed", 405),
    };
    if query.len() < 12 {
        return Response::error("short DNS message", 400);
    }

    let client_id = dns::read_id(&query);
    let Some((qname, qtype)) = dns::parse_question(&query) else {
        return Response::error("unparseable dns query", 400);
    };
    let cache_key = format!("https://doh-edge.cache/{qname}/{qtype}");
    let cache = Cache::default();

    let force_stale = req.headers().get("x-debug-force-stale")?.is_some();
    let ttl_override = req.headers().get("x-debug-ttl-override")?;
    let bypass_l1 = req.headers().get("x-debug-bypass-l1")?.is_some()
        || force_stale
        || ttl_override.is_some();

    // L1: per-edge Cache API. Skipped when a debug header is set so the
    // request exercises the DO path where SWR lives.
    if !bypass_l1 {
        if let Some(mut cached) = cache.get(&cache_key, true).await? {
            let fetched_at = cached
                .headers()
                .get(FETCHED_AT_HEADER)?
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_else(now_ms);
            let mut bytes = cached.bytes().await?;
            let elapsed = ((now_ms() - fetched_at) / 1000.0).max(0.0) as u32;
            dns::apply_cache_hit(&mut bytes, client_id, elapsed);
            let hits = HITS.fetch_add(1, Ordering::Relaxed) + 1;
            let misses = MISSES.load(Ordering::Relaxed);
            console_log!("L1-HIT  {qname} {qtype} age={elapsed}s");
            return finalize(bytes, hits, misses, None, "L1");
        }
    }

    let misses = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    let hits = HITS.load(Ordering::Relaxed);
    console_log!("L1-MISS {qname} {qtype} region={region} force_stale={force_stale}");

    // L2: regional Durable Object. Falls back to direct upstream on DO error.
    let (bytes, tier) = match fetch_via_do(&env, &region, &query, force_stale, ttl_override.as_deref()).await {
        Ok((b, Some(s))) => (b, match s.as_str() {
            "HIT" => "L2-HIT",
            "STALE" => "L2-STALE",
            _ => "L2-MISS",
        }),
        Ok((b, None)) => (b, "L2-MISS"),
        Err(e) => {
            console_log!("DO error ({e}), falling back to upstream");
            (fetch_upstream(&query).await?, "UPSTREAM")
        }
    };
    let (ttl, _) = dns::answer_ttl_info(&bytes);

    let to_cache = dns_response(bytes.clone())?;
    let h = to_cache.headers();
    h.set("cache-control", &format!("max-age={ttl}"))?;
    h.set(FETCHED_AT_HEADER, &now_ms().to_string())?;
    cache.put(&cache_key, to_cache).await?;

    let mut out = bytes;
    dns::rewrite_id(&mut out, client_id);
    finalize(out, hits, misses, Some(ttl), tier)
}

fn region_from(req: &Request) -> String {
    req.cf()
        .and_then(|cf| cf.continent())
        .unwrap_or_else(|| "UNKNOWN".to_string())
}

async fn fetch_via_do(
    env: &Env,
    region: &str,
    query: &[u8],
    force_stale: bool,
    ttl_override: Option<&str>,
) -> Result<(Vec<u8>, Option<String>)> {
    let ns = env.durable_object(DO_BINDING)?;
    let stub = ns.id_from_name(&format!("region:{region}"))?.get_stub()?;
    let init = http::dns_post_init(query)?;
    if force_stale {
        init.headers.set("x-debug-force-stale", "1")?;
    }
    if let Some(v) = ttl_override {
        init.headers.set("x-debug-ttl-override", v)?;
    }
    let inner_req = Request::new_with_init("https://do/resolve", &init)?;
    let mut resp = stub.fetch_with_request(inner_req).await?;
    let status = resp.headers().get("x-do-status")?;
    Ok((resp.bytes().await?, status))
}

fn finalize(
    bytes: Vec<u8>,
    hits: u64,
    misses: u64,
    ttl: Option<u32>,
    tier: &str,
) -> Result<Response> {
    let resp = dns_response(bytes)?;
    let h = resp.headers();
    if let Some(ttl) = ttl {
        h.set("cache-control", &format!("max-age={ttl}"))?;
    }
    h.set("x-cache-hits", &hits.to_string())?;
    h.set("x-cache-misses", &misses.to_string())?;
    h.set("x-cache-tier", tier)?;
    Ok(resp)
}
