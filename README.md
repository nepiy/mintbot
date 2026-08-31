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
- Overlaps the final OpenSea transaction build and fee estimate at the trigger.
- Supports normal and aggressive OpenSea execution modes for public, GTD, and FCFS stages.
- Refreshes the OpenSea stage schedule while waiting in automatic OpenSea mode.
- Reuses one OpenSea HTTP client and retries transient transaction-build responses with bounded backoff.
- Broadcasts identical signed bytes concurrently through the validated WebSocket and HTTP endpoints.
- Reconnects WebSocket subscriptions and backfills missed event logs after a transport interruption.
- Supports bounded transaction replacement with the same nonce when explicitly enabled.

### Honest execution model

The bot does not automate a browser, click OpenSea, use MetaMask, acquire Merkle proofs, forge signatures, or defeat eligibility checks. Any proof or authorization must be legitimately supplied by the user or returned for the connected wallet by OpenSea’s API.

## 3. Complete setup guide

Follow these steps in order. Commands labeled **macOS/Linux** run in Terminal. Commands labeled **Windows** run in PowerShell.

### Step 1 — Understand what is required

Normal use requires:

- A 64-bit macOS, Linux, or Windows computer.
- Git, unless you download the repository as a ZIP file.
- Rust `1.94.1` or newer, including Cargo.
- A native linker/compiler toolchain.
- One dedicated EVM wallet private key.
- One HTTPS RPC endpoint and one WebSocket RPC endpoint for the same network.
- An OpenSea API key only when using OpenSea Drops mode.
- Enough native currency in the dedicated wallet for the maximum mint payment, maximum gas cost, and a small margin.

Node.js, npm, Python, Foundry, Anvil, a browser wallet, and MetaMask are not required for normal use. Foundry and Anvil are used only by the optional local test later in this README.

Do not use a valuable everyday wallet. Create a dedicated mint wallet, fund it only with the amount you are prepared to spend, and never share its private key or seed phrase.

### Step 2 — Install system prerequisites

#### macOS

Install Apple’s command-line developer tools:

```bash
xcode-select --install
```

Complete the installer window, reopen Terminal, then install Rust with `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustup default stable
```

Git is included with the command-line developer tools.

#### Ubuntu or Debian Linux

```bash
sudo apt update
sudo apt install -y build-essential ca-certificates curl git pkg-config unzip
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustup default stable
```

#### Fedora Linux

```bash
sudo dnf group install -y "Development Tools"
sudo dnf install -y ca-certificates curl git pkgconf-pkg-config unzip
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustup default stable
```

#### Windows 10 or 11

Install Git and Rust from an Administrator PowerShell window:

```powershell
winget install --id Git.Git -e --source winget
winget install --id Rustlang.Rustup -e --source winget
```

Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and select the **Desktop development with C++** workload. Close and reopen PowerShell after the installers finish, then run:

```powershell
rustup update stable
rustup default stable
```

Verify the tools on any operating system:

```bash
git --version
rustc --version
cargo --version
```

`rustc --version` must report `1.94.1` or newer. If the stable toolchain on the machine is older, install the required toolchain inside the repository after Step 3:

```bash
rustup toolchain install 1.94.1
rustup override set 1.94.1
```

### Step 3 — Download the repository

#### Recommended: clone with Git

```bash
git clone https://github.com/nepiy/mintbot.git
cd mintbot
```

#### Alternative: download a ZIP

Open [the mintbot repository](https://github.com/nepiy/mintbot), select **Code → Download ZIP**, extract it, and open a terminal in the extracted `mintbot-main` directory.

On macOS or Linux, the equivalent terminal commands are:

```bash
curl -L https://github.com/nepiy/mintbot/archive/refs/heads/main.zip -o mintbot.zip
unzip mintbot.zip
cd mintbot-main
```

On Windows PowerShell:

```powershell
Invoke-WebRequest https://github.com/nepiy/mintbot/archive/refs/heads/main.zip -OutFile mintbot.zip
Expand-Archive .\mintbot.zip -DestinationPath .
Set-Location .\mintbot-main
```

All remaining commands must be run from the repository directory—the directory containing `Cargo.toml`, `.env.example`, and this README.

### Step 4 — Download Rust dependencies, test, and build

Cargo downloads the dependencies pinned in `Cargo.lock`; there is no separate dependency installer.

```bash
cargo fetch --locked
cargo test --locked --all-targets
cargo build --locked --release
```

The optimized executable is created at:

- macOS/Linux: `target/release/nft-mint-bot`
- Windows: `target\release\nft-mint-bot.exe`

Confirm that it starts:

```bash
./target/release/nft-mint-bot --version
./target/release/nft-mint-bot --help
```

Windows PowerShell equivalents:

```powershell
.\target\release\nft-mint-bot.exe --version
.\target\release\nft-mint-bot.exe --help
```

Use the release executable for real mints. `cargo run --release` is also supported, but it may spend time rebuilding before starting.

### Step 5 — Prepare a dedicated wallet

Create a new EVM wallet specifically for the bot. Export only that wallet’s private key; never enter a seed phrase into this project.

The `PRIVATE_KEY` value must be the wallet’s 32-byte hexadecimal private key, normally written as `0x` followed by 64 hexadecimal characters. The bot derives and displays a shortened wallet address from this key during startup.

Fund the wallet on the network you intend to use. It must cover:

1. The requested NFT quantity multiplied by the maximum price per NFT.
2. The configured maximum gas cost.
3. A small additional balance margin.

Do not send another transaction from this wallet while an aggressive-mode bot is armed; doing so can change its nonce.

### Step 6 — Obtain RPC endpoints

You need both endpoint types for the same chain:

- An HTTPS endpoint beginning with `https://` for reads and transaction broadcasts.
- A WebSocket endpoint beginning with `wss://` for new-block monitoring.

Create them in the dashboard of an RPC provider that supports your selected network. A dedicated provider is strongly recommended for a competitive mint because public endpoints can be rate-limited.

Optional backup and broadcast endpoints must be HTTPS endpoints for the same chain. At startup the bot checks all configured providers and refuses to arm if they report the wrong chain ID.

The interactive launcher supports these profiles:

| Network | Chain ID | Preferred variables |
| --- | ---: | --- |
| Robinhood Chain mainnet | `4663` | `ROBINHOOD_HTTP_RPC_URL`, `ROBINHOOD_WS_RPC_URL` |
| Ink mainnet | `57073` | `INK_HTTP_RPC_URL`, `INK_WS_RPC_URL` |

If either network-specific HTTP or WebSocket variable is filled, that profile is selected and both values must be valid. If a selected network has no profile values, the bot falls back to `HTTP_RPC_URL` and `WS_RPC_URL`.

The included `.env.example` contains Ink’s public HTTPS and WebSocket endpoints. Replace them with dedicated endpoints when reliability matters. See [Ink RPC documentation](https://docs.inkonchain.com/tools/rpc).

### Step 7 — Obtain an OpenSea API key if needed

OpenSea Drops mode requires `OPENSEA_API_KEY`:

1. Sign in to OpenSea using the account that will own the developer key.
2. Open OpenSea’s developer/API-key settings.
3. Create or reveal an API key.
4. Copy it once and paste it after `OPENSEA_API_KEY=` in `.env`.
5. Never paste the key into a command, issue report, screenshot, or committed file.

Direct contract mode does not require an OpenSea key, so leave `OPENSEA_API_KEY` empty when using only direct mints. See [OpenSea’s programmatic mint guide](https://docs.opensea.io/docs/mint-from-a-drop) for current API-key requirements.

You will also need:

- The OpenSea drop slug from the collection URL (the value after `/collection/`), not the collection’s display name.
- The NFT collection contract address on the selected network.

OpenSea constructs wallet-specific calldata and selects the first eligible active stage. The bot cannot bypass allowlists, wallet limits, unavailable supply, or OpenSea authorization requirements.

### Step 8 — Create and secure `.env`

Copy the provided template.

macOS/Linux:

```bash
cp .env.example .env
chmod 600 .env
nano .env
```

Windows PowerShell:

```powershell
Copy-Item .env.example .env
notepad .env
```

Fill the values without committing or sharing the file. A minimal generic configuration looks like this:

```dotenv
PRIVATE_KEY=0xYOUR_64_HEX_CHARACTER_PRIVATE_KEY

HTTP_RPC_URL=https://your-network-http-rpc.example
WS_RPC_URL=wss://your-network-websocket-rpc.example
BACKUP_RPC_URL=
BROADCAST_RPC_URLS=

OPENSEA_API_KEY=

RPC_TIMEOUT_MS=5000
BROADCAST_TIMEOUT_MS=3000
RUST_LOG=nft_mint_bot=info
```

For network-specific profiles, fill the matching pair instead:

```dotenv
ROBINHOOD_HTTP_RPC_URL=https://your-robinhood-http-rpc.example
ROBINHOOD_WS_RPC_URL=wss://your-robinhood-websocket-rpc.example
ROBINHOOD_BACKUP_RPC_URL=
ROBINHOOD_BROADCAST_RPC_URLS=

INK_HTTP_RPC_URL=https://rpc-gel.inkonchain.com
INK_WS_RPC_URL=wss://ws-gel.inkonchain.com
INK_BACKUP_RPC_URL=
INK_BROADCAST_RPC_URLS=
```

Environment-variable rules:

- `PRIVATE_KEY` is always required.
- `HTTP_RPC_URL` and `WS_RPC_URL` are the generic fallback pair.
- `*_BACKUP_RPC_URL` is one optional additional HTTPS provider.
- `*_BROADCAST_RPC_URLS` is an optional comma-separated list of additional HTTPS providers.
- `OPENSEA_API_KEY` is required only for OpenSea Drops.
- `RPC_TIMEOUT_MS` and `BROADCAST_TIMEOUT_MS` are optional millisecond timeouts.
- `RUST_LOG=nft_mint_bot=info` is the recommended default.
- Do not wrap values in angle brackets and do not leave placeholder URLs in fields the selected profile will use.

The real `.env` is ignored by Git. On macOS and Linux, the bot refuses to start if `.env` is readable by other users; fix that with `chmod 600 .env`.

### Step 9 — Check RPC connectivity

The RPC benchmark tests the generic `HTTP_RPC_URL` and `WS_RPC_URL` pair:

```bash
./target/release/nft-mint-bot rpc-test
```

If you use only a network-specific profile, the interactive startup in Step 10 validates that profile instead. It checks chain IDs, deployed contract bytecode, wallet balance, WebSocket subscriptions, and every usable broadcast endpoint before printing `BOT ARMED`.

### Step 10 — Run the interactive setup safely

Start with dry-run mode so the bot follows the real trigger path without signing or broadcasting:

```bash
./target/release/nft-mint-bot start --dry-run
```

Windows PowerShell:

```powershell
.\target\release\nft-mint-bot.exe start --dry-run
```

The wizard asks for these values in order:

1. Network: `1` for Robinhood Chain mainnet or `2` for Ink mainnet.
2. NFT collection contract address on that network.
3. OpenSea drop slug, or direct contract mode. If `OPENSEA_API_KEY` is filled, type `direct`; if it is empty, press Enter.
4. Mint quantity.

For OpenSea Drops it then asks:

1. Whether the mint must remain free. Choose `yes` only if any nonzero mint price must abort.
2. If it is paid, the maximum acceptable price per NFT in native currency.
3. Execution mode: choose `normal` for the safer default or `aggressive` only after reading the warnings below.

For direct contract mode it asks for the price, optional Merkle proof, and trigger. The simple interactive direct mode calls `mint(uint256)` with the requested quantity. Use an advanced JSON configuration when the contract uses a different function or argument layout.

Review the full startup summary. Do not proceed unless all of these are correct:

- Selected network and contract.
- Shortened wallet address.
- Mint quantity.
- Free-mint or maximum-price guard.
- Gas limit and maximum gas-cost cap.
- Trigger or OpenSea stage schedule.

If anything is wrong, press Ctrl+C and restart with the correct values. Dry-run mode prints `Mode: DRY-RUN`. When its trigger fires, it stops before signing or broadcasting.

### Step 11 — Start a real mint

After the dry run and configuration review, start the real executor:

```bash
./target/release/nft-mint-bot start
```

Windows PowerShell:

```powershell
.\target\release\nft-mint-bot.exe start
```

When the terminal prints `BOT ARMED`:

- Keep the process running and keep the computer awake and connected.
- Do not close the terminal.
- Do not send another transaction from the dedicated wallet while aggressive mode is armed.
- Ctrl+C stops monitoring without submitting a transaction.
- After submission, save the printed transaction hash; stopping receipt monitoring cannot cancel an on-chain transaction.

No-subcommand startup (`./target/release/nft-mint-bot`) is equivalent to a real interactive start, not a dry run.

### Step 12 — Understand OpenSea stage behavior

OpenSea mode automatically loads active and upcoming GTD, FCFS, allowlist, and public stages. It has no manual timestamp prompt.

- An explicit ineligibility, used wallet allowance, or unavailable stage supply response moves to the next scheduled phase.
- An ambiguous OpenSea `422` response retries the current phase instead of falsely skipping it.
- After an ambiguous response, the bot moves forward only after the next phase has actually started.
- Live total and maximum supply are checked before arming and after rejected requests.
- A temporary “stage not active” response is retried.
- A higher-than-allowed price, explicit balance failure, or insufficient remaining supply stops before signing.

The final transaction request is always fresh. OpenSea selects the first eligible active stage and returns the exact payment and wallet-specific calldata. The bot validates that calldata, collection, recipient, quantity, and payment before signing.

The automatic schedule refreshes every 30 seconds normally and every 5 seconds when a stage is within five minutes. If OpenSea changes or publishes a phase, the bot updates its trigger.

### Step 13 — Choose normal or aggressive OpenSea execution

- `normal` is recommended. It obtains fresh fees, performs live gas simulation, checks balance, and selects the nonce just before signing.
- `aggressive` minimizes trigger-path RPC work. It continuously prewarms fee, nonce, and balance data, uses the configured gas limit, and skips live gas simulation plus the final balance RPC.

Aggressive mode still enforces eligibility, calldata, payment, gas-cost, and balance guards, but it carries more risk: changed on-chain state or an insufficient fixed gas limit can produce a reverted transaction that still consumes gas. It requires explicit `gas.gas_limit` and `gas.max_total_gas_cost_native` values.

### Step 14 — Update an existing Git clone

Stop any running bot, then run:

```bash
git switch main
git pull --ff-only origin main
cargo test --locked --all-targets
cargo build --locked --release
```

Your ignored `.env` and personal `configs/*.json` files remain local. Review README changes after every update. ZIP installations do not support `git pull`; download a fresh ZIP, copy the existing `.env` into the new directory, run `chmod 600 .env` on macOS/Linux, and rebuild.

### Step 15 — Installation troubleshooting

- `cargo: command not found`: reopen the terminal or run `source "$HOME/.cargo/env"` on macOS/Linux.
- `rustc is too old`: run `rustup update stable`; if necessary, run `rustup toolchain install 1.94.1` and `rustup override set 1.94.1` inside the repository.
- `linker 'cc' not found`: install Xcode command-line tools on macOS, `build-essential` on Ubuntu/Debian, Development Tools on Fedora, or Visual Studio Build Tools with C++ on Windows.
- `.env permissions are too open`: run `chmod 600 .env`.
- `PRIVATE_KEY is not set`: confirm the file is named exactly `.env` and run the bot from the repository directory.
- RPC URL errors: use `https://` and `wss://`, make sure both endpoints support the selected chain, and remove unused placeholder values.
- OpenSea authorization errors: verify `OPENSEA_API_KEY` and the drop slug.

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

## 4. Command and runtime reference

### Interactive commands

The recommended real and dry-run commands are:

```bash
./target/release/nft-mint-bot start
./target/release/nft-mint-bot start --dry-run
```

Running through Cargo is supported, but the prebuilt release executable avoids a possible rebuild immediately before a mint:

```bash
cargo run --release
cargo run --release -- start --dry-run
```

After setup succeeds, the bot prints `BOT ARMED`, waits for the selected trigger, signs locally, broadcasts, and monitors the receipt. `Mint value: fetched from OpenSea when the stage is active` is normal before an OpenSea stage opens. A no-subcommand launch is a real run; it is not a dry run.

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
- `refresh_each_block`: refreshes and uses the pending nonce from each block; do not send another wallet transaction between the final refresh and the trigger.
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
