use futures_channel::oneshot;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;

/// Single-threaded async coalescer. Concurrent calls for the same key share
/// a single `fetch()` invocation; all waiters receive a clone of its result.
///
/// Correctness hinges on the borrow window: the map lookup-and-insert must
/// be synchronous (no `.await` between them), or two callers can both
/// conclude they are the leader for a key.
pub struct Coalescer<K, V> {
    inflight: RefCell<HashMap<K, Vec<oneshot::Sender<V>>>>,
}

impl<K: Eq + Hash + Clone, V: Clone> Coalescer<K, V> {
    pub fn new() -> Self {
        Self {
            inflight: RefCell::new(HashMap::new()),
        }
    }

    pub async fn run<F, Fut>(&self, key: K, fetch: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let rx = {
            let mut map = self.inflight.borrow_mut();
            match map.get_mut(&key) {
                Some(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    Some(rx)
                }
                None => {
                    map.insert(key.clone(), Vec::new());
                    None
                }
            }
        };

        if let Some(rx) = rx {
            return rx.await.expect("leader dropped without sending");
        }

        let value = fetch().await;
        let waiters = self
            .inflight
            .borrow_mut()
            .remove(&key)
            .unwrap_or_default();
        for tx in waiters {
            let _ = tx.send(value.clone());
        }
        value
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Default for Coalescer<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::future::{join, join_all};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Poll;

    async fn yield_once() {
        let mut yielded = false;
        futures::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    #[test]
    fn single_call_returns_fetched_value() {
        let c: Coalescer<&str, u32> = Coalescer::new();
        let v = block_on(c.run("a", || async { 42 }));
        assert_eq!(v, 42);
    }

    #[test]
    fn concurrent_same_key_fetches_once() {
        let c: Coalescer<&str, u32> = Coalescer::new();
        let calls = Rc::new(Cell::new(0u32));

        let mk = || {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    yield_once().await;
                    calls.set(calls.get() + 1);
                    99u32
                }
            }
        };

        let (a, b) = block_on(join(c.run("k", mk()), c.run("k", mk())));
        assert_eq!((a, b), (99, 99));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn different_keys_fetch_independently() {
        let c: Coalescer<&str, u32> = Coalescer::new();
        let calls = Rc::new(Cell::new(0u32));

        let mk = |val: u32| {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.set(calls.get() + 1);
                    val
                }
            }
        };

        let (a, b) = block_on(join(c.run("x", mk(1)), c.run("y", mk(2))));
        assert_eq!((a, b), (1, 2));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn many_waiters_all_receive_value() {
        let c: Coalescer<&str, u32> = Coalescer::new();
        let calls = Rc::new(Cell::new(0u32));

        let futs: Vec<_> = (0..10)
            .map(|_| {
                let calls = calls.clone();
                c.run("k", move || async move {
                    yield_once().await;
                    calls.set(calls.get() + 1);
                    7u32
                })
            })
            .collect();
        let results = block_on(join_all(futs));
        assert!(results.iter().all(|&v| v == 7));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn sequential_calls_each_fetch() {
        let c: Coalescer<&str, u32> = Coalescer::new();
        let calls = Rc::new(Cell::new(0u32));

        for _ in 0..3 {
            let calls = calls.clone();
            let v = block_on(c.run("k", move || async move {
                calls.set(calls.get() + 1);
                5u32
            }));
            assert_eq!(v, 5);
        }
        assert_eq!(calls.get(), 3);
    }
}
