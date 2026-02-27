# Interval Sniper (Rust)

Same logic as the TypeScript Interval Sniper in `src/bot/marketMaker/`: **buy only when the best ask is in a configured range** `[min_buy_price, max_buy_price]`, **one buy per 5-minute interval**, and **sell on take profit and stop loss**.

## Logic

- **Market**: BTC or SOL 5-minute Up/Down (Polymarket). Slug: `btc-updown-5m-{interval_start_unix}` or `sol-updown-5m-{interval_start_unix}`.
- **Entry**: Choose the side (Up or Down) with the **higher best ask** that is inside `[min_buy_price, max_buy_price]` and has enough liquidity. Place a single buy per interval (FAK cross-spread by default).
- **Take profit**: After a fill, if `enable_auto_sell` is set, sell when `best_bid >= take_profit_price` (fixed price from config, or 0.99 if `auto_sell_at_max_price`).
- **Stop loss**: If `enable_stop_loss` is set, sell when `best_bid <= stop_loss_price` (fixed price from config).

No UI; run as a standalone binary.

## Build

```bash
cargo build --release
```

## Run

Copy `.env.example` to `.env` and set at least:

- **Gamma**: `POLYMARKET_REST_BASE` (e.g. `https://gamma-api.polymarket.com`)
- **CLOB** (for order book; required for live orders): `POLYMARKET_CLOB_HOST` (e.g. `https://clob.polymarket.com`)
- **Interval Sniper**: `MM_DRY_RUN=true` (recommended first), `MM_SIZE_SHARES`, `MM_MIN_BUY_PRICE`, `MM_MAX_BUY_PRICE`, `MM_ENABLE_AUTO_SELL`, `MM_TAKE_PROFIT_PRICE`, `MM_ENABLE_STOP_LOSS`, `MM_STOP_LOSS_PRICE`, etc.

Then:

```bash
cargo run
# or
./target/release/sniper
```

## Environment variables

Compatible with the TypeScript bot `MM_*` and `INTERVAL_SNIPER_*` names:

| Variable | Description | Default |
|----------|-------------|---------|
| `INTERVAL_SNIPER_MARKET` | `btc_5m` or `sol_5m` | `btc_5m` |
| `MM_MARKET_SLUG` | Override slug (empty = current 5m) | (dynamic) |
| `MM_SIZE_SHARES` | Max shares to buy per interval | `5` |
| `MM_MIN_BUY_PRICE` | Min ask price to buy (0–1) | `0.9` |
| `MM_MAX_BUY_PRICE` | Max ask price to buy (0–1) | `0.95` |
| `MM_ALLOW_BUY_UP` / `MM_ALLOW_BUY_DOWN` | Allow buying Up/Down | `true` |
| `MM_SECONDS_BEFORE_CLOSE` | Only act when seconds to close ≤ this | `20` |
| `MM_NO_WINDOW_ALL_INTERVALS` | If true, act all interval | `true` |
| `MM_MIN_SECONDS_AFTER_MARKET_OPEN` | No buy in first N seconds | `0` |
| `MM_DRY_RUN` | If true, no real orders | `true` |
| `MM_ENABLE_AUTO_SELL` | Enable take profit | `true` |
| `MM_TAKE_PROFIT_PRICE` | TP: sell when best_bid ≥ this (0–1) | `0.97` |
| `MM_ENABLE_STOP_LOSS` | Enable stop loss | `true` |
| `MM_STOP_LOSS_PRICE` | SL: sell when best_bid ≤ this (0–1) | `0.90` |
| `MM_LOOP_MS` | Loop interval (ms) | `100` |

CLOB/Gamma (same as main polybot): `POLYMARKET_CLOB_HOST` (or `POLYMARKET_CLOB_URL`), `POLYMARKET_REST_BASE`. For **live orders** set `MM_DRY_RUN=false` and:

- `PRIVATE_KEY` or `POLYMARKET_PRIVATE_KEY` — wallet private key (hex, with or without `0x`)
- `API_KEY`, `SECRET`, `PASSPHRASE` — CLOB API credentials (from Polymarket L1 derive)
- `POLYMARKET_CHAIN_ID` — e.g. `137` (Polygon)
- `FUNDER_ADDRESS` — address that holds funds (proxy/Safe); defaults to signer if unset
- `SIGNATURE_TYPE` — `0` EOA, `1` POLY_PROXY, `2` GNOSIS_SAFE (default `2`)
- `MM_NEG_RISK` — `true` for multi-outcome (neg-risk) markets; default `false` for BTC/SOL 5m

## Live orders

**Live order placement is implemented** in this Rust binary: EIP-712 order signing and HMAC L2 auth for the Polymarket CLOB. Set `MM_DRY_RUN=false` and configure `PRIVATE_KEY` (or `POLYMARKET_PRIVATE_KEY`), `API_KEY`, `SECRET`, `PASSPHRASE`, and optionally `FUNDER_ADDRESS` and `SIGNATURE_TYPE`. Use **`MM_DRY_RUN=true`** to run in simulation without sending real orders.

## Reference

- TypeScript implementation: `../src/bot/marketMaker/`
- Spec (HFT-style): `../docs/INTERVAL_SNIPER_RUST_HFT.md`
