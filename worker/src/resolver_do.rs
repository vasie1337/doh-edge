use crate::coalesce::Coalescer;
use crate::dns;
use crate::http::{self, now_ms};
use std::cell::RefCell;
use std::collections::HashMap;
use worker::*;

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
        let now = now_ms();

        if let Some(bytes) = self.serve_from_cache(&key, client_id, now) {
            return with_status(bytes, "HIT");
        }

        let log_key = format!("{} {}", key.0, key.1);
        let fetched = self
            .coalescer
            .run(key.clone(), || async {
                console_log!("DO-UPSTREAM {log_key}");
                http::fetch_upstream(&query).await.map_err(|e| e.to_string())
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
        with_status(out, "MISS")
    }
}

impl Resolver {
    fn serve_from_cache(&self, key: &Key, client_id: u16, now_ms: f64) -> Option<Vec<u8>> {
        let cache = self.cache.borrow();
        let entry = cache.get(key)?;
        let elapsed_secs = ((now_ms - entry.fetched_at_ms).max(0.0) / 1000.0) as u32;
        if elapsed_secs >= entry.ttl_secs {
            return None;
        }
        let mut bytes = entry.bytes.clone();
        dns::apply_cache_hit(&mut bytes, client_id, elapsed_secs);
        Some(bytes)
    }
}

fn with_status(bytes: Vec<u8>, status: &str) -> Result<Response> {
    let resp = http::dns_response(bytes)?;
    resp.headers().set("x-do-status", status)?;
    Ok(resp)
}
