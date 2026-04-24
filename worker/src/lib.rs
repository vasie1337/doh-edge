mod coalesce;
mod dns;
mod http;
mod metrics;
mod resolver_do;
mod stats;

pub use resolver_do::Resolver;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http::{dns_response, fetch_upstream, now_ms};
use std::sync::atomic::{AtomicU64, Ordering};
use worker::*;

const DO_BINDING: &str = "RESOLVER";
const FETCHED_AT_HEADER: &str = "x-fetched-at";

// Per-isolate counters. Not global: each edge machine runs multiple isolates,
// and each isolate restart resets the counts. Useful for eyeball debugging via
// response headers, not for real hit-rate measurement (see Analytics Engine).
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    let url = req.url()?;
    if url.path() == "/stats" {
        return stats::render(&env).await;
    }
    handle_dns(&mut req, &env, &ctx).await
}

async fn handle_dns(req: &mut Request, env: &Env, ctx: &Context) -> Result<Response> {
    let start_ms = now_ms();
    let region = region_from(req);
    let colo = req
        .cf()
        .map(|cf| cf.colo())
        .unwrap_or_else(|| "UNKNOWN".to_string());

    let query = match req.method() {
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

    // L1: per-edge Cache API. Bypassed when a debug header is set.
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
            let resp = finalize(bytes.clone(), hits, misses, None, "L1")?;
            emit_event(env, ctx, &qname, qtype, "L1", &region, &colo, &bytes, start_ms, 0.0, 0.0);
            return Ok(resp);
        }
    }

    let misses = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    let hits = HITS.load(Ordering::Relaxed);
    console_log!("L1-MISS {qname} {qtype} region={region}");

    // L2: regional DO (fallback: direct upstream on DO error — not expected in prod).
    let l2_start = now_ms();
    let (bytes, tier, upstream_ms) = match fetch_via_do(env, &region, &query, force_stale, ttl_override.as_deref()).await {
        Ok((b, Some(s), u_ms)) => (b, match s.as_str() {
            "HIT" => "L2-HIT",
            "STALE" => "L2-STALE",
            _ => "L2-MISS",
        }, u_ms),
        Ok((b, None, u_ms)) => (b, "L2-MISS", u_ms),
        Err(e) => {
            console_log!("DO error ({e}), falling back to upstream");
            let up_start = now_ms();
            let b = fetch_upstream(&query).await?;
            (b, "UPSTREAM", now_ms() - up_start)
        }
    };
    let l2_roundtrip_ms = now_ms() - l2_start;
    let (ttl, _) = dns::answer_ttl_info(&bytes);

    let to_cache = dns_response(bytes.clone())?;
    let h = to_cache.headers();
    h.set("cache-control", &format!("max-age={ttl}"))?;
    h.set(FETCHED_AT_HEADER, &now_ms().to_string())?;
    cache.put(&cache_key, to_cache).await?;

    let mut out = bytes.clone();
    dns::rewrite_id(&mut out, client_id);
    let resp = finalize(out, hits, misses, Some(ttl), tier)?;
    resp.headers().set("x-upstream-ms", &format!("{upstream_ms:.1}"))?;
    resp.headers().set("x-l2-ms", &format!("{l2_roundtrip_ms:.1}"))?;
    emit_event(env, ctx, &qname, qtype, tier, &region, &colo, &bytes, start_ms, upstream_ms, l2_roundtrip_ms);
    Ok(resp)
}

fn emit_event(
    env: &Env,
    ctx: &Context,
    qname: &str,
    qtype: u16,
    tier: &str,
    region: &str,
    colo: &str,
    bytes: &[u8],
    start_ms: f64,
    upstream_ms: f64,
    l2_roundtrip_ms: f64,
) {
    let latency_ms = now_ms() - start_ms;
    let rcode = metrics::rcode_of(bytes);
    let Ok(ds) = env.analytics_engine(metrics::DATASET_BINDING) else {
        return;
    };
    let qname_s = qname.to_string();
    let tier_s = tier.to_string();
    let region_s = region.to_string();
    let colo_s = colo.to_string();
    let resp_bytes = bytes.len();
    ctx.wait_until(async move {
        let point = AnalyticsEngineDataPointBuilder::new()
            .indexes([qname_s.as_str()])
            .add_blob(tier_s.as_str())
            .add_blob(metrics::qtype_name(qtype))
            .add_blob(region_s.as_str())
            .add_blob(colo_s.as_str())
            .add_blob(metrics::rcode_name(rcode))
            .add_double(latency_ms)
            .add_double(upstream_ms)
            .add_double(resp_bytes as f64)
            .add_double(l2_roundtrip_ms)
            .build();
        let _ = ds.write_data_point(&point);
    });
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
) -> Result<(Vec<u8>, Option<String>, f64)> {
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
    let upstream_ms = resp
        .headers()
        .get("x-upstream-ms")?
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    Ok((resp.bytes().await?, status, upstream_ms))
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
