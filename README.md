# sniper

HFT bot for Polymarket 5-minute BTC prediction markets.

Buys in the 0.93--0.95 range, takes profit at 0.97, stops loss at 0.90.
All SL/TP sells use `limit = best_bid` (always crossing).
SL uses FAK with immediate retry on partial fills.

## Requirements

- Rust >= 1.88 (MSRV of polymarket-client-sdk)
- A funded Polymarket wallet (USDC on Polygon)
- Either the `token_id` (U256) of the outcome to trade, or **AUTO_BTC5M=1** to auto-rotate to the current BTC 5-min window

## Quick start

**Single market (manual TOKEN_ID):**

```bash
export POLYMARKET_PRIVATE_KEY="0xYOUR_PRIVATE_KEY"
export TOKEN_ID="12345..."   # U256 asset id from Polymarket
# Optional: ORDER_SIZE=100 MAX_POSITION=500 BUY_MIN=0.93 BUY_MAX=0.95 TAKE_PROFIT=0.97 STOP_LOSS=0.90
cargo run --release
```

**Auto-rotate BTC 5-min markets (no TOKEN_ID):**

```bash
export POLYMARKET_PRIVATE_KEY="0xYOUR_PRIVATE_KEY"
export AUTO_BTC5M=1
export OUTCOME=up   # or "down" (default: up)
# Optional: ORDER_SIZE=100 MAX_POSITION=500 BUY_MIN=0.93 BUY_MAX=0.95 TAKE_PROFIT=0.97 STOP_LOSS=0.90
cargo run --release
```

When `AUTO_BTC5M=1`, the bot switches to the next 5-minute market automatically when the current window ends (e.g. from `btc-updown-5m-1771991400` to `btc-updown-5m-1771991700`).

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `POLYMARKET_PRIVATE_KEY` | yes | -- | Hex private key for signing orders |
| `TOKEN_ID` | yes* | -- | U256 token/asset ID of the outcome to trade (*omit when AUTO_BTC5M=1) |
| `POLYMARKET_API_KEY` | no | -- | CLOB API key (UUID). If set, also set `_SECRET` and `_PASSPHRASE` to use that account’s balance |
| `POLYMARKET_API_SECRET` | no | -- | CLOB API secret (required if `POLYMARKET_API_KEY` is set) |
| `POLYMARKET_API_PASSPHRASE` | no | -- | CLOB API passphrase (required if `POLYMARKET_API_KEY` is set) |
| `AUTO_BTC5M` | no | 0 | Set to `1` or `true` to auto-rotate to the current BTC 5-min market every 5 minutes |
| `OUTCOME` | no | up | When AUTO_BTC5M=1: `up` or `down` (which outcome to trade) |
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

## How to find the token_id (when not using AUTO_BTC5M)

1. Open the market on [polymarket.com](https://polymarket.com).
2. Open browser DevTools -> Network tab.
3. Look for requests to `clob.polymarket.com` containing `token_id` or `asset_id`.
4. Alternatively, use the Gamma API: `GET https://gamma-api.polymarket.com/markets/slug/{slug}` and read the `clobTokenIds` array (first = Up, second = Down).

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

### Why the bot buys but does not sell (SL/TP): "not enough balance / allowance"

The bot **trades with your Polymarket (Safe) wallet**. To **buy**, the Safe needs USDC and
USDC allowance to the exchange. To **sell** (SL/TP), the Safe needs:

1. **Outcome token balance** (the shares you bought — e.g. 5.02 Down).
2. **Conditional Token (CTF) approval** — the Safe must have called
   `setApprovalForAll(exchange, true)` on the Conditional Tokens (ERC-1155) contract.
   Without this, the exchange cannot debit your outcome tokens and the CLOB returns
   "not enough balance / allowance" on sell orders.

So: **buy** = USDC + USDC allowance; **sell** = outcome tokens + **CTF setApprovalForAll**.

- If you use the **Safe** (browser wallet): the **Safe** must have given both approvals.
  Usually the first time you trade on [polymarket.com](https://polymarket.com) (buy or sell)
  the site prompts you to approve USDC and Conditional Tokens for the Safe. If you only
  ever bought from the UI and never sold, the Safe might have USDC approval but not CTF.
  **Fix:** Open Polymarket in the browser, go to the same market, and try to place a small
  sell (or open the market and accept any approval prompt). That will set CTF approval for
  the Safe. Then the bot will be able to sell at SL/TP.
- If you use a **plain EOA** (no Safe): run the
  [approvals example](https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs)
  once. It sets both USDC and CTF approvals for the EOA.

**Check balances and approvals (read-only):**

```bash
cargo run --bin check_balance
```

This prints your **EOA**, the **Polymarket trading wallet (Safe)**, USDC balance/allowance,
and **CTF approved for sell** (Safe only). If "CTF approved (sell): false" for the Safe,
do the browser approval step above or run the SDK approvals from the Safe if you have a flow for it.

### "Not enough balance / allowance" but I have balance (EOA)

The CLOB rejects orders when **either** your USDC **balance** on Polygon is too low
**or** your **allowance** (permission for the exchange contract to spend USDC) is
missing or too low. With an EOA wallet you often have balance but **allowance = 0**
until you run the approvals once.

**Check what the chain sees (read-only, no gas, no orders):**

```bash
cargo run --bin check_balance
```

This prints your **EOA** address (from your private key), the **Polymarket trading wallet**
(Gnosis Safe derived from it), and USDC balance/allowance for each. If you use
Phantom or MetaMask with Polymarket, the site uses the Safe; the bot now does too
(see below).

If you have balance but allowance is 0 (EOA only), run the
[approvals example](https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs)
once (same `POLYMARKET_PRIVATE_KEY`, same network).

### Bot uses your Polymarket (Safe) balance

If you connected Polymarket with a **browser wallet** (Phantom, MetaMask, etc.),
the site uses a **derived wallet** (Gnosis Safe), not your EOA. The bot is
configured to use **SignatureType::GnosisSafe**, so it trades with the same
account and balance you see on Polymarket. No need to move funds to the EOA.

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
  main.rs        -- entry point, WS event loop, tick handler, AUTO_BTC5M rotation
  config.rs      -- environment / CLI configuration
  gamma.rs       -- Gamma API: resolve token_id for btc-updown-5m-{window} slug
  orderbook.rs   -- book state with monotonic stale detection
  position.rs    -- local expected position tracking
  dedupe.rs      -- intent deduplication (kind + size, TTL)
  strategy.rs    -- SL > TP > Buy evaluation with early return
  execution.rs   -- SDK wrapper: order placement, cancellation, REST book
```
