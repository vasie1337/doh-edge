mod coalesce;
mod dns;
mod resolver_do;

pub use resolver_do::Resolver;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::sync::atomic::{AtomicU64, Ordering};
use worker::*;

const DNS_MSG: &str = "application/dns-message";
const DO_BINDING: &str = "RESOLVER";
const FETCHED_AT_HEADER: &str = "x-fetched-at";

// Per-isolate counters. These are *not* global: each edge machine runs multiple
// isolates, and each isolate restart resets the counts. Useful for eyeball
// debugging via response headers, not for real hit-rate measurement.
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

    // L1: per-edge Cache API.
    if let Some(mut cached) = cache.get(&cache_key, true).await? {
        let fetched_at = cached
            .headers()
            .get(FETCHED_AT_HEADER)?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or_else(now_ms);
        let mut bytes = cached.bytes().await?;
        let elapsed = ((now_ms() - fetched_at) / 1000.0).max(0.0) as u32;
        let (_, offsets) = dns::answer_ttl_info(&bytes);
        dns::decrement_ttls(&mut bytes, &offsets, elapsed);
        dns::rewrite_id(&mut bytes, client_id);
        let hits = HITS.fetch_add(1, Ordering::Relaxed) + 1;
        let misses = MISSES.load(Ordering::Relaxed);
        console_log!("L1-HIT  {qname} {qtype} age={elapsed}s");
        return finalize(bytes, hits, misses, None, "L1");
    }

    let misses = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    let hits = HITS.load(Ordering::Relaxed);
    console_log!("L1-MISS {qname} {qtype} region={region}");

    // L2: regional Durable Object. Falls back to direct upstream if binding missing.
    let (bytes, tier) = match fetch_via_do(&env, &region, &query).await {
        Ok((b, do_status)) => {
            let t = if do_status.as_deref() == Some("HIT") { "L2-HIT" } else { "L2-MISS" };
            (b, t)
        }
        Err(e) => {
            console_log!("DO error ({e}), falling back to upstream");
            (fetch_upstream(&query).await?, "UPSTREAM")
        }
    };
    let (ttl, _) = dns::answer_ttl_info(&bytes);

    let to_cache = build_response(bytes.clone(), Some(ttl))?;
    to_cache
        .headers()
        .set(FETCHED_AT_HEADER, &now_ms().to_string())?;
    cache.put(&cache_key, to_cache).await?;

    let mut out_bytes = bytes;
    dns::rewrite_id(&mut out_bytes, client_id);
    finalize(out_bytes, hits, misses, Some(ttl), tier)
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
) -> Result<(Vec<u8>, Option<String>)> {
    let ns = env.durable_object(DO_BINDING)?;
    let stub = ns.id_from_name(&format!("region:{region}"))?.get_stub()?;

    let headers = Headers::new();
    headers.set("content-type", DNS_MSG)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(wasm_bindgen::JsValue::from(
            js_sys::Uint8Array::from(query),
        )));
    let inner_req = Request::new_with_init("https://do/resolve", &init)?;
    let mut resp = stub.fetch_with_request(inner_req).await?;
    let status = resp.headers().get("x-do-status")?;
    let bytes = resp.bytes().await?;
    Ok((bytes, status))
}

async fn fetch_upstream(query: &[u8]) -> Result<Vec<u8>> {
    let headers = Headers::new();
    headers.set("accept", DNS_MSG)?;
    headers.set("content-type", DNS_MSG)?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(wasm_bindgen::JsValue::from(
            js_sys::Uint8Array::from(query),
        )));

    let upstream = Request::new_with_init("https://1.1.1.1/dns-query", &init)?;
    let mut resp = Fetch::Request(upstream).send().await?;
    resp.bytes().await
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn build_response(bytes: Vec<u8>, ttl: Option<u32>) -> Result<Response> {
    let mut resp = Response::from_bytes(bytes)?;
    let h = resp.headers_mut();
    h.set("content-type", DNS_MSG)?;
    if let Some(ttl) = ttl {
        h.set("cache-control", &format!("max-age={ttl}"))?;
    }
    Ok(resp)
}

fn finalize(
    bytes: Vec<u8>,
    hits: u64,
    misses: u64,
    ttl: Option<u32>,
    tier: &str,
) -> Result<Response> {
    let mut resp = build_response(bytes, ttl)?;
    let h = resp.headers_mut();
    h.set("x-cache-hits", &hits.to_string())?;
    h.set("x-cache-misses", &misses.to_string())?;
    h.set("x-cache-tier", tier)?;
    Ok(resp)
}
