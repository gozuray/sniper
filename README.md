# Sniper — Momentum + RL (Polymarket BTC 5m)

Bot de trading de **momentum en tiempo casi real** sobre los mercados **Bitcoin Up/Down de 5 minutos** de Polymarket. Compara el movimiento reciente del spot en CEX con el precio de referencia al abrir la vela 5m, traduce eso en una probabilidad **fair** de “Up”, y entra en el lado del libro cuando el **edge** frente al mejor ask supera un mínimo. Incluye modo **paper** (simulación), modo **live** (órdenes reales vía SDK), **arbitraje YES+NO** opcional y **ajuste de umbrales** en paper mediante heurísticas o **Q-learning (RL)**.

---

## Qué hace el bot (vista de pájaro)

1. **Feeds CEX en vivo** — WebSockets de **Binance** (`aggTrade`) y **Coinbase** (`matches`) alimentan *ring buffers* por activo con precio, volumen en quote y flujo **taker buy/sell**.
2. **Ancla multi-venue al inicio de cada franja 5m** — Con REST se consultan hasta **7 exchanges** (Binance, Coinbase, Kraken, Bybit, OKX, Bitfinex, Bitstamp); la **media** se guarda como `binance_5m_open` en el estado (precio de referencia del intervalo) y se avisa si difiere mucho del spot WS (posible ancla defectuosa).
3. **Sincronía con Polymarket** — La API **Gamma** resuelve slugs activos (`btc-updown-5m-*`), token IDs UP/DOWN y ventanas `[interval_start, close_time)`. Al cambiar de franja 5m se **reconecta el WebSocket del libro** (CLOB) para los nuevos tokens.
4. **Señal momentum** — Sobre una ventana rodante (`momentum.window_sec`) se mide el cambio porcentual; umbrales pueden ser **en USD** (`delta_*_usd`, convertidos con el precio ancla) o en **fracción** (`delta_*_pct`). Se exige volumen mínimo, **desequilibrio taker** alineado con la dirección (`min_taker_imbalance`) y probabilidad “fuerte” vía **logística** (`prob_scale`, `strong_prob_threshold`).
5. **Política CEX** — `[cex].mode`: `auto` (Binance salvo obsoleto → Coinbase), `binance_only`, `coinbase_only`. `[cex].momentum_policy`: **`primary`** (una fuente) o **`consensus`** (Binance y Coinbase deben dar la **misma** dirección Up/Down con ambos feeds frescos).
6. **Entrada en el mercado** — Del libro WS se leen **best ask/bid** por outcome. Si el edge `fair − ask` (lado elegido) ≥ `trading.edge_min` y pasan filtros de **spread** y **libro no obsoleto**, se emite una señal al **OrderManager**.
7. **Arbitraje pareado (opcional)** — Si `yes_no_arb_enabled` y `p_up + p_down ≤ arb_yes_no_sum_max`, puede evaluarse entrada **YES+NO** (delta-neutral) en lugar de solo momentum.
8. **Ejecución y gestión** — El **OrderManager** recibe señales por canal, coloca límites con TIF configurable (**FAK** por defecto en entradas agresivas, **GTC**/post-only si lo configuras), reconcilia **fills** vía WebSocket de usuario (live) o simula fills en paper, aplica **TP/SL/trailing**, **cooldowns** y **settle** al cierre del intervalo usando precios spot de apertura/cierre de la franja.
9. **Riesgo** — Tamaño por trade (`risk_per_trade_frac`), **kill switch** por drawdown diario simulado, límite de posiciones por mercado.
10. **Paper lab (solo `mode = paper` + `[adaptive_paper]`)** — Informes por intervalo, `analysis.jsonl` (lag CEX vs libro, impulso reciente, `pct_vs_anchor`), tuning heurístico de deltas o **RL** tabular con persistencia de Q-table.

---

## Arquitectura del código

| Módulo | Rol |
|--------|-----|
| `src/main.rs` | Arranque: config, tracing, feeds CEX, descubrimiento Gamma, WS libro, bucle de señales ~`tick_ms`, diagnóstico momentum periódico, Ctrl+C → shutdown. |
| `src/cex/` | Binance/Coinbase WS, helpers REST multi-venue para la ancla 5m. |
| `src/strategy/momentum.rs` | Cálculo de snapshot momentum, `evaluate_market_signal`, `evaluate_arb_both`, merge **consensus**. |
| `src/polymarket/` | Cliente live/paper, WS orderbook, utilidades de mercados y slugs. |
| `src/execution/order_manager.rs` | Entradas, salidas, PnL, paper/live, integración **PaperLab**/RL stats. |
| `src/paper_lab/` | Métricas, `analysis.jsonl`, tuning adaptativo y envoltorio RL. |
| `src/rl/` | Agente Q-learning (acciones sobre deltas, edge, imbalance, TP/SL, etc.). |
| `src/config.rs` | Deserialización TOML, validación y defaults. |

---

## Flujo en tiempo de ejecución

1. **Bootstrap** — Carga `config.toml`, fija ancla BTC 5m (Gamma si disponible, si no franja ET local), resuelve mercados en horizonte (`subscription_horizon_intervals`), arranca WS de libro y — si aplica — **snapshot CEX** de arranque (`log_btc_5m_window_snapshot` “arranque”).
2. **Bucle principal** — Cada tick: actualiza mercados si cambió la franja Polymarket (`sync_polymarket_5m_interval_if_needed`), refresca ancla en **ventana nueva**, computa momentum según política CEX, opcionalmente merge consensus, evalúa arb o señal momentum vs asks, envía `Signal` al **OrderManager**.
3. **Fin de intervalo** — Precios de apertura/cierre por franja en `SpotIntervalState` alimentan el **settle** de posiciones abiertas.

---

## Configuración

Copia la plantilla y edita:

```bash
cp config.example.toml config.toml
```

Claves habituales (el detalle está comentado en `config.example.toml`):

| Sección | Función |
|---------|---------|
| `mode` | `paper` \| `live` |
| `[paper]` | Balance virtual USDC (paper). |
| `[momentum]` | `window_sec`, `delta_*_usd` / `delta_*_pct`, `min_quote_volume_window`, `prob_scale`, `strong_prob_threshold`, `min_taker_imbalance`. |
| `[trading]` | `edge_min`, `tick_ms`, TP/SL ticks, TIF, spread máximo, antigüedad máxima del libro, cooldowns, arb YES+NO. |
| `[cex]` | `mode`, `max_feed_staleness_ms`, `momentum_policy` (`primary` \| `consensus`). |
| `[risk]` | Fracción por trade, drawdown diario, kill switch. |
| `[adaptive_paper]` | Informes, `impulse_window_ms`, `impulse_min_pct`, análisis JSONL, tuning. |
| `[adaptive_paper.rl]` | Hiperparámetros Q-learning y paths de persistencia. |

**Nota:** en `assets` puedes listar otros símbolos; el bot **solo opera Polymarket BTC 5m** y avisará si ignora el resto.

---

## Variables de entorno

| Variable | Valor por defecto | Uso |
|----------|-------------------|-----|
| `SNIPER_LOG` | `sniper.log` | Ruta del fichero de log además de consola. |
| `RUST_LOG` | `warn,sniper=info` | Niveles `tracing`. Ej.: `RUST_LOG=sniper=trace` para ver rechazos `mom · ✗ edge`, `✗ spread`, `✗ delta`, etc. |

En PowerShell, el filtro literal `mom ·` puede fallar por codificación; usa por ejemplo `"mom {0}" -f [char]0x00B7` o extrae líneas con `Select-String "edge\|spread\|mom"` tras lanzar con trace.

---

## Compilar y ejecutar

```bash
cargo build --release
cargo run --release -- config.toml
```

Sin argumento, el binario usa `config.toml` en el directorio de trabajo actual.

---

## Modo live

En `config.toml` deben figurar, como mínimo:

- `mode = "live"`
- `private_key_polygon` — clave privada en hex (¡no subas esto al repo; usa `config.toml` en `.gitignore` o secretos locales).
- `signature_type` — `eoa`, `proxy` o `gnosissafe`
- `starting_balance_usdc` — referencia para sizing y drawdown

Endpoints CLOB/Gamma pueden sobreescribirse en `[endpoints]` si tu despliegue lo requiere.

---

## Logs y diagnóstico

- **INFO** `mom · detect · binance|coinbase` — snapshot que pasó filtros internos de momentum.
- **TRACE** `mom · ✗ …` — motivo de rechazo (delta, imbalance, prob, edge, spread, intervalo).
- **INFO** `BTC 5m · arranque|ventana_nueva` — bloque con venues y `precio_ref`; **WARN** si REF vs spot WS discrepa &gt; ~100 USD (ancla sospechosa).
- **Paper** — Directorio `paper_reports/` (según config; suele estar en `.gitignore`) con JSON por intervalo y `analysis.jsonl`.

---

## Buenas prácticas operativas

1. **VPS** cercano a rutas bajas latencia hacia Polymarket y exchanges.
2. Probar **paper** + `adaptive_paper` hasta que métricas y fills simulados encajen con tu tolerancia al riesgo.
3. Ajustar `edge_min`, ventana momentum y deltas antes de live; en **consensus** habrá menos señales pero más acuerdo entre venues.

---

## Licencia y responsabilidad

Este software es con fines educativos y de investigación. El trading implica riesgo de pérdidas totales; no hay garantía de rentabilidad. Revísalo y despliégalo bajo tu propia responsabilidad.
