# Security policy

This project signs and broadcasts transactions from a locally supplied wallet key. Treat every security report as potentially wallet-impacting.

## Reporting a vulnerability

Do not include private keys, API keys, RPC credentials, seed phrases, or funded-wallet details in a report. Use the repository's **Security → Report a vulnerability** flow to open a private GitHub security advisory. If that option is unavailable, contact the repository owner privately before sharing technical details.

Include the affected commit, reproduction steps using a test-only wallet, expected impact, and a suggested remediation when possible. Never test a report against another person's wallet, production mint, or contract without explicit authorization.

## Operator safety

- Use a dedicated low-balance mint wallet.
- Keep `.env` mode `0600` and never commit it.
- Use encrypted `https://` and `wss://` RPC endpoints.
- Set both mint-price and gas-cost caps.
- Treat the gas-cost setting as a pre-broadcast budget. Ink includes buffered L1 data and operator fees and fails closed if its fee oracle cannot be read, but inclusion-time surcharges can still change. Other custom L2 surcharge models are not covered.
- Verify the contract address, drop slug, quantity, and armed summary before leaving the bot unattended.
