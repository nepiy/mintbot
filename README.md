# NFT Mint Bot

`nft-mint-bot` is a terminal-based, event-driven NFT mint executor for EVM-compatible networks. It prepares a legitimate wallet transaction before a sale opens, enters an explicit `BOT ARMED` state, listens for new blocks or configured contract events over WebSocket, atomically accepts the first valid trigger, signs locally, broadcasts the exact same raw transaction to all configured HTTP endpoints, and monitors the receipt.

It does not automate a browser, click a website, use MetaMask, or bypass allowlists, signatures, queues, CAPTCHA, wallet limits, or other contract/platform controls. Any proof or authorization must be supplied by the user in the collection configuration.

For OpenSea Drops, the optional Drops API mode asks OpenSea to build the eligible mint transaction for the connected wallet. OpenSea applies the active GTD/FCFS/public stage rules and returns the exact contract, calldata, and payment value for the wallet to sign; it does not bypass eligibility.

## Supported networks

The implementation is network-agnostic: Ethereum, Base, Arbitrum, Optimism, Polygon, BNB Chain, Avalanche C-Chain, Robinhood Chain, local Anvil, and other EVM JSON-RPC networks can be used by changing the chain ID and endpoints. No network-specific transaction logic is hardcoded. Solana is intentionally not mixed into this engine; a future Solana backend should implement a separate chain abstraction.

## How it works

Startup performs configuration validation, signer creation, HTTP and WebSocket connection setup, chain and bytecode validation, dynamic ABI parsing, calldata encoding, value calculation, nonce loading, gas estimation/selection, gas-cap validation, balance validation, transaction-template preparation, and trigger subscription setup. Only after all critical checks and subscriptions succeed does it print `BOT ARMED`.

The trigger monitor is block-driven rather than a timer poller:

```text
WebSocket block/log
        ↓
trigger evaluation
        ↓
atomic WaitingForTrigger → Triggered transition
        ↓
local signing
        ↓
same raw transaction to configured HTTP RPCs
        ↓
receipt and optional replacement monitor
```

The atomic state transition means a block trigger, event trigger, or manual trigger racing with another trigger can produce at most one submission attempt.

## Installation

Rust stable is required. The current Alloy release selected by `Cargo.toml` requires Rust 1.94.1 or newer.

```bash
cp .env.example .env
cargo build --release
```

The release profile uses optimized codegen, thin LTO, one codegen unit, `panic = "abort"`, and symbol stripping. Measure the release binary, not `cargo run` debug builds.

## Environment

Copy `.env.example` to `.env` and fill in:

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

`BACKUP_RPC_URL` and `BROADCAST_RPC_URLS` are optional. At startup, the bot verifies the WebSocket endpoint and every usable HTTP endpoint against `chain_id`; wrong-chain endpoints prevent arming and unavailable endpoints are excluded. Healthy HTTP endpoints provide read fallback as well as receiving the identical signed bytes. The first valid acknowledgement is treated as submission, and the returned hash must match the locally computed transaction hash. Known-transaction responses are accepted as successful rebroadcasts. The timeout values are optional and must be positive integer milliseconds.

Use a dedicated mint wallet. Fund it with the mint value, maximum expected gas, and a small margin only. Never use a valuable main wallet for automation, never commit `.env`, and never paste a private key into JSON. The process holds the signer in memory and only prints a shortened address.

## Collection configuration

Start with [`configs/example.json`](/Users/nepiy/workspace/mintbot/configs/example.json), replace the placeholder contract address, and adjust the chain and RPC endpoints. One configuration file represents one mint.

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

Supported argument placeholders are `$wallet`, `$quantity`, and `$proof`. Common dynamic calls include `mint(uint256)`, `publicMint(uint256)`, `mint(address,uint256)`, and `mint(uint256,bytes32[])`. The proof is encoded only when it is legitimately provided. Static bindings for the included `MockMint` interface are demonstrated in `src/abi.rs`; the normal collection path uses dynamic ABI parsing so a new mint does not require recompilation.

`expected_start_time` is informational. It never overrides actual on-chain state and the bot does not exit when that time passes.

### Trigger types

- `block_timestamp`: fire when an incoming block header timestamp reaches the configured Unix timestamp.
- `boolean_contract_state`: call a zero-argument view such as `publicSaleActive() returns (bool)` once per incoming block.
- `numeric_phase`: call a zero-argument view and compare the returned unsigned integer to `target_value`.
- `contract_event`: subscribe to a contract event such as `PublicSaleStarted()` and optionally wait for event confirmations.
- `manual`: wait for the local control command described below.

The timestamp trigger uses the chain header, not the local clock. View triggers read the configured contract at the exact incoming block hash through the healthy HTTP provider set. Event subscriptions are restricted to the configured contract address and event signature; confirmed events are rechecked for canonicality, and reconnects backfill the missed block range.

### Gas and nonce safety

Gas modes are `auto`, `eip1559`, `legacy`, and `manual`. Auto mode estimates EIP-1559 fees, applies `gas.multiplier`, and refreshes fees while the bot waits; explicit modes accept gwei strings without floating-point money arithmetic. `max_total_gas_cost_native` prevents arming or submitting when the estimated maximum gas cost exceeds the configured cap. If a closed sale causes `eth_estimateGas` to revert before opening, set `gas.gas_limit` from a trusted simulation or prior transaction; the bot will not guess a limit.

Nonce modes are:

- `preloaded`: lowest trigger latency; obtains the pending nonce before arming. Do not send another transaction from this wallet while armed.
- `refresh_each_block`: refreshes the pending nonce on each received block.
- `just_before_trigger`: fetches the pending nonce after the trigger wins and before signing.

Replacement transactions, when enabled, reuse the original nonce, bump the applicable fee fields, stop after `max_attempts`, and respect the gas cap. Receipt monitoring continues across the original hash and every replacement hash.

## Commands

### Interactive launch

For a one-off mint, you do not need to prepare a JSON file first. Start the interactive flow:

```bash
./target/release/nft-mint-bot
```

The explicit equivalent is:

```bash
./target/release/nft-mint-bot start
```

It targets Robinhood Chain mainnet automatically (chain ID `4663`), so it does not ask for a chain ID, collection name, mint function, arguments, or gas limit. The normal contract flow asks for the contract address, quantity, price, proof, and an automatic trigger. Its defaults are `mint(uint256)` with `$quantity` and a `200000` gas limit; collections using a different signature or requiring a higher limit must use the JSON/advanced workflow.

To use an OpenSea GTD/FCFS/public drop, set `OPENSEA_API_KEY` in `.env` and enter the collection’s OpenSea drop slug when prompted. When that key is configured, the interactive flow requires a slug or the explicit word `direct`; it will not silently fall back to the incompatible direct-contract flow. OpenSea mode asks whether the mint must remain free; when enabled, any nonzero value returned by OpenSea aborts before signing. The bot fetches the drop’s stage schedule, so enter `0` to monitor the active and upcoming stages automatically, or enter a Unix timestamp to ignore stages before that time. It can be started before the first stage and remains armed; when a stage starts it calls `POST /api/v2/drops/{slug}/mint` with your wallet and quantity. OpenSea then chooses the first active stage for which that wallet is eligible and returns the ready-to-sign transaction. If you are not GTD-eligible, a 422 response advances the monitor to the next scheduled stage, such as FCFS, without requiring a Merkle proof. The OpenSea mode does not ask for price, Merkle proof, or mint function because OpenSea supplies the payable value and calldata.

The interactive configuration is held in memory and is not written to a JSON file. Use `--dry-run` to verify the trigger path without signing or broadcasting:

```bash
./target/release/nft-mint-bot start --dry-run
```

Interactive setup keeps the private key out of the generated JSON:

```bash
./target/release/nft-mint-bot setup
```

Benchmark the configured RPCs, including chain ID, block number, balance, WebSocket startup, and subscription setup:

```bash
./target/release/nft-mint-bot rpc-test
```

Validate and simulate without broadcasting:

```bash
./target/release/nft-mint-bot simulate --config configs/my-mint.json
```

Run against actual chain triggers but stop immediately before signing:

```bash
./target/release/nft-mint-bot run --config configs/my-mint.json --dry-run
```

Run the real executor:

```bash
./target/release/nft-mint-bot run --config configs/my-mint.json
```

For a manual trigger, start the bot with a `manual` trigger and run this from another terminal:

```bash
./target/release/nft-mint-bot trigger --config configs/my-mint.json
```

The control channel binds only to localhost and writes a mode-`0600` short-lived control file under the system temporary directory. The command authenticates with a random one-time token from that file, the listener accepts exactly one valid trigger, and the file is removed when the monitor exits.

OpenSea API mode still requires the contract address: the bot compares OpenSea’s returned transaction target with that address before signing. The API key is read from `OPENSEA_API_KEY`, is never logged or stored in the mint configuration, and is held only in memory.

## Local Anvil test

[`contracts/MockNFT.sol`](/Users/nepiy/workspace/mintbot/contracts/MockNFT.sol) is a deliberately small test contract. It has `publicSaleActive()`, `salePhase()`, a `PublicSaleStarted` event, and a `mint(uint256)` function.

With Foundry installed:

```bash
anvil --chain-id 31337
forge create contracts/MockNFT.sol:MockNFT \
  --rpc-url http://127.0.0.1:8545 \
  --private-key <anvil-test-private-key>
```

Copy the deployed address into `configs/example.json`, use the Anvil funded key as `PRIVATE_KEY`, and set `WS_RPC_URL=ws://127.0.0.1:8545` and `HTTP_RPC_URL=http://127.0.0.1:8545`. Start the bot with the boolean trigger, confirm `BOT ARMED`, then activate the sale in another terminal:

```bash
cast send <contract-address> "setPublicSale(bool)" true \
  --rpc-url http://127.0.0.1:8545 \
  --private-key <anvil-test-private-key>
```

The next relevant block makes the boolean trigger ready. The Rust tests cover configuration defaults, unit parsing, ABI/proof encoding, event-filter construction, and the block/event one-shot race. The Anvil flow above is the live integration smoke test because it requires an external node and deployed address.

## RPC and latency tuning

Run `rpc-test` several times and compare the reported distribution of real provider response times. In practice, endpoint location, provider load, WebSocket reliability, and transaction propagation usually dominate local Rust execution time. The bot records `Instant` timestamps for trigger receipt/evaluation, atomic acquisition, signing, and the first broadcast acknowledgement, then prints a latency report after submission.

`benchmark` measures local calldata preparation, transaction finalization, signing, raw encoding, the atomic operation, and the combined trigger-to-send-ready path without sending a transaction:

```bash
./target/release/nft-mint-bot benchmark --iterations 10000
```

Keep critical-path logging intentionally small. Calldata, ABI parsing, environment reads, RPC client setup, balance checks, gas estimation, and signer construction happen before `ARMED` wherever possible.

## Troubleshooting

- `PRIVATE_KEY is not set`: load `.env` from the directory where the command runs or export the variable.
- `chain ID mismatch`: update `chain_id`; the bot refuses to arm against a different chain.
- `has no deployed bytecode`: check the address and network; the bot refuses to arm for an empty address.
- `insufficient balance`: fund the dedicated wallet for mint value plus the configured maximum gas cost.
- ABI errors: use canonical Solidity signatures and make `arguments` count/types match the function. Use placeholders only where supported.
- WebSocket disconnects: the monitor logs the failure, reconnects with bounded Alloy retries, revalidates the chain ID, restores subscriptions, and backfills event logs across the gap. HTTP broadcast/read endpoints remain available independently.
- A pending transaction: enable bounded replacement only after choosing an appropriate fee multiplier and safety cap. Never run an unbounded replacement loop.
- A reverted transaction: inspect the receipt and contract requirements. This tool does not bypass sale state, allowlists, signatures, or wallet limits.

Ctrl+C stops an armed monitor without submitting. After a transaction is submitted, Ctrl+C stops receipt monitoring but prints the transaction hash so it can be followed independently.

## Adding another collection or EVM chain

Create another JSON file, change `chain_id`, endpoints, contract address, mint signature/arguments, price, and trigger. No Rust source change is needed for normal dynamic mint calls. A known contract can instead add a compile-time `alloy::sol!` interface for stronger type checking and IDE support; dynamic JSON ABI is more flexible but moves ABI validation to startup. A new EVM chain only needs compatible HTTP/WebSocket JSON-RPC endpoints and the correct chain ID/native denomination.

## Limitations and future work

This version intentionally does not include a mempool watcher, private relay submission, contract-specific source-code inference, automated proof acquisition, or Solana transaction logic. Public mempool support is highly provider- and L2-dependent and should be added as an optional signal without replacing block/event monitoring. Future Solana support should be a separate engine with its own transaction preparation, signing, and RPC abstractions.

## Official API references

The OpenSea integration follows [Build mint transaction data](https://docs.opensea.io/reference/build_drop_mint_transaction) and [Mint from a Drop Programmatically](https://docs.opensea.io/docs/mint-from-a-drop). The provider and transaction paths follow the current Alloy provider, ABI, and transaction APIs: [Alloy RPC providers](https://alloy.rs/rpc-providers/introduction/), [Alloy static and dynamic ABI](https://alloy.rs/guides/static-dynamic-abi-in-alloy/), and [Alloy transactions](https://alloy.rs/transactions/introduction/). Tokio’s signal handling is documented at [tokio::signal::ctrl_c](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html).
