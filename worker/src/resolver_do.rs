use crate::coalesce::Coalescer;
use crate::dns;
use crate::http::{self, now_ms};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use worker::*;

const STALE_WINDOW_FLOOR_SECS: u32 = 30;

type Key = (String, u16);
type Cache = Rc<RefCell<HashMap<Key, Entry>>>;
type Pending = Rc<RefCell<HashSet<Key>>>;
type Coal = Rc<Coalescer<Key, Result<Vec<u8>, String>>>;

struct Entry {
    bytes: Vec<u8>,
    fetched_at_ms: f64,
    ttl_secs: u32,
}

enum Status {
    Fresh { elapsed: u32 },
    StaleUsable { elapsed: u32 },
    Expired,
}

impl Entry {
    fn classify(&self, now_ms: f64, force_stale: bool) -> Status {
        let elapsed = ((now_ms - self.fetched_at_ms).max(0.0) / 1000.0) as u32;
        let stale_window = (self.ttl_secs / 10).max(STALE_WINDOW_FLOOR_SECS);
        let effective_age = if force_stale {
            elapsed.saturating_add(self.ttl_secs)
        } else {
            elapsed
        };
        if effective_age < self.ttl_secs {
            Status::Fresh { elapsed: effective_age }
        } else if effective_age < self.ttl_secs + stale_window {
            Status::StaleUsable { elapsed: effective_age }
        } else {
            Status::Expired
        }
    }
}

#[durable_object]
pub struct Resolver {
    cache: Cache,
    coalescer: Coal,
    refresh_inflight: Pending,
    state: State,
    _env: Env,
}

impl DurableObject for Resolver {
    fn new(state: State, env: Env) -> Self {
        Self {
            cache: Rc::new(RefCell::new(HashMap::new())),
            coalescer: Rc::new(Coalescer::new()),
            refresh_inflight: Rc::new(RefCell::new(HashSet::new())),
            state,
            _env: env,
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let force_stale = req.headers().get("x-debug-force-stale")?.is_some();
        let ttl_override: Option<u32> = req
            .headers()
            .get("x-debug-ttl-override")?
            .and_then(|v| v.parse().ok());
        let query = req.bytes().await?;
        let Some((qname, qtype)) = dns::parse_question(&query) else {
            return Response::error("unparseable dns query", 400);
        };
        let key = (qname, qtype);
        let client_id = dns::read_id(&query);
        let now = now_ms();

        let status = self
            .cache
            .borrow()
            .get(&key)
            .map(|e| e.classify(now, force_stale));

        match status {
            Some(Status::Fresh { elapsed }) => {
                let bytes = serve_from_cache(&self.cache, &key, client_id, elapsed)
                    .expect("entry still present");
                with_status(bytes, "HIT", 0.0)
            }
            Some(Status::StaleUsable { elapsed }) => {
                let bytes = serve_from_cache(&self.cache, &key, client_id, elapsed)
                    .expect("entry still present");
                self.schedule_refresh(key, query);
                with_status(bytes, "STALE", 0.0)
            }
            Some(Status::Expired) | None => {
                let upstream_start = now_ms();
                let bytes = self.fetch_and_store(key.clone(), &query, ttl_override).await?;
                let upstream_ms = now_ms() - upstream_start;
                let mut out = bytes;
                dns::rewrite_id(&mut out, client_id);
                with_status(out, "MISS", upstream_ms)
            }
        }
    }
}

impl Resolver {
    async fn fetch_and_store(
        &self,
        key: Key,
        query: &[u8],
        ttl_override: Option<u32>,
    ) -> Result<Vec<u8>> {
        let fetched = self
            .coalescer
            .run(key.clone(), || async {
                console_log!("DO-UPSTREAM {} {}", key.0, key.1);
                http::fetch_upstream(query).await.map_err(|e| e.to_string())
            })
            .await;
        let bytes = fetched.map_err(|e| Error::RustError(format!("upstream: {e}")))?;
        store(&self.cache, key, &bytes, ttl_override);
        Ok(bytes)
    }

    fn schedule_refresh(&self, key: Key, query: Vec<u8>) {
        if !self.refresh_inflight.borrow_mut().insert(key.clone()) {
            return; // already scheduled
        }
        let cache = self.cache.clone();
        let coalescer = self.coalescer.clone();
        let pending = self.refresh_inflight.clone();
        console_log!("L2-STALE-REFRESH-START {} {}", key.0, key.1);
        self.state.wait_until(async move {
            let log_key = format!("{} {}", key.0, key.1);
            let fetched = coalescer
                .run(key.clone(), || async {
                    console_log!("DO-UPSTREAM {log_key}");
                    http::fetch_upstream(&query).await.map_err(|e| e.to_string())
                })
                .await;
            match fetched {
                Ok(bytes) => {
                    store(&cache, key.clone(), &bytes, None);
                    console_log!("L2-STALE-REFRESH-DONE {log_key}");
                }
                Err(e) => console_log!("L2-STALE-REFRESH-FAIL {log_key} {e}"),
            }
            pending.borrow_mut().remove(&key);
        });
    }
}

fn serve_from_cache(cache: &Cache, key: &Key, client_id: u16, elapsed: u32) -> Option<Vec<u8>> {
    let cache = cache.borrow();
    let entry = cache.get(key)?;
    let mut bytes = entry.bytes.clone();
    dns::apply_cache_hit(&mut bytes, client_id, elapsed);
    Some(bytes)
}

fn store(cache: &Cache, key: Key, bytes: &[u8], ttl_override: Option<u32>) {
    let (ttl, _) = dns::answer_ttl_info(bytes);
    cache.borrow_mut().insert(
        key,
        Entry {
            bytes: bytes.to_vec(),
            fetched_at_ms: now_ms(),
            ttl_secs: ttl_override.unwrap_or(ttl),
        },
    );
}

fn with_status(bytes: Vec<u8>, status: &str, upstream_ms: f64) -> Result<Response> {
    let resp = http::dns_response(bytes)?;
    let h = resp.headers();
    h.set("x-do-status", status)?;
    h.set("x-upstream-ms", &format!("{upstream_ms:.1}"))?;
    Ok(resp)
}
