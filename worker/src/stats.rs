use serde::Deserialize;
use worker::*;

const DATASET: &str = "doh_edge_queries_v2";
const BAR_WIDTH: usize = 30;

const Q_OVERALL: &str = "
SELECT
  count() AS total,
  sum(if(blob1 = 'L1', 1, 0)) AS l1,
  sum(if(blob1 = 'L2-HIT', 1, 0)) AS l2_hit,
  sum(if(blob1 = 'L2-STALE', 1, 0)) AS l2_stale,
  sum(if(blob1 = 'L2-MISS', 1, 0)) AS l2_miss,
  sum(if(blob1 = 'UPSTREAM', 1, 0)) AS upstream,
  quantileWeighted(0.5)(double1, _sample_interval) AS p50,
  quantileWeighted(0.95)(double1, _sample_interval) AS p95,
  quantileWeighted(0.99)(double1, _sample_interval) AS p99
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
";

const Q_TIER_LATENCY: &str = "
SELECT
  blob1 AS tier,
  count() AS n,
  quantileWeighted(0.5)(double1, _sample_interval) AS p50,
  quantileWeighted(0.95)(double1, _sample_interval) AS p95,
  quantileWeighted(0.99)(double1, _sample_interval) AS p99,
  quantileWeighted(0.5)(double2, _sample_interval) AS up50,
  quantileWeighted(0.95)(double2, _sample_interval) AS up95
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
GROUP BY tier
ORDER BY n DESC
";

const Q_TOP_NAMES: &str = "
SELECT index1 AS qname, count() AS n
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
GROUP BY qname
ORDER BY n DESC
LIMIT 20
";

const Q_QTYPES: &str = "
SELECT blob2 AS qtype, count() AS n
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
GROUP BY qtype
ORDER BY n DESC
";

const Q_RCODES: &str = "
SELECT blob5 AS rcode, count() AS n
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
GROUP BY rcode
ORDER BY n DESC
";

const Q_L2_HIT_TAIL: &str = "
SELECT blob4 AS colo, count() AS n, quantileWeighted(0.95)(double1, _sample_interval) AS p95
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR AND blob1 = 'L2-HIT' AND double1 > 400
GROUP BY colo
ORDER BY n DESC
";

const Q_COLOS: &str = "
SELECT blob4 AS colo, count() AS n
FROM DATASET
WHERE timestamp > NOW() - INTERVAL '24' HOUR
GROUP BY colo
ORDER BY n DESC
LIMIT 10
";

#[derive(Deserialize, Debug)]
struct SqlResponse {
    data: Vec<serde_json::Value>,
}

async fn run_sql(account_id: &str, token: &str, sql: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/analytics_engine/sql"
    );
    let body = sql.replace("DATASET", DATASET);

    let headers = Headers::new();
    headers.set("authorization", &format!("Bearer {token}"))?;
    headers.set("content-type", "text/plain")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(wasm_bindgen::JsValue::from_str(&body)));

    let req = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let text = resp.text().await?;
    let parsed: SqlResponse = serde_json::from_str(&text)
        .map_err(|e| Error::RustError(format!("sql parse: {e}; body={text}")))?;
    Ok(parsed.data)
}

pub async fn render(env: &Env) -> Result<Response> {
    let account_id = env.secret("CLOUDFLARE_ACCOUNT_ID")?.to_string();
    let token = env.secret("ANALYTICS_API_TOKEN")?.to_string();

    let overall = run_sql(&account_id, &token, Q_OVERALL).await?;
    let tier_latency = run_sql(&account_id, &token, Q_TIER_LATENCY).await?;
    let top_names = run_sql(&account_id, &token, Q_TOP_NAMES).await?;
    let qtypes = run_sql(&account_id, &token, Q_QTYPES).await?;
    let rcodes = run_sql(&account_id, &token, Q_RCODES).await?;
    let colos = run_sql(&account_id, &token, Q_COLOS).await?;
    let l2_hit_tail = run_sql(&account_id, &token, Q_L2_HIT_TAIL).await?;

    let body = render_html(&overall, &tier_latency, &top_names, &qtypes, &rcodes, &colos, &l2_hit_tail);
    let resp = Response::from_html(body)?;
    resp.headers().set("cache-control", "no-store")?;
    Ok(resp)
}

fn render_html(
    overall: &[serde_json::Value],
    tier_latency: &[serde_json::Value],
    top_names: &[serde_json::Value],
    qtypes: &[serde_json::Value],
    rcodes: &[serde_json::Value],
    colos: &[serde_json::Value],
    l2_hit_tail: &[serde_json::Value],
) -> String {
    let o = overall.first();
    let total = jnum(o, "total");
    let l1 = jnum(o, "l1");
    let l2_hit = jnum(o, "l2_hit");
    let l2_stale = jnum(o, "l2_stale");
    let l2_miss = jnum(o, "l2_miss");
    let upstream = jnum(o, "upstream");
    let hits = l1 + l2_hit + l2_stale;
    let hit_rate = if total > 0.0 { hits / total * 100.0 } else { 0.0 };
    let p50 = jnum(o, "p50");
    let p95 = jnum(o, "p95");
    let p99 = jnum(o, "p99");

    let tier_section = render_tier_dist(total, l1, l2_hit, l2_stale, l2_miss, upstream);
    let latency_section = render_latency(tier_latency);
    let names_section = render_counts(top_names, "qname", "n");
    let qtypes_section = render_counts(qtypes, "qtype", "n");
    let rcodes_section = render_counts(rcodes, "rcode", "n");
    let colos_section = render_counts(colos, "colo", "n");
    let l2_tail_section = render_l2_tail(l2_hit_tail);

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="30">
<title>doh-edge · stats</title>
<style>
body {{ background: #0d1117; color: #c9d1d9; font-family: "JetBrains Mono", ui-monospace, monospace; margin: 2rem auto; max-width: 900px; padding: 0 1rem; }}
h1 {{ color: #f0f6fc; font-weight: 500; font-size: 1.1rem; border-bottom: 1px solid #30363d; padding-bottom: .5rem; }}
pre {{ font-size: 13px; line-height: 1.5; white-space: pre; overflow-x: auto; }}
.dim {{ color: #8b949e; }}
</style>
</head>
<body>
<h1>doh-edge · stats · last 24h</h1>
<pre>
Queries         {total:>10}
Cache hit rate  {hit_rate:>9.1}%
p50 latency     {p50:>9.0} ms
p95 latency     {p95:>9.0} ms
p99 latency     {p99:>9.0} ms

Tier distribution
{tier_section}
Latency by tier (ms)
{latency_section}
Top 20 queried names
{names_section}
Qtype distribution
{qtypes_section}
Response codes
{rcodes_section}
Top colos
{colos_section}
Slow L2-HITs (>400ms) by colo
{l2_tail_section}</pre>
<p class="dim">Queries are logged to Cloudflare Analytics Engine in aggregate. Qnames are indexed for top-N computation. No client IPs are logged. ~30s ingestion lag. This is a personal PoC; do not use as your production resolver if you object.</p>
</body>
</html>"#
    )
}

fn render_tier_dist(total: f64, l1: f64, l2_hit: f64, l2_stale: f64, l2_miss: f64, upstream: f64) -> String {
    let mut out = String::new();
    for (label, n) in [
        ("L1", l1),
        ("L2-HIT", l2_hit),
        ("L2-STALE", l2_stale),
        ("L2-MISS", l2_miss),
        ("UPSTREAM", upstream),
    ] {
        let pct = if total > 0.0 { n / total * 100.0 } else { 0.0 };
        let bar_len = if total > 0.0 { (n / total * BAR_WIDTH as f64) as usize } else { 0 };
        let bar = "█".repeat(bar_len);
        out.push_str(&format!("  {label:<10} {bar:<30} {pct:>5.1}%  {n:>8.0}\n"));
    }
    out
}

fn render_latency(rows: &[serde_json::Value]) -> String {
    let mut out = String::from("              count   total p50/p95/p99      upstream p50/p95\n");
    for r in rows {
        let tier = jstr(Some(r), "tier");
        let n = jnum(Some(r), "n");
        let p50 = jnum(Some(r), "p50");
        let p95 = jnum(Some(r), "p95");
        let p99 = jnum(Some(r), "p99");
        let up50 = jnum(Some(r), "up50");
        let up95 = jnum(Some(r), "up95");
        out.push_str(&format!(
            "  {tier:<10} {n:>8.0}   {p50:>4.0} / {p95:>4.0} / {p99:>4.0}      {up50:>4.0} / {up95:>4.0}\n"
        ));
    }
    out
}

fn render_l2_tail(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "  (none)\n".to_string();
    }
    let mut out = String::from("              count   p95\n");
    for r in rows {
        let colo = jstr(Some(r), "colo");
        let n = jnum(Some(r), "n");
        let p95 = jnum(Some(r), "p95");
        out.push_str(&format!("  {colo:<10} {n:>8.0}   {p95:>5.0}\n"));
    }
    out
}

fn render_counts(rows: &[serde_json::Value], key: &str, val: &str) -> String {
    let mut out = String::new();
    for r in rows {
        let k = jstr(Some(r), key);
        let n = jnum(Some(r), val);
        out.push_str(&format!("  {k:<40} {n:>8.0}\n"));
    }
    out
}

fn jnum(v: Option<&serde_json::Value>, k: &str) -> f64 {
    v.and_then(|v| v.get(k))
        .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0.0)
}

fn jstr(v: Option<&serde_json::Value>, k: &str) -> String {
    v.and_then(|v| v.get(k))
        .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
        .unwrap_or_default()
}
