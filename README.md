# Interval Sniper (Rust)

Same logic as the TypeScript Interval Sniper in `src/bot/marketMaker/`: **buy only when the best ask is in a configured range** `[min_buy_price, max_buy_price]`, **one buy per 5-minute interval**, and **sell on take profit and stop loss**.

## Logic

- **Market**: BTC or SOL 5-minute Up/Down (Polymarket). Slug: `btc-updown-5m-{interval_start_unix}` or `sol-updown-5m-{interval_start_unix}`.
- **Entry**: Choose the side (Up or Down) with the **higher best ask** that is inside `[min_buy_price, max_buy_price]` and has enough liquidity. Place a single buy per interval (FAK cross-spread by default).
- **Take profit**: After a fill, if `enable_auto_sell` is set, sell when `best_bid >= target_price` (target = entry × (1 + profit%) or 0.99 if `auto_sell_at_max_price`).
- **Stop loss**: If `enable_stop_loss` is set, sell when `best_bid <= trigger_price` (trigger = entry × (1 - stop_loss%)).

No UI; run as a standalone binary.

## Build

```bash
cargo build --release
```

## Run

Copy `.env.example` to `.env` and set at least:

- **Gamma**: `POLYMARKET_REST_BASE` (e.g. `https://gamma-api.polymarket.com`)
- **CLOB** (for order book; required for live orders): `POLYMARKET_CLOB_HOST` (e.g. `https://clob.polymarket.com`)
- **Interval Sniper**: `MM_DRY_RUN=true` (recommended first), `MM_SIZE_USD`, `MM_MIN_BUY_PRICE`, `MM_MAX_BUY_PRICE`, `MM_ENABLE_AUTO_SELL`, `MM_AUTO_SELL_PROFIT_PERCENT`, `MM_ENABLE_STOP_LOSS`, `MM_STOP_LOSS_PERCENT`, etc.

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
| `MM_SIZE_USD` | Max USD per interval | `5` |
| `MM_MIN_BUY_PRICE` | Min ask price to buy (0–1) | `0.9` |
| `MM_MAX_BUY_PRICE` | Max ask price to buy (0–1) | `0.95` |
| `MM_ALLOW_BUY_UP` / `MM_ALLOW_BUY_DOWN` | Allow buying Up/Down | `true` |
| `MM_SECONDS_BEFORE_CLOSE` | Only act when seconds to close ≤ this | `20` |
| `MM_NO_WINDOW_ALL_INTERVALS` | If true, act all interval | `true` |
| `MM_MIN_SECONDS_AFTER_MARKET_OPEN` | No buy in first N seconds | `0` |
| `MM_DRY_RUN` | If true, no real orders | `true` |
| `MM_ENABLE_AUTO_SELL` | Enable take profit | `true` |
| `MM_AUTO_SELL_PROFIT_PERCENT` | TP % above entry | `5` |
| `MM_ENABLE_STOP_LOSS` | Enable stop loss | `true` |
| `MM_STOP_LOSS_PERCENT` | SL % below entry | `7` |
| `MM_LOOP_MS` | Loop interval (ms) | `100` |

CLOB/Gamma (same as main polybot): `POLYMARKET_CLOB_HOST`, `POLYMARKET_REST_BASE`, and for live orders: `PRIVATE_KEY`, `API_KEY`, `SECRET`, `PASSPHRASE`, `POLYMARKET_CHAIN_ID`.

## Live orders

**Live order placement is not implemented in this Rust binary** (EIP-712 signing for the CLOB is required). Use **`MM_DRY_RUN=true`** to run the same logic in simulation, or use the **TypeScript Interval Sniper** (Next.js app) for real orders.

## Reference

- TypeScript implementation: `../src/bot/marketMaker/`
- Spec (HFT-style): `../docs/INTERVAL_SNIPER_RUST_HFT.md`
