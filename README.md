# Momentum + RL Sniper (Polymarket BTC 5m)

HFT momentum bot for Polymarket BTC Up/Down 5-minute markets, with optional
reinforcement-learning (Q-learning) threshold tuning in paper mode.

## How it works

1. **CEX spot feeds** — Binance `aggTrade` and Coinbase `matches` WebSockets stream
   real-time BTC prices into per-asset ring buffers.
2. **Multi-venue anchor** — At each 5m interval open, REST calls to 7 exchanges
   (Binance, Coinbase, Kraken, Bybit, OKX, Bitfinex, Bitstamp) produce a robust
   reference price.
3. **Momentum signal** — The percentage move from anchor to latest price is mapped
   to a fair `P(Up)` via a logistic function, gated by volume and probability
   thresholds.
4. **Polymarket execution** — The bot resolves active BTC 5m slugs via the Gamma
   API, reads best asks from the CLOB WebSocket orderbook, and enters when the
   momentum-implied edge exceeds `edge_min`.
5. **Optional arb** — When `YES + NO` ask sum is low enough, a delta-neutral
   both-sides entry is placed.
6. **Paper / Live** — `paper` mode simulates fills against the live orderbook;
   `live` mode posts real orders via `polymarket-client-sdk` (EIP-712 auth).
7. **RL tuning (paper only)** — When `[adaptive_paper.rl]` is enabled, a tabular
   Q-learning agent adjusts momentum thresholds (`delta_up_pct`, `delta_down_pct`,
   `edge_min`) each 5m interval, learning from PnL and TP/SL statistics.

## Build

```bash
cargo build --release
```

## Run

```bash
cp config.example.toml config.toml   # edit with your settings
cargo run --release -- config.toml
```

If you omit the argument, the binary defaults to `config.toml` in the current
working directory.

## Configuration

All settings live in `config.toml`. See `config.example.toml` for the full
reference with comments.

| Section | Purpose |
|---------|---------|
| `mode` | `paper` or `live` |
| `[paper]` | Virtual USDC balance for paper trading |
| `[momentum]` | Window, delta thresholds, volume gate, probability scale |
| `[trading]` | Edge min, tick rate, TP/SL, TIF, spread/staleness guards |
| `[risk]` | Per-trade sizing, daily drawdown, kill switch |
| `[adaptive_paper]` | Interval reports, lag logging, heuristic threshold tuning |
| `[adaptive_paper.rl]` | Q-learning agent hyperparameters |

### Environment variables (optional)

| Variable | Default | Description |
|----------|---------|-------------|
| `SNIPER_LOG` | `sniper.log` | Log file path |
| `RUST_LOG` | unset → `warn,sniper=info` | Quiets non-bot crates; sniper stays at `info`. Momentum diagnostics: e.g. `RUST_LOG=sniper::strategy::momentum=trace,sniper=info` |

## Live trading

Provide in `config.toml`:

- `private_key_polygon` — Polygon wallet private key hex
- `signature_type` — `eoa`, `proxy`, or `gnosissafe`
- `starting_balance_usdc` — Required in live mode

## VPS / latency tips

1. Run on a low-latency VPS near Polymarket / CEX routing.
2. Start in `paper` mode; switch to `live` once fills behave as expected.
3. Key tuning knobs: `momentum.window_sec`, `trading.tick_ms`,
   `trading.edge_min`, `momentum.delta_up_pct` / `delta_down_pct`.
