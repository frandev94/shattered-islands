# Compatibilidad de juegos

Registro de qué juegos y aplicaciones se han probado con Borderless Shatter y cómo se comportó cada uno.

## Cómo leer esta página

Estados:

- ✅ funciona
- ⚠️ funciona con ajustes
- ❌ se defiende (el juego deshace el cambio)
- ❓ sin probar

Qué se mide en cada prueba:

- **style**: estilo de la ventana (`GWL_STYLE`).
- **ex-style**: estilo extendido (`GWL_EXSTYLE`).
- **rect**: rectángulo de la ventana.
- **restauración**: si, al cerrar Borderless Shatter (`WM_QUIT`), los tres valores anteriores vuelven exactamente al original.

## Resumen

| Juego / app | Motor | Versión / commit | Fecha | Estado | Nota |
|---|---|---|---|---|---|
| Bloc de notas | Win32 nativo | prototipo v0.1 (`1b5c142`) | 2026-11-25 | ✅ | Aplica y restaura exacto, 5/5 con retraso |
| Total War: Warhammer III | Warscape | prototipo v0.1 (`1b5c142`) | 2026-11-25 | ❌ | El juego revierte estilo y tamaño; ver causa en `preferences.script.txt` |
| Phoenix Wright: Ace Attorney Trilogy | — | — | — | ❓ | Pendiente |

## Bloc de notas

Monitor único de 4096x1152.

| Valor | Antes | Después |
|---|---|---|
| style | `0x14CF0000` | `0x14000000` |
| ex-style | `0x110` | `0x10` |
| rect | — | `[0,0,4096,1152]` |

- `WM_QUIT` → restauración exacta.
- Con retraso de 1500 ms: 5/5 exactas.

## Total War: Warhammer III

Proceso `Warhammer3.exe`, en modo ventana, sin elevar.

**Estado original:** style `0x14CA0000` (sin `THICKFRAME`), ex-style `0x100`.

**Cronología medida:**

- Nuestra escritura de estilo: el juego la revierte unos 8 ms después.
- Nuestro `SetWindowPos` a `rcMonitor`: el juego lo revierte unos 2 ms después a `[0,0,1936,1119]` (cliente 1920x1080 más marco), conservando la posición 0,0.
- Con retraso: mismo comportamiento.
- Restauración: exacta.

**Causa:** el fichero `%APPDATA%\The Creative Assembly\Warhammer3\scripts\preferences.script.txt` contiene:

- `fix_res true` («Forbid window resizing»)
- `fix_window_pos true` («Forbid window repositioning»)
- `x_res 1920`, `y_res 1080`

Es coherente con el tamaño inmutable observado. Hay precedente en el mismo motor: en Shogun 2, poner `fix_res false; fix_window_pos false; gfx_fullscreen false;` hizo que Borderless Gaming funcionase (issue #197, 2015).

**Qué se podría probar (sin probar):** con el juego cerrado, `fix_res false`, `fix_window_pos false`, `x_res 2048`, `y_res 1152`, para una isla 16:9.

**Reproducción:** `testwin --mode warscape` imita este comportamiento (ver abajo).

## Phoenix Wright: Ace Attorney Trilogy

Pendiente de probar. Es el siguiente juego real previsto. Registrar los datos según la sección «Cómo añadir un juego».

## Juego de prueba `testwin`

`examples/testwin.rs` es una ventana falsa que imita comportamientos de juegos. Uso: `testwin --mode normal|fixed|revert-style|force-size|warscape|late-frame [--client WxH] [--pos X,Y] [--delay MS] [--title TEXT] [--log PATH]`.

Resultado de la matriz (12 escenarios: 6 modos, inmediato y con `delay_ms:1500`):

| Modo | Qué imita | Inmediato | Con retraso 1500 ms |
|---|---|---|---|
| `normal` | Juego que acepta el cambio | borderless + rcMonitor (46–82 ms) | igual |
| `fixed` | Ventana de tamaño fijo que acepta el cambio | borderless + rcMonitor (46–82 ms) | igual |
| `revert-style` | Juego que repone su estilo | marco de vuelta en ~1 ms, tamaño conservado | igual |
| `force-size` | Juego que fuerza su tamaño | estilos quitados, tamaño forzado a 1280x720 en 0,0 | igual |
| `warscape` | TW:WH3 (revierte estilo y tamaño) | estilo revertido, tamaño forzado, posición 0,0 conservada | igual |
| `late-frame` | Juego que repone el marco tarde | borderless; marco de vuelta ~1500 ms después | igual |

La restauración fue exacta en los 12 escenarios.

## Cómo añadir un juego

Registrar, para cada juego:

- **Proceso**: nombre del ejecutable y si está elevado.
- **Motor**.
- **Modo de ventana del juego**: ventana, borderless nativo o pantalla completa.
- **Ficheros de configuración relevantes**: ruta y opciones que afecten a tamaño, posición o modo.
- **Valores antes/después**: style, ex-style y rect, inmediato y con retraso.
- **Si el juego revierte**: qué valor y cuántos ms después.
- **Restauración**: si es exacta tras `WM_QUIT`.
- Versión o commit de Borderless Shatter, fecha y estado (✅/⚠️/❌/❓); añadir la fila al resumen.
