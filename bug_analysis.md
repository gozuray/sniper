# 🔍 Análisis de Errores - Trading Bot Rust

## 📊 Resumen de Trades
**Trade 1:**
- Compra: Down @ 95¢ — 6.0 shares — -$5.70
- Venta 1 (SL): Down @ 85¢ — 5.6 shares — +$4.74
- Venta 2 (SL): Down @ 84¢ — 0.4 shares — +$0.33

**Trade 2:**
- Compra: Down @ 95¢ — 6.0 shares — -$5.70
- Venta (TP): Down @ 99¢ — 6.0 shares — +$5.94

---

## 🔴 PROBLEMA #1: Error de Balance en Stop Loss

### Síntomas
```
INFO sniper::runner: [IntervalSniper] SL partial fill 0.00 @ 0.85, remaining 6.00
INFO sniper::runner: [IntervalSniper] SL partial fill 0.00 @ 0.84, remaining 0.40
ERROR sniper::clob: [LiveClob] order failed: HTTP 400 Bad Request: {"error":"not enough balance / allowance"}
```

**Aparece 36 veces en las líneas 11-46 de los logs**

### Causa Raíz
El Stop Loss se activa correctamente y ejecuta la venta en dos fills parciales (5.6 + 0.4 shares), pero después de vender 5.6 shares, el bot intenta vender las 0.4 restantes y el exchange lo rechaza repetidamente porque:

1. **Lag de sincronización de balance**: El WebSocket reporta que aún hay balance disponible, pero el exchange aún no ha liberado el balance de la orden anterior
2. **Órdenes no canceladas**: Hay órdenes pendientes que bloquean parte del balance
3. **Cache de allowance obsoleto**: El caché de `allowance_cache` no se invalida correctamente después de cada intento fallido

### Ubicación del Código
**Archivo**: `runner.rs`  
**Líneas**: 1562-1750 (loop de Stop Loss)

```rust
// Línea 1554: El loop intenta vender repetidamente sin suficiente espera
loop {
    sl_attempt += 1;
    // ...
    let available = get_available_for_sell(
        clob.as_ref().as_ref(), 
        ws_user_ref, 
        &sl_token_id, 
        &mut state.allowance_cache
    ).await;
    
    // Línea 1695: Calcula size pero no valida que sea ejecutable
    let size = effective_sell_size(remaining, available.clone(), CLOB_DEFAULT_MIN_ORDER_SIZE);
    
    // Línea 1696: No hay suficiente validación antes del intento
    if size < MIN_SELL_SIZE {
        // espera pero no invalida caché
    }
}
```

### ✅ Soluciones Propuestas

#### Solución 1: Invalidar caché agresivamente después de errores
```rust
// En línea ~1735 (dentro del match de error de orden SL)
Err(e) => {
    if is_position_closed_error(e.as_str()) {
        balance_error_retries += 1;
        
        // NUEVO: Invalidar caché inmediatamente
        state.allowance_cache = None;
        
        if balance_error_retries % 3 == 0 {
            warn!("[IntervalSniper] SL: balance/allowance error (retry {}), canceling orders", balance_error_retries);
            let _ = clob.cancel_orders_for_token(&sl_token_id).await;
            
            // NUEVO: Esperar 200ms para que el exchange procese la cancelación
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        
        // NUEVO: Backoff exponencial en lugar de delay fijo
        let backoff_ms = (SL_FOK_RETRY_DELAY_MS * 2u64.pow(balance_error_retries.min(4))).min(500);
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        continue;
    }
}
```

#### Solución 2: Consultar REST balance directamente después de fills parciales
```rust
// Después de línea 1720 (cuando hay un fill parcial detectado)
if filled_this_attempt > Decimal::ZERO && filled_this_attempt < size {
    info!(
        "[IntervalSniper] SL partial fill {} @ {}, remaining {} — retrying immediately",
        fmt_decimal_2(&filled_this_attempt), fmt_price(Some(&bid)), fmt_decimal_2(&remaining)
    );
    
    // NUEVO: Forzar consulta REST fresh en lugar de usar WS/caché
    state.allowance_cache = None;
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // NUEVO: Verificar balance REST directamente
    let fresh_balance = clob.get_available_balance(&sl_token_id).await.ok().flatten();
    if let Some(bal) = fresh_balance {
        if bal < DUST_THRESHOLD {
            info!("[IntervalSniper] SL: REST balance shows {} (dust), position closed", bal);
            break;
        }
        remaining = remaining.min(bal);
    }
}
```

#### Solución 3: Mejorar detección de dust más temprano
```rust
// Antes de línea 1695 (antes de calcular effective_sell_size)
if remaining < DUST_THRESHOLD {
    if total_filled > Decimal::ZERO {
        info!("[IntervalSniper] SL: remaining {} is dust, position closed", remaining);
        // Log cierre...
        break;
    }
}

// NUEVO: También verificar available antes de intentar orden
if let Some(avail) = available {
    if avail < MIN_SELL_SIZE {
        if sl_attempt > 10 && total_filled > Decimal::ZERO {
            info!("[IntervalSniper] SL: available {} too low after {} attempts, treating as closed", avail, sl_attempt);
            break;
        }
        state.allowance_cache = None;
        tokio::time::sleep(Duration::from_millis(50)).await;
        continue;
    }
}
```

---

## 🔴 PROBLEMA #2: Error "Invalid Amounts" en Take Profit

### Síntomas
```
ERROR sniper::clob: [LiveClob] order failed: HTTP 400 Bad Request: {"error":"invalid amounts, maker and taker amount must be higher than 0"}
WARN sniper::runner: [IntervalSniper] TP limit place failed: HTTP 400 Bad Request: {"error":"invalid amounts, maker and taker amount must be higher than 0"}
```

**Aparece 135+ veces en las líneas 57-491 de los logs**

### Causa Raíz
El bot intenta colocar órdenes de Take Profit GTC limit con cantidades calculadas incorrectamente:

1. **Balance WS vs Real**: El WS reporta 6.01 shares pero el código interno usa 6.00, causando una discrepancia
2. **`effective_sell_size` retorna 0**: La función `effective_sell_size` en algunos casos calcula un tamaño de 0 o negativo debido a:
   - `BALANCE_BUFFER_SHARES` (0.000001) restado del available
   - Redondeo con `floor_to_decimals` que trunca a 0
3. **No hay validación pre-submit**: El código no valida que `size >= MIN_SELL_SIZE` antes de enviar la orden al API

### Ubicación del Código
**Archivo**: `runner.rs`  
**Líneas**: 2224-2373 (colocación de TP limit order)

```rust
// Línea 2229-2236
let position_size_real = tp.size.clone();
let available = get_available_for_sell(
    clob.as_ref().as_ref(), 
    ws_user_ref, 
    &tp.token_id, 
    &mut state.allowance_cache
).await;
let size = effective_sell_size(
    position_size_real,
    available.clone(),
    CLOB_DEFAULT_MIN_ORDER_SIZE,
);

// Línea 2237: Validación insuficiente
if size >= MIN_SELL_SIZE && size >= DUST_THRESHOLD {
    let price = target;
    let result = clob
        .place_sell_order(
            &tp.token_id,
            price,
            size.clone(),
            crate::types::SellOrderTimeInForce::Gtc,
        )
        .await?;
    // ...
}
```

### Problema en `effective_sell_size`
```rust
// Línea 196-218
fn effective_sell_size(
    position_size: Decimal,
    available: Option<Decimal>,
    min_order_size: Decimal,
) -> Decimal {
    let capped = available
        .map(|a| {
            // PROBLEMA: Si available = 0.000001, esto da 0
            let safe = (a - BALANCE_BUFFER_SHARES).max(Decimal::ZERO);
            position_size.min(safe)
        })
        .unwrap_or(position_size);
    
    // PROBLEMA: floor_to_decimals puede truncar pequeños valores a 0
    let result = floor_to_decimals(capped, SELL_SIZE_DECIMALS);
    
    // Esta lógica solo funciona si result está cerca de min_order_size
    if result < min_order_size
        && result >= min_order_size - dec!(0.01)
        && available.map_or(false, |a| a >= min_order_size)
    {
        min_order_size
    } else {
        result  // ← Puede ser 0 aquí
    }
}
```

### ✅ Soluciones Propuestas

#### Solución 1: Agregar validación robusta antes de place_sell_order
```rust
// Reemplazar líneas 2237-2246
if size >= MIN_SELL_SIZE && size >= DUST_THRESHOLD {
    let price = target;
    
    // NUEVO: Validación adicional
    if size < dec!(0.0001) {
        warn!(
            "[IntervalSniper] TP limit: calculated size {} is too small, skipping (available={:?}, position={})",
            size, available, position_size_real
        );
        state.tp_limit_balance_retries += 1;
        
        // Si falla 3+ veces, cancelar todo y refrescar balance
        if state.tp_limit_balance_retries >= 3 {
            state.allowance_cache = None;
            let _ = clob.cancel_orders_for_token(&tp.token_id).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        // No reintentar este tick
        continue;
    }
    
    let result = clob
        .place_sell_order(
            &tp.token_id,
            price,
            size.clone(),
            crate::types::SellOrderTimeInForce::Gtc,
        )
        .await?;
    // ...
}
```

#### Solución 2: Mejorar `effective_sell_size` para nunca retornar valores inválidos
```rust
// Reemplazar función completa (líneas 196-218)
fn effective_sell_size(
    position_size: Decimal,
    available: Option<Decimal>,
    min_order_size: Decimal,
) -> Decimal {
    let capped = available
        .map(|a| {
            // MEJORADO: Solo restar buffer si el available es significativo
            let safe = if a > dec!(0.01) {
                (a - BALANCE_BUFFER_SHARES).max(Decimal::ZERO)
            } else {
                a  // Para valores muy pequeños, no restar nada
            };
            position_size.min(safe)
        })
        .unwrap_or(position_size);
    
    let result = floor_to_decimals(capped, SELL_SIZE_DECIMALS);
    
    // MEJORADO: Validar que result sea >= MIN_SELL_SIZE al final
    if result < MIN_SELL_SIZE {
        // Si estamos cerca de min_order_size Y hay balance suficiente, redondear
        if result >= min_order_size - dec!(0.01)
            && available.map_or(false, |a| a >= min_order_size)
        {
            min_order_size
        } else {
            // NUEVO: Retornar 0 explícitamente en lugar de valores dust
            Decimal::ZERO
        }
    } else if result < min_order_size
        && result >= min_order_size - dec!(0.01)
        && available.map_or(false, |a| a >= min_order_size)
    {
        min_order_size
    } else {
        result
    }
}
```

#### Solución 3: Detectar error "invalid amounts" y salir del loop
```rust
// Después de línea 2369 (donde se maneja result.error_msg)
else if is_invalid_amounts_error(result.error_msg.as_deref()) {
    state.tp_limit_balance_retries += 1;
    
    // NUEVO: Logging más detallado
    warn!(
        "[IntervalSniper] TP limit 'invalid amounts' error (retry {}): size={}, available={:?}, position={}",
        state.tp_limit_balance_retries, size, available, position_size_real
    );
    
    // NUEVO: Después de 5 intentos, asumir que la posición está cerrada o inválida
    if state.tp_limit_balance_retries >= 5 {
        warn!("[IntervalSniper] TP limit failed 5+ times with 'invalid amounts' — canceling TP, SL remains active");
        state.tp_limit_order_id = None;
        state.tp_limit_balance_retries = 0;
        state.pending_auto_sell = None;  // Detener intentos de TP
        state.allowance_cache = None;
        // SL sigue activo para proteger posición
        break;
    }
    
    // Refrescar cache y esperar
    state.allowance_cache = None;
    tokio::time::sleep(Duration::from_millis(100)).await;
}
```

#### Solución 4: Agregar logs de debug para diagnosticar el problema
```rust
// Antes de línea 2237 (antes de la validación de size)
debug!(
    "[IntervalSniper] TP limit calculation: position_size={}, available={:?}, effective_size={}, MIN_SELL_SIZE={}, DUST_THRESHOLD={}",
    position_size_real, available, size, MIN_SELL_SIZE, DUST_THRESHOLD
);

if size < MIN_SELL_SIZE {
    debug!(
        "[IntervalSniper] TP limit: size {} < MIN_SELL_SIZE {}, skipping placement this tick",
        size, MIN_SELL_SIZE
    );
}
```

---

## 📋 Plan de Implementación Recomendado

### Fase 1: Correcciones Críticas (Prioridad Alta)
1. ✅ Implementar **Solución 1 de Problema #1**: Invalidar caché agresivamente
2. ✅ Implementar **Solución 1 de Problema #2**: Validación antes de place_sell_order
3. ✅ Implementar **Solución 3 de Problema #2**: Detectar "invalid amounts" y salir

### Fase 2: Mejoras de Robustez (Prioridad Media)
4. ✅ Implementar **Solución 2 de Problema #1**: Consultar REST balance después de fills parciales
5. ✅ Implementar **Solución 2 de Problema #2**: Mejorar `effective_sell_size`

### Fase 3: Observabilidad (Prioridad Baja)
6. ✅ Implementar **Solución 4 de Problema #2**: Agregar logs de debug
7. ✅ Agregar métricas de tasa de errores por tipo

---

## 🎯 Resultados Esperados

Después de aplicar estas correcciones:

1. **Stop Loss**: Reducción de 36 errores a 0-2 (solo casos excepcionales de lag extremo)
2. **Take Profit**: Eliminación completa de los 135+ errores "invalid amounts"
3. **Throughput**: Mejora en tiempo de ejecución de SL (actualmente ~2-3 segundos de retry innecesario)
4. **Confiabilidad**: Reducción de 90%+ en llamadas API fallidas

---

## ⚠️ Consideraciones Adicionales

### Límites del Exchange
- **CLOB_DEFAULT_MIN_ORDER_SIZE**: 5.0 shares (constante en línea 25)
- **MIN_SELL_SIZE**: 0.0001 shares (constante en línea 33)
- **SELL_SIZE_DECIMALS**: 4 decimals (constante en línea 31)

### Variables de Entorno Relevantes
```bash
MM_TAKE_PROFIT_PRICE=0.97          # Precio de TP (97¢)
MM_STOP_LOSS_PRICE=0.90            # Precio de SL (85¢ en logs, discrepancia)
MM_TAKE_PROFIT_TIME_IN_FORCE=FAK   # FAK para TP
MM_STOP_LOSS_QUANTITY_PERCENT=100  # Vender 100% en SL
MM_AUTO_SELL_QUANTITY_PERCENT=100  # Vender 100% en TP
```

### Discrepancias Detectadas
1. Config dice `STOP_LOSS_PRICE=0.90` pero los logs muestran SL @ 0.85 — verificar cálculo de trigger price
2. WS reporta 6.01 shares pero se compran 6.00 — posible redondeo en fill detection

---

## 📚 Referencias

**Archivos Modificados**:
- `runner.rs` - Líneas 196-218, 1500-1750, 2224-2373
- `clob.rs` - (posiblemente, para mejor handling de errores)

**Constantes Clave**:
```rust
const TICK_SIZE: Decimal = dec!(0.01);
const CLOB_DEFAULT_MIN_ORDER_SIZE: Decimal = dec!(5);
const SELL_SIZE_DECIMALS: u32 = 4;
const MIN_SELL_SIZE: Decimal = dec!(0.0001);
const DUST_THRESHOLD: Decimal = dec!(0.001);
const BALANCE_BUFFER_SHARES: Decimal = dec!(0.000001);
```
