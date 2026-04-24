# design

## tier 1: Workers Cache API

- Per-edge, sub-ms lookup. Hit never leaves the machine.
- Honors `Cache-Control: max-age`, so eviction is automatic.
- Not shared across edge machines. Fine — misses fall through to tier 2.

Alternatives:

- **KV** — eventually consistent, ~tens of ms. Slower than the upstream it's meant to shortcut.
- **DO** — network round-trip per read. Right for tier 2 (shared state), wrong for the hot path.
- **In-isolate memory** — no eviction, no sharing, short lifetime. OK for counters, not the cache.
