# Mint execution latency — September 5, 2026

Both OpenSea execution modes now overlap more network work. These changes
reduce preparation time; they do not promise earlier blockchain inclusion.

## Changes

- Normal mode starts the OpenSea build, fresh fees, balance, and just-in-time
  nonce lookup concurrently. Gas simulation still uses the validated calldata
  and refreshed fees. Final payment and gas checks use the fresh balance.
- Aggressive mode processes new blocks/events while one cache refresh is in
  flight. A trigger overlaps completion of that refresh with the OpenSea build.
  Failed or cancelled refreshes cannot authorize signing with an unhealthy cache.
- Nonce selection remains protected by a cross-process wallet lock, held through
  broadcast acknowledgement. Contention forces a fresh nonce lookup.
- Final bytecode-pin and Ink surcharge checks run concurrently before signing.
- Identical HTTP RPC URLs are deduplicated, including repeated backup/broadcast
  entries. Different paths or query parameters remain distinct.
- The latency report appears immediately after submission and includes the
  complete observed-trigger-to-send interval.

No payment caps, gas limits, provider credentials, or local `.env` values were changed.

## Controlled preparation comparison

Local mock services took 100 ms per external operation. The baseline reproduces
the previous request ordering; optimized measurements use production preparation
helpers. Neither path signs or broadcasts a transaction.

| Scenario | Previous ordering | Optimized ordering |
| --- | ---: | ---: |
| Normal preparation | 417.3 ms | 209.7 ms |
| Aggressive, refresh pending at trigger | 205.7 ms | 104.8 ms |

The aggressive comparison applies when a refresh is still pending. An already
warm cache has no refresh delay to remove. These measurements exclude real
OpenSea response variability, signing, propagation, and block inclusion.

Reproduce with:

```bash
cargo +1.94.1 test --locked --lib mocked_opensea_latency_comparison -- --nocapture
```

The release CPU benchmark over 1,000 iterations measured mean signing at
36.3 microseconds and mean local trigger-to-send-ready work at 37.5 microseconds.
That benchmark excludes RPC and OpenSea work.

## Live read-only RPC probes

Each HTTP route received 10 chain-ID, 10 block-number, and 10 zero-address
balance requests. Profiles were probed concurrently from this machine using
the release executable. These are one-session observations, not provider-wide
reliability estimates. Failures are excluded from latency averages.

| Network | Configured route | Successful reads | Mean block read | Mean balance read |
| --- | --- | ---: | ---: | ---: |
| Robinhood | Primary: Alchemy | 30/30 | 89.4 ms | 87.4 ms |
| Robinhood | Backup: dRPC | 6/30 | No successful samples | No successful samples |
| Robinhood | Broadcast: PublicNode | 30/30 | 170.6 ms | 170.1 ms |
| Ink | Primary: Alchemy | 30/30 | 244.2 ms | 258.8 ms |
| Ink | Backup: Ink public RPC | 30/30 | 188.2 ms | 193.5 ms |
| HyperEVM | Primary: dRPC | 22/30 | 81.7 ms | 119.3 ms |
| HyperEVM | Backup: Alchemy | 30/30 | 87.5 ms | 88.0 ms |

The HyperEVM dRPC WebSocket logged two connection resets and one failed
subscription attempt out of ten. Robinhood and Ink subscriptions succeeded
10/10. Subscription setup timing does not measure how quickly a provider
delivers a new block notification.

The current dRPC HTTP/WebSocket failures are a remaining execution risk worth
investigating with that provider. The bot already races validated HTTP routes
for reads and uses both WebSocket and HTTP for broadcasting, so changing HTTP
list order alone does not remove the need for reliable block notifications.

Repeat a profile probe with `rpc-test --chain-id 4663`, `57073`, or `999`.
No live wallet was used and no mint was submitted during these probes.

## Validation

- Rust 1.94.1: 78 tests passed; the opt-in Gitleaks executable test was skipped.
- Clippy passed with all targets/features and warnings denied.
- Formatting and diff whitespace checks passed.
- The optimized release executable was rebuilt successfully.
- New regressions cover concurrent requests, responsive monitoring, cache
  cancellation/recovery, stage invalidation, gas/balance checks, nonce contention,
  lock release after failure, and endpoint deduplication.
