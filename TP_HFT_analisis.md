# Análisis: Take Profit (TP) y recomendaciones HFT

---

## Cómo detecta el bot que la compra se ejecutó (en tiempo real)

El bot **no espera a la blockchain** para saber si la compra se ejecutó. Usa dos fuentes en tiempo real:

### 1. Respuesta REST al colocar la orden

Al llamar a `place_limit_order` (compra), el CLOB de Polymarket devuelve un JSON. De ahí el código obtiene:

- **`success`**: si la orden fue aceptada.
- **`order_id`**: ID de la orden.
- **`filled_size`** (para BUY): sale del campo **`takingAmount`** de la respuesta (cantidad en 6 decimales, convertida a shares).  
  - Si la orden es **FAK/FOK** y hace **match al instante**, el API suele devolver `takingAmount` = lo que se llenó.  
  - Si es **GTC** y queda en libro, muchas veces la respuesta no trae fill → `filled_size` queda `None`.

En `clob.rs`, para BUY se hace:  
`filled_size = taker_6dec_opt.map(|t| t / 1_000_000)` (takingAmount en 6 decimals → shares).

### 2. WebSocket canal user (eventos “matched”)

El bot se conecta al **WebSocket user** (`wss://.../ws/user`) y recibe eventos en vivo:

- **Evento `order`**: actualización de una orden (PLACEMENT, UPDATE, CANCELLATION). Trae:
  - **`size_matched`**: cantidad ya ejecutada de esa orden.
  - **`original_size`**, **`side`** (BUY/SELL), **`asset_id`**, **`type`**.
- **Evento `trade`**: un trade ejecutado. Trae:
  - **`status`**: si es **`"MATCHED"`** = el trade se ejecutó en el CLOB.
  - **`size`**: cantidad llenada en ese trade.
  - **`side`** (BUY/SELL), **`price`**, **`asset_id`**, **`taker_order_id`** / **`maker_order_id`** (nuestra orden).

Cuando llega un **trade** con `status == "MATCHED"` y `side == "BUY"`, el código en `clob_ws_user.rs`:

1. Suma ese `size` a **`confirmed_buy`** por `asset_id` (para saber “cuánto comprado” por token).
2. Actualiza el estado de la orden por `order_id`: **`size_matched`** = acumulado de todos los trades de esa orden.

Así el bot sabe **en tiempo real** que la compra se recibió (se matcheó en el CLOB), sin esperar balance en REST ni blockchain.

### ¿“Matched” devuelve los valores del FAK?

- **MATCHED** es el **estado del trade en el CLOB**: “este trade se ejecutó”. No es un mensaje de blockchain; Polymarket usa un order book centralizado (CLOB) y el settlement on-chain puede ser posterior.
- Los **valores** que usa el bot son:
  - **REST**: `takingAmount` de la respuesta de la orden → **`filled_size`** en `PlaceOrderResult`. Para **FAK**, si hay match inmediato, el API suele llenar este campo.
  - **WS**: en eventos **trade**, el campo **`size`** (y por orden, **`size_matched`**) son la cantidad realmente ejecutada. Esos son los mismos “valores del FAK” en el sentido de “cuánto se llenó”.
- Para una **compra FAK**: a menudo el API no devuelve `filled_size` (o lo devuelve vacío), entonces el bot usa el **WS**: al poco de enviar la orden llega un evento **trade** con status MATCHED y el `size` llenado, y con eso actualiza `size_matched` y `confirmed_buy` y considera la compra recibida.

### Resumen

| Fuente      | Qué aporta |
|------------|------------|
| **REST**   | `takingAmount` → `filled_size` en la respuesta de la orden (sobre todo cuando FAK/FOK hace match al instante). |
| **WS trade** | `status: "MATCHED"`, `size` → se acumula en `size_matched` por orden y en `confirmed_buy` por token. |
| **WS order** | `size_matched` en eventos ORDER (UPDATE, etc.) para esa orden. |

La detección es **en tiempo real vía CLOB (REST + WS)**, no vía confirmación en blockchain. Los valores que usa para TP/SL son esos: lo que el CLOB reporta como ejecutado (`takingAmount` / `size_matched` / `size` en trade MATCHED).

---

## ¿Está fallando el TP?

**Sí, puede fallar** en estos casos (ya documentados en `bug_analysis.md`):

### 1. Error "invalid amounts"
- **Qué pasa**: Polymarket rechaza la orden con `"invalid amounts, maker and taker amount must be higher than 0"`.
- **Por qué**: 
  - Se envía un **size** que el exchange considera inválido (por debajo de su mínimo real).
  - En el código, `MIN_SELL_SIZE = 0.0001` y `DUST_THRESHOLD = 0.001`, pero el CLOB de Polymarket suele exigir **mínimo 5** en tamaño de orden (`CLOB_DEFAULT_MIN_ORDER_SIZE = 5`).
  - Si `effective_sell_size()` devuelve algo entre 0.0001 y 5 (por balance bajo o redondeo), la API puede rechazar.
- **Dónde**: En `runner.rs` se valida `size >= MIN_SELL_SIZE` (0.0001) antes de enviar, pero **no** se exige `size >= CLOB_DEFAULT_MIN_ORDER_SIZE` (5) en todas las ramas. Si en algún camino se manda un size &lt; 5, el exchange devuelve "invalid amounts".

### 2. Lag balance / allowance
- Tras un **fill** (compra), el WebSocket ya sabe que tienes posición, pero el **REST** (balance/allowance) puede tardar unos cientos de ms en actualizarse.
- Si colocas el TP **muy pronto**, el CLOB puede responder "not enough balance / allowance".
- El código ya reintenta cada 200 ms (`TP_SL_BALANCE_RETRY_MS`) y usa WS cuando está disponible; aun así, en picos de latencia puede fallar varias veces.

### 3. TP como orden GTC (siempre)
- En el código, las órdenes de **Take Profit** se envían siempre como **GTC** (Good-Til-Cancel), no se usa `config.take_profit_time_in_force`.
- Para un bot tipo HFT, **GTC** implica: colocar limit y esperar a que alguien cruce. Si el precio se mueve antes, la orden puede quedar sin ejecutar o hay que cancelar y recolocar.

---

## Resumen en una frase

El TP puede fallar por: (1) enviar un size &lt; mínimo real del CLOB → "invalid amounts", (2) colocar el TP antes de que REST refleje el balance → "not enough balance", (3) usar solo GTC para TP → más latencia y menos estilo “ejecutar ya” que en HFT.

---

## Recomendaciones para que sea más HFT y el TP falle menos

### 1. Validar size mínimo del CLOB antes de enviar TP
- **Recomendación**: No enviar nunca una orden de TP con `size < CLOB_DEFAULT_MIN_ORDER_SIZE` (5).
- En todas las ramas donde se llama `place_sell_order` para TP, asegurar algo como:
  - `size >= CLOB_DEFAULT_MIN_ORDER_SIZE` (y si el “position remaining” es menor que 5, tratarlo como dust y cerrar posición por TP/SL con el tamaño ya vendido, o no enviar esa orden).
- Así se evita en la práctica el error "invalid amounts" por tamaño demasiado pequeño.

### 2. Usar `take_profit_time_in_force` de la config
- **Recomendación**: Usar `config.take_profit_time_in_force` (FAK / FOK / GTC) en lugar de hardcodear `SellOrderTimeInForce::Gtc` en todas las llamadas de TP.
- **HFT**:
  - **FAK**: “Fill and kill” → ejecuta lo que pueda al instante y cancela el resto. Bueno para salir rápido cuando el bid toca el precio de TP.
  - **FOK**: “Fill or kill” → todo o nada al instante. Si no hay liquidez, no dejas orden resting.
  - **GTC**: deja orden en libro; mejor para no perder precio, pero más lento y dependiente de que el precio se mantenga.

Para un bot HFT, suele ser mejor **FAK** (o FOK si quieres todo-o-nada) en TP, y reservar GTC para casos en que quieras priorizar precio sobre velocidad.

### 3. Reducir latencia del loop (si el exchange aguanta)
- **Recomendación**: `MM_LOOP_MS` ya está entre 1–500 ms (por defecto 100). Para reaccionar más rápido al book (best_bid >= TP), puedes bajar a 50 ms o menos.
- Cuidado: más ticks por segundo = más llamadas a REST/WS; revisar límites de rate y que no empeore el “balance lag”.

### 4. Reintentos de balance un poco más agresivos (opcional)
- **Recomendación**: Mantener o bajar un poco `TP_SL_BALANCE_RETRY_MS` (ej. 100–150 ms) para que tras un fill el primer intento de TP llegue antes, asumiendo que invalidas bien el caché de allowance/balance en cada error (ya se hace en parte).
- No bajar demasiado para no saturar al CLOB cuando el balance aún no está actualizado.

### 5. Priorizar WebSocket para saber “cuándo colocar TP”
- El código ya prioriza el WS para el fill de la compra y para el estado de balance cuando hay WS. Mantener esa prioridad y, si hay datos de WS que indiquen “ya tengo tamaño X”, usar ese X para el size de TP (respetando el mínimo 5 del CLOB) puede reducir intentos con balance 0 y “invalid amounts”.

---

## Cambios concretos sugeridos (resumen)

| Qué | Dónde | Objetivo |
|-----|--------|----------|
| No enviar TP con size &lt; 5 | Todas las `place_sell_order` de TP en `runner.rs` | Evitar "invalid amounts" |
| Usar `config.take_profit_time_in_force` | Sustituir `SellOrderTimeInForce::Gtc` por el valor de config en las llamadas de TP | HFT: FAK/FOK para salir al instante |
| (Opcional) Bajar `MM_LOOP_MS` | `.env` / config | Reacción más rápida al best_bid |
| (Opcional) Ajustar `TP_SL_BALANCE_RETRY_MS` | Constante en `runner.rs` | Menos espera entre reintentos de TP tras fill |

Con esto el TP debería fallar menos y el comportamiento acercarse más a un bot HFT (reacción rápida, salida al precio de TP con FAK/FOK si lo configuras así).
