use crate::coalesce::Coalescer;
use crate::dns;
use std::cell::RefCell;
use std::collections::HashMap;
use worker::*;

const UPSTREAM: &str = "https://1.1.1.1/dns-query";
const DNS_MSG: &str = "application/dns-message";

type Key = (String, u16);

struct Entry {
    bytes: Vec<u8>,
    fetched_at_ms: f64,
    ttl_secs: u32,
}

#[durable_object]
pub struct Resolver {
    cache: RefCell<HashMap<Key, Entry>>,
    coalescer: Coalescer<Key, Result<Vec<u8>, String>>,
    _state: State,
    _env: Env,
}

impl DurableObject for Resolver {
    fn new(state: State, env: Env) -> Self {
        Self {
            cache: RefCell::new(HashMap::new()),
            coalescer: Coalescer::new(),
            _state: state,
            _env: env,
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let query = req.bytes().await?;
        let Some((qname, qtype)) = dns::parse_question(&query) else {
            return Response::error("unparseable dns query", 400);
        };
        let key = (qname, qtype);
        let client_id = dns::read_id(&query);
        let now = js_sys::Date::now();

        if let Some(bytes) = self.serve_from_cache(&key, client_id, now) {
            return bytes_response(bytes, "HIT");
        }

        let log_key = format!("{} {}", key.0, key.1);
        let fetched = self
            .coalescer
            .run(key.clone(), || async {
                console_log!("DO-UPSTREAM {log_key}");
                fetch_upstream(&query).await.map_err(|e| e.to_string())
            })
            .await;

        let upstream_bytes = match fetched {
            Ok(b) => b,
            Err(e) => return Response::error(format!("upstream: {e}"), 502),
        };

        let (ttl, _) = dns::answer_ttl_info(&upstream_bytes);
        self.cache.borrow_mut().entry(key).or_insert(Entry {
            bytes: upstream_bytes.clone(),
            fetched_at_ms: now,
            ttl_secs: ttl,
        });

        let mut out = upstream_bytes;
        dns::rewrite_id(&mut out, client_id);
        bytes_response(out, "MISS")
    }
}

impl Resolver {
    fn serve_from_cache(&self, key: &Key, client_id: u16, now_ms: f64) -> Option<Vec<u8>> {
        let cache = self.cache.borrow();
        let entry = cache.get(key)?;
        let elapsed_ms = (now_ms - entry.fetched_at_ms).max(0.0);
        let elapsed_secs = (elapsed_ms / 1000.0) as u32;
        if elapsed_secs >= entry.ttl_secs {
            return None;
        }
        let mut bytes = entry.bytes.clone();
        let (_, offsets) = dns::answer_ttl_info(&bytes);
        dns::decrement_ttls(&mut bytes, &offsets, elapsed_secs);
        dns::rewrite_id(&mut bytes, client_id);
        Some(bytes)
    }
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

fn bytes_response(bytes: Vec<u8>, status: &str) -> Result<Response> {
    let mut resp = Response::from_bytes(bytes)?;
    let h = resp.headers_mut();
    h.set("content-type", DNS_MSG)?;
    h.set("x-do-status", status)?;
    Ok(resp)
}
