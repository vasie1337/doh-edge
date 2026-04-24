mod dns;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::sync::atomic::{AtomicU64, Ordering};
use worker::*;

const UPSTREAM: &str = "https://1.1.1.1/dns-query";
const DNS_MSG: &str = "application/dns-message";

// Per-isolate counters. These are *not* global: each edge machine runs multiple
// isolates, and each isolate restart resets the counts. Useful for eyeball
// debugging via response headers, not for real hit-rate measurement — that
// needs aggregation via D1 / a Durable Object.
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

const FETCHED_AT_HEADER: &str = "x-fetched-at";

#[event(fetch)]
async fn fetch(mut req: Request, _env: Env, _ctx: Context) -> Result<Response> {
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

    let (qname, qtype) = match dns::parse_question(&query) {
        Some(q) => q,
        None => {
            // unparseable — forward without caching
            return forward(&query, None).await;
        }
    };
    let cache_key = format!("https://doh-edge.cache/{qname}/{qtype}");
    let cache = Cache::default();

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
        console_log!("HIT  {qname} {qtype} age={elapsed}s");
        return finalize(bytes, hits, misses, None);
    }

    let misses = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    let hits = HITS.load(Ordering::Relaxed);

    let bytes = fetch_upstream(&query).await?;
    let (ttl, _) = dns::answer_ttl_info(&bytes);
    console_log!("MISS {qname} {qtype} ttl={ttl}s");

    let to_cache = build_response(bytes.clone(), Some(ttl))?;
    to_cache
        .headers()
        .set(FETCHED_AT_HEADER, &now_ms().to_string())?;
    cache.put(&cache_key, to_cache).await?;

    finalize(bytes, hits, misses, Some(ttl))
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

async fn forward(query: &[u8], ttl: Option<u32>) -> Result<Response> {
    let bytes = fetch_upstream(query).await?;
    build_response(bytes, ttl)
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

    let upstream = Request::new_with_init(UPSTREAM, &init)?;
    let mut resp = Fetch::Request(upstream).send().await?;
    resp.bytes().await
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

fn finalize(bytes: Vec<u8>, hits: u64, misses: u64, ttl: Option<u32>) -> Result<Response> {
    let mut resp = build_response(bytes, ttl)?;
    let h = resp.headers_mut();
    h.set("x-cache-hits", &hits.to_string())?;
    h.set("x-cache-misses", &misses.to_string())?;
    Ok(resp)
}
