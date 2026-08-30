# Build Context

## Review

- Reviewed: 2026-08-29
- Scope: recent OpenSea normal/aggressive execution upgrade, schedule refresh, RPC reads/broadcasting, nonce handling, gas/payment guards, and secret handling
- Security score: A-
- Code quality score: B+
- Performance score: B+
- P0 findings: 0
- P1 findings: 4 fixed
- P2 findings: 5 fixed
- Automated verification: 46 tests, strict Clippy, release build, and `git diff --check` all pass
- Dependency advisory verification: not run because `cargo-audit` and `cargo-deny` are not installed
- Ready for unattended real-funds use: no

## Fixed findings

1. Aggressive OpenSea mode now refreshes nonce, fee, and balance caches on every waiting block instead of retaining a startup nonce.
2. A bot process that waits for another process's wallet lock now refreshes the nonce after acquiring the lock.
   Wallet locks are scoped by chain ID so independent Ink and Robinhood nonces do not block each other.
3. Aggressive mode validates the fresh OpenSea payment against a cached balance and avoids a final balance RPC on the critical path.
4. Aggressive mode requires both a fixed gas limit and a maximum total gas-cost cap.
5. Normal OpenSea mode no longer makes redundant fee/balance RPC calls on every waiting block.
6. Aggressive inactive-stage responses retry on the next block without a two-second delay or wall-clock dependency.
7. OpenSea schedule refreshes preserve a previously selected later GTD/FCFS/public stage.
8. `refresh_each_block` now actually uses the refreshed nonce unless lock contention makes it stale.
9. Runtime OpenSea client invariants return errors instead of panicking.

## Residual risks

- Aggressive mode intentionally skips `eth_estimateGas`. A fixed gas limit can be insufficient, and a submitted transaction can revert while still consuming gas.
- Another wallet application can change the nonce after the bot's final block refresh. Use a dedicated wallet and do not send other transactions while armed.
- OpenSea still supplies wallet-specific public/GTD/FCFS calldata. API latency, rate limits, stale stage data, and service failures remain outside the bot's control.
- Mainnet behavior was not exercised with real funds during this review.
