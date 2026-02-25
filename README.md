# sniper

HFT bot for Polymarket 5-minute BTC prediction markets.

Buys in the 0.93--0.95 range, takes profit at 0.97, stops loss at 0.90.
All SL/TP sells use `limit = best_bid` (always crossing).
SL uses FAK with immediate retry on partial fills.

## Requirements

- Rust >= 1.88 (MSRV of polymarket-client-sdk)
- A funded Polymarket wallet (USDC on Polygon)
- The `token_id` (U256) of the YES/NO outcome you want to trade

## Quick start

```bash
# Set required env vars
export POLYMARKET_PRIVATE_KEY="0xYOUR_PRIVATE_KEY"
export TOKEN_ID="12345..."   # U256 asset id from Polymarket

# Optional env vars (with defaults shown)
export ORDER_SIZE=100
export MAX_POSITION=500
export BUY_MIN=0.93
export BUY_MAX=0.95
export TAKE_PROFIT=0.97
export STOP_LOSS=0.90
export DEDUPE_TTL_MS=50       # 20-80 ms recommended
export STALE_THRESHOLD_MS=200 # 100-250 ms for 5-min BTC
export POLYMARKET_CLOB_URL=https://clob.polymarket.com

cargo run --release
```

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `POLYMARKET_PRIVATE_KEY` | yes | -- | Hex private key for signing orders |
| `TOKEN_ID` | yes | -- | U256 token/asset ID of the outcome to trade |
| `ORDER_SIZE` | no | 100 | Size per order (shares for sells, USDC for buys) |
| `MAX_POSITION` | no | 500 | Maximum position in shares |
| `BUY_MIN` | no | 0.93 | Lower bound of buy range |
| `BUY_MAX` | no | 0.95 | Upper bound of buy range |
| `TAKE_PROFIT` | no | 0.97 | Best bid threshold to trigger TP sell |
| `STOP_LOSS` | no | 0.90 | Best bid threshold to trigger SL sell |
| `DEDUPE_TTL_MS` | no | 50 | Dedup window for same-intent orders (ms) |
| `STALE_THRESHOLD_MS` | no | 200 | Book age before REST fallback for SL/TP (ms) |
| `POLYMARKET_CLOB_URL` | no | https://clob.polymarket.com | CLOB endpoint |
| `RUST_LOG` | no | sniper=info | Log level filter |

## How to find the token_id

1. Open the market on [polymarket.com](https://polymarket.com).
2. Open browser DevTools -> Network tab.
3. Look for requests to `clob.polymarket.com` containing `token_id` or `asset_id`.
4. Alternatively, use the Gamma API: `GET https://gamma-api.polymarket.com/markets/slug/{slug}` and read the `tokens[].token_id` field.

## Trading logic

```
SL > TP > Buy  (strict priority, early return per tick)

SL:  best_bid <= 0.90  ->  sell FAK at limit = best_bid
     partial fill?     ->  retry remainder immediately (dedupe allows: size changed)

TP:  best_bid >= 0.97  ->  sell FOK at limit = best_bid

Buy: 0.93 <= price <= 0.95, book not stale, position < max
     1 live GTC order, cancel/replace when price moves > 1 tick
```

Stale-book gate: if the book hasn't been updated in `STALE_THRESHOLD_MS`, the bot
fetches a fresh snapshot via REST before executing SL/TP. Buy orders are blocked
entirely while the book is stale.

## Token allowances (EOA wallets)

If your private key is an EOA (MetaMask, hardware wallet), you must approve
USDC and Conditional Tokens for the Polymarket exchange contracts **once** before
trading. See the
[SDK approvals example](https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs).

Proxy/Safe wallets (email login, browser extension) do **not** need manual
approvals.

## Architecture

```
WebSocket (market channel)
    |
    v
OrderBook state (best_bid, best_ask, monotonic timestamp)
    |
    v
Strategy: evaluate(SL > TP > Buy) -> Action
    |
    v
Execution: post_order / cancel_order / GET /book
    |
    v
Position (local expected) + Dedupe (intent + size, TTL)
```

Single async loop -- no channels, no extra threads.
All decisions and order sends happen inline on each WS event.

## Project structure

```
src/
  main.rs        -- entry point, WS event loop, tick handler
  config.rs      -- environment / CLI configuration
  orderbook.rs   -- book state with monotonic stale detection
  position.rs    -- local expected position tracking
  dedupe.rs      -- intent deduplication (kind + size, TTL)
  strategy.rs    -- SL > TP > Buy evaluation with early return
  execution.rs   -- SDK wrapper: order placement, cancellation, REST book
```
