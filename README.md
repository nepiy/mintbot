# NFT Mint Bot

## 1. Information about the bot

`nft-mint-bot` is a terminal-based, event-driven NFT mint executor for EVM-compatible networks. It prepares a wallet transaction before a sale opens, waits for a blockchain trigger, signs locally, broadcasts the same raw transaction to configured HTTP RPC endpoints, and monitors the receipt.

The interactive launcher supports:

- Robinhood Chain mainnet — chain ID `4663`
- Ink mainnet — chain ID `57073`

Advanced JSON configurations can target other EVM-compatible networks by providing the correct chain ID, RPC endpoints, contract address, mint ABI, and trigger.

For OpenSea Drops, the optional Drops API mode asks OpenSea to build the eligible mint transaction for the configured wallet. OpenSea selects the active GTD, FCFS, or public stage and returns the calldata and exact payment value. The bot does not bypass allowlists, signatures, queues, CAPTCHA, wallet limits, or other contract/platform controls.

In OpenSea mode, the contract you enter is the NFT collection contract. The bot automatically uses OpenSea’s SeaDrop transaction target internally; you do not need to inspect a wallet popup to find that address.

## 2. Qualities

### Safety

- Validates the configured chain ID, RPC providers, and deployed contract bytecode before arming.
- Requires encrypted `https://` and `wss://` RPC transport outside local development.
- Keeps the private key in memory, loads it only from `.env`, and prints only a shortened wallet address.
- Uses a dedicated wallet recommendation and owner-only permissions for `.env` and generated configuration files.
- Uses a one-shot atomic trigger transition, so competing block, event, or manual triggers produce at most one submission attempt.
- Uses the latest live OpenSea price/value and refuses to sign above the configured cap.
- Free-mint mode refuses any nonzero payment.
- Applies a maximum total gas-cost cap before arming and before submission.
- Serializes just-in-time nonce selection across local bot processes.

### Low-latency execution

- Monitors every new block through WebSocket instead of polling a timer.
- Prepares configuration, ABI, calldata, gas strategy, balance checks, and subscriptions before `BOT ARMED`.
- Refreshes dynamic fee fields while waiting.
- Broadcasts identical signed bytes to every healthy configured HTTP endpoint.
- Reconnects WebSocket subscriptions and backfills missed event logs after a transport interruption.
- Supports bounded transaction replacement with the same nonce when explicitly enabled.

### Honest execution model

The bot does not automate a browser, click OpenSea, use MetaMask, acquire Merkle proofs, forge signatures, or defeat eligibility checks. Any proof or authorization must be legitimately supplied by the user or returned for the connected wallet by OpenSea’s API.

## 3. Setup mechanics

### Prerequisites

Rust stable is required. The current Alloy release selected by `Cargo.toml` requires Rust `1.94.1` or newer.

```bash
cp .env.example .env
chmod 600 .env
cargo build --release
```

Use the release binary for real runs. `cargo run --release` rebuilds only when needed and then starts it.

### Environment

Fill `.env` with a dedicated mint wallet and RPC endpoints:

```dotenv
PRIVATE_KEY=0x...
WS_RPC_URL=wss://...
HTTP_RPC_URL=https://...
BACKUP_RPC_URL=https://...
BROADCAST_RPC_URLS=https://rpc-two.example,https://rpc-three.example
OPENSEA_API_KEY=
RPC_TIMEOUT_MS=5000
BROADCAST_TIMEOUT_MS=3000
RUST_LOG=nft_mint_bot=info
```

The generic RPC variables are used by default, including for Robinhood. Ink can use its own profile:

```dotenv
INK_HTTP_RPC_URL=https://rpc-gel.inkonchain.com
INK_WS_RPC_URL=wss://ws-gel.inkonchain.com
INK_BACKUP_RPC_URL=
INK_BROADCAST_RPC_URLS=
```

The interactive launcher selects the Ink profile automatically when those variables are present. Ink’s official mainnet RPC documentation lists the HTTPS and WebSocket endpoints above. [Ink RPC documentation](https://docs.inkonchain.com/tools/rpc)

Keep backup and broadcast endpoints on the same network as the selected primary endpoints. Public RPCs may be rate-limited; use a dedicated provider for a time-sensitive mint. At startup, the bot verifies every usable endpoint against the configured chain ID and refuses to arm on a wrong-chain provider.

Never commit `.env`, paste a private key into JSON, or use a valuable main wallet for automation. The process needs enough native currency for the mint value, maximum expected gas, and a small margin.

### Interactive configuration

The interactive launcher does not ask for a raw chain ID. It asks for a network choice, then selects the correct chain ID automatically:

```text
1. Robinhood Chain mainnet
2. Ink mainnet
```

For a direct contract mint, it asks for the collection contract, quantity, price, proof if required, and an automatic trigger. The normal default is `mint(uint256)` with `$quantity`; collections with another ABI should use the advanced JSON workflow.

For an OpenSea GTD/FCFS/public drop, set `OPENSEA_API_KEY` and enter the collection’s drop slug. Enter `direct` only for a custom contract mint. OpenSea mode then asks:

- Whether the mint must remain free.
- If paid, the maximum acceptable price per NFT.
- The earliest stage time; enter `0` to automatically monitor active/upcoming stages.
- The mint quantity.

When the stage opens, the bot asks OpenSea to build a transaction for the configured wallet and quantity. If the wallet is not eligible for the current stage, the monitor can advance to the next scheduled stage. A higher returned price aborts before signing, and free-mint mode aborts on any nonzero value.

### Advanced JSON configuration

Start with [`configs/example.json`](configs/example.json). One configuration file represents one mint. Personal mint configurations are ignored by Git by default; only the placeholder example is tracked.

```json
{
  "name": "Example NFT",
  "chain_id": 8453,
  "native_currency": "ETH",
  "contract_address": "0x...",
  "quantity": 1,
  "mint": {
    "function": "mint(address,uint256)",
    "arguments": ["$wallet", "$quantity"],
    "proof": null,
    "price_per_nft": "0.005"
  },
  "trigger": {
    "type": "boolean_contract_state",
    "function": "publicSaleActive() returns (bool)",
    "expected_value": true
  },
  "gas": {
    "mode": "auto",
    "multiplier": 1.15,
    "max_total_gas_cost_native": "0.01"
  },
  "nonce_strategy": "preloaded",
  "replacement": {
    "enabled": false,
    "after_blocks": 2,
    "fee_multiplier": 1.15,
    "max_attempts": 2
  },
  "expected_start_time": null,
  "confirmations": 1
}
```

Supported argument placeholders are `$wallet`, `$quantity`, and `$proof`. Common dynamic calls include `mint(uint256)`, `publicMint(uint256)`, `mint(address,uint256)`, and `mint(uint256,bytes32[])`.

## 4. Run mechanics

### Interactive run

Start the optimized launcher:

```bash
cargo run --release
```

The explicit equivalent is:

```bash
./target/release/nft-mint-bot start
```

After setup succeeds, the bot prints `BOT ARMED`, waits for the selected trigger, signs locally, broadcasts, and monitors the receipt. `Mint value: fetched from OpenSea when the stage is active` is normal before an OpenSea stage opens.

Use dry-run to verify the trigger path without signing or broadcasting:

```bash
./target/release/nft-mint-bot start --dry-run
```

### Advanced commands

```bash
# Save a JSON configuration interactively
./target/release/nft-mint-bot setup

# Validate and simulate without broadcasting
./target/release/nft-mint-bot simulate --config configs/my-mint.json

# Run actual triggers but stop before signing
./target/release/nft-mint-bot run --config configs/my-mint.json --dry-run

# Run the real JSON-configured executor
./target/release/nft-mint-bot run --config configs/my-mint.json

# Measure configured RPC endpoints and WebSocket setup
./target/release/nft-mint-bot rpc-test

# Measure local preparation and signing without sending a transaction
./target/release/nft-mint-bot benchmark --iterations 10000
```

For a `manual` trigger, run the bot and then send the authenticated local trigger from another terminal:

```bash
./target/release/nft-mint-bot trigger --config configs/my-mint.json
```

### Trigger types

- `block_timestamp`: fire when an incoming block timestamp reaches a Unix timestamp.
- `boolean_contract_state`: call a zero-argument view such as `publicSaleActive() returns (bool)` once per block.
- `numeric_phase`: call a zero-argument view and compare the returned integer to `target_value`.
- `contract_event`: subscribe to an event such as `PublicSaleStarted()` and optionally wait for confirmations.
- `manual`: wait for the authenticated local control command.

The timestamp trigger uses the chain header, not the computer’s local clock. View triggers read the configured contract at the exact incoming block hash. Event subscriptions are restricted to the configured contract and event signature; canonicality is rechecked after confirmations.

### Gas and nonce behavior

Gas modes are `auto`, `eip1559`, `legacy`, and `manual`. Auto mode estimates EIP-1559 fees, applies the configured multiplier, and refreshes fee fields while waiting. `max_total_gas_cost_native` is only a safety ceiling; the actual fee is gas used multiplied by the effective gas price.

Nonce modes are:

- `preloaded`: lowest trigger latency, but do not send another transaction from that wallet while armed.
- `refresh_each_block`: refreshes the pending nonce on each block.
- `just_before_trigger`: obtains the pending nonce after the trigger wins and immediately before signing.

Ctrl+C stops an armed monitor without submitting. After a transaction has been submitted, Ctrl+C stops receipt monitoring but cannot cancel the blockchain transaction; keep the printed hash for independent tracking.

### Local Anvil test

[`contracts/MockNFT.sol`](contracts/MockNFT.sol) is a small test contract with sale-state views, a sale event, and `mint(uint256)`.

With Foundry installed:

```bash
anvil --chain-id 31337
forge create contracts/MockNFT.sol:MockNFT \
  --rpc-url http://127.0.0.1:8545 \
  --private-key <anvil-test-private-key>
```

Use the deployed address in `configs/example.json`, the Anvil funded key as `PRIVATE_KEY`, and loopback RPC URLs. Activate the sale from another terminal:

```bash
cast send <contract-address> "setPublicSale(bool)" true \
  --rpc-url http://127.0.0.1:8545 \
  --private-key <anvil-test-private-key>
```

### Troubleshooting

- `PRIVATE_KEY is not set`: run the command from the directory containing `.env` or export the variable.
- `chain ID mismatch`: select the correct network and ensure every RPC profile endpoint belongs to it.
- `has no deployed bytecode`: check the contract address and selected network.
- `insufficient balance`: fund the dedicated wallet for mint value plus the configured gas ceiling.
- ABI errors: use canonical Solidity signatures and match argument types/counts.
- OpenSea stage unavailable: the stage may not yet be active or the wallet may not be eligible; the monitor may advance to the next scheduled stage.
- WebSocket disconnects: the monitor reconnects, revalidates the chain, restores subscriptions, and backfills missed event logs.
- Reverted transaction: inspect the receipt and contract requirements; the bot does not bypass sale state, allowlists, signatures, or wallet limits.

### Official references

- [OpenSea: Mint from a Drop Programmatically](https://docs.opensea.io/docs/mint-from-a-drop)
- [OpenSea: Build drop mint transaction](https://docs.opensea.io/reference/build_drop_mint_transaction)
- [OpenSea: Compatible blockchains](https://support.opensea.io/en/articles/8867082-which-blockchains-are-compatible-with-opensea)
- [Ink RPC documentation](https://docs.inkonchain.com/tools/rpc)
- [Alloy RPC providers](https://alloy.rs/rpc-providers/introduction/)
- [Alloy static and dynamic ABI](https://alloy.rs/guides/static-dynamic-abi-in-alloy/)
- [Tokio Ctrl+C handling](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html)
