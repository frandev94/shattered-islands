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

> **Nota sobre píxeles.** El monitor es único, de **5120x1440 físicos**, a 125 % (120 DPI). Las primeras pruebas se hicieron con scripts no DPI-aware y Windows virtualizó las lecturas: 4096x1152 era la vista virtualizada (÷1,25). Los valores marcados **(virt.)** vienen de ese espacio virtualizado; para el valor físico, multiplicar por 1,25. Los resultados relativos («rect == `rcMonitor`», estilos) siguen siendo válidos. Borderless Shatter es PerMonitorV2 y trabaja en píxeles físicos.

## Resumen

| Juego / app | Motor | Versión / commit | Fecha | Estado | Nota |
|---|---|---|---|---|---|
| Bloc de notas | Win32 nativo | prototipo v0.1 (`1b5c142`) | 2026-11-25 | ✅ | Aplica y restaura exacto, 5/5 con retraso |
| Total War: Warhammer III | Warscape | prototipo v0.1 (`1b5c142`) | 2026-11-25 | ❌ | El juego revierte estilo y tamaño; ver causa en `preferences.script.txt` |
| Phoenix Wright: Ace Attorney Trilogy | Unity | `8fb1b51` | 2026-11-25 | ⚠️ | Quita el marco; Unity fuerza 1280x720 y reposiciona |

## Bloc de notas

Monitor único de 4096x1152 (virt.) = 5120x1440 físicos. Rect (virt.) `[0,0,4096,1152]` = `rcMonitor`; el resultado relativo (rect == `rcMonitor`) sigue siendo válido.

| Valor | Antes | Después |
|---|---|---|
| style | `0x14CF0000` | `0x14000000` |
| ex-style | `0x110` | `0x10` |
| rect | — | `[0,0,4096,1152]` (virt.) |

- `WM_QUIT` → restauración exacta.
- Con retraso de 1500 ms: 5/5 exactas.

## Total War: Warhammer III

Proceso `Warhammer3.exe`, en modo ventana, sin elevar. **DPI:** unaware (awareness 0, `GetDpiForWindow` 96); Windows lo virtualiza y escala el bitmap. Ventana física `[1350,6,3770,1405]`, cliente 2400x1350 físico = `x_res 1920` × 1,25 e `y_res 1080` × 1,25. Los rects de la cronología (p. ej. `[0,0,1936,1119]`) son (virt.).

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

**Qué se podría probar (sin probar):** con el juego cerrado, `fix_res false`, `fix_window_pos false`, y, para una mitad física de 2560x1440, `x_res 2048`, `y_res 1152` (píxeles virtuales, porque el juego es DPI-unaware: 2048 × 1,25 = 2560).

**Reproducción:** `testwin --preset warscape` imita este comportamiento (ver abajo).

## Phoenix Wright: Ace Attorney Trilogy

Proceso `PWAAT.exe` (Steam, `<Steam library>\steamapps\common\Phoenix Wright Ace Attorney Trilogy\`), motor Unity (`UnityPlayer.dll`, `PWAAT_Data`, clase de ventana `UnityWndClass`), 32 bits, en modo ventana, sin elevar y lanzado sin argumentos de Unity. Monitor único de 5120x1440 físicos. **DPI:** per-monitor aware (awareness 2, DPI 120): físico = lógico; cliente en ventana 1280x720 físico. Las coordenadas de la cronología siguiente (x=2126, 1732, 1852…) son (virt.) del script anterior; ×1,25 en físico. 

**Configuración:** registro `HKCU\Software\CAPCOM\Phoenix Wright: Ace Attorney Trilogy / …` (solo lectura):

- `Screenmanager Resolution Width`: 1600
- `Screenmanager Resolution Height`: 900
- `Screenmanager Is Fullscreen mode`: 0

No coincide con el cliente real, que es 1280x720. El origen del 1280x720 es desconocido.

**Estado original:** style `0x14CA0000`, ex-style `0x100`, rect `[231,223,1516,971]`, cliente 1280x720.

**Cronología medida, inmediata:**

| Momento | Evento | Valores |
|---|---|---|
| +48 ms | Se quita el estilo y no se repone | style `0x14000000`, ex-style `0` |
| +69 ms | Nuestro `SetWindowPos` a `rcMonitor` | rect `rcMonitor` |
| +130 ms | Unity redimensiona a su cliente y reposiciona | `[2126,0,3406,720]` |
| +150 ms | Nuestra app vuelve a afirmar la ventana | — |
| +186 ms | Unity revierte el tamaño otra vez | final 1280x720 en x=2126 |

**Cronología medida, con retraso de 3000 ms:**

| Momento | Evento | Valores |
|---|---|---|
| ~+48 ms | Unity ya había devuelto la ventana a su tamaño | 1280x720 en x=1732 |
| ~3053 ms | Se quita el estilo y no se repone | style `0x14000000`, ex-style `0` |
| ~3091 ms | Nuestro `SetWindowPos` a `rcMonitor` | rect `rcMonitor` |
| ~3153 ms (≈ +62 ms) | Unity revierte el tamaño | final 1280x720 en x=1852 |

La posición final cambia de una prueba a otra (2126 en la inmediata, 1852 en la retardada). Su origen es desconocido.

- `WM_QUIT` → restauración exacta, en ambas pruebas.

**Comparación:**

- Frente a TW:WH3 (`warscape`): aquí Unity no repone el estilo, así que el marco queda quitado. Ambos juegos revierten el tamaño.
- Frente a `testwin --mode force-size`: mismo comportamiento (estilo aceptado, tamaño forzado), más el reposicionamiento.

**Estado ⚠️:** el marco se mantiene quitado, pero el tamaño no. La leyenda no tiene estado parcial; se usa ⚠️ porque la ventana sí queda sin marco.

**Ideas sin probar:**

- Con `-popupwindow -screen-width 2560 -screen-height 1440` (los argumentos de tamaño no se respetaron en la prueba con 2048x1152).
- Fijar en PlayerPrefs la resolución al tamaño de la isla antes de lanzar.

### Con `-popupwindow`

Lanzado con `-popupwindow -screen-width 2048 -screen-height 1152`:

- style `0x94000000` (`WS_POPUP`, sin bits de marco), ex-style `0`, cliente 1280x720 físico: los argumentos de tamaño se ignoraron.
- Registro: `Screenmanager Resolution Width_h182942802` / `Height_h2627697771` = 1280 / 720 (antes, 1600 / 900 leídos sin los nombres `_h`).
- `SetWindowPos` manual a `[2560,0,5120,1440]`: Unity lo revierte en menos de 20 ms a 1280x720 en `[3677,0,4957,720]`. A `[0,0,2560,1440]`: `[2397,0,3677,720]`.
- **Limitación de la app:** Borderless Shatter no toca esta ventana, porque `has_targetable_style` exige bits de marco y `capture()` devuelve `Ok(false)`. Ver D-14.

## Juego de prueba `testwin`

`examples/testwin.rs` es una ventana falsa que imita comportamientos de juegos. Uso: `testwin [--preset warscape|unity|unity-popup|notepad] [--style frame|fixed|popup] [--revert-style MS] [--force-size MS] [--reposition keep|center-on-request|offset:X,Y|game-unity] [--late-frame MS] [--dpi aware|unaware] [--client WxH] [--pos X,Y] [--delay MS] [--title TEXT] [--log PATH]`. Los nombres de `--mode` antiguos siguen funcionando.

Fidelidad de los presets (píxeles físicos, monitor 5120x1440 al 125 %, validación del 2026-11-25):

| Preset | Juego real | Qué coincide | Diferencias |
|---|---|---|---|
| `warscape` | TW:WH3 | Repone el estilo y fuerza el tamaño (ventana 2420x1399, cliente 2400x1350). Mantiene la posición 0,0 y revierte la reafirmación de la app. | Solo tiempos: repone el estilo en 15–40 ms frente a ~8 ms real, y fuerza el tamaño en ~7 ms frente a ~2 ms. `SetTimer` tiene un mínimo de 10 ms y un tick de ~15,6 ms. |
| `unity` | Phoenix Wright (Unity, ventana) | El estilo quitado se mantiene. El tamaño se fuerza a 1280x720 unos 62 ms tras nuestro `SetWindowPos` (real: ~60 ms). | Reposicionamiento: el juego real movió la ventana (x=2126 / 1852); testwin mantiene 0,0. La regla de reposición no se reproduce. |
| `unity-popup` | Phoenix Wright con `-popupwindow` | Estilo `0x94000000` sin tocar, como el real. Un `SetWindowPos` manual a `[2560,0,5120,1440]` vuelve a 1280x720 en 21 ms (real: < 20 ms). | Posición tras revertir: testwin `[2560,0,3840,720]`; real `[3677,0,4957,720]`. |
| `notepad` | Bloc de notas (aplicación cooperativa) | Acepta borderless 5120x1440 sin reaccionar. | Ninguna medida. |

La restauración fue exacta en todas las ejecuciones de la app.

La matriz anterior de 6 modos (12 escenarios) se hizo con la versión previa, en píxeles virtualizados; sus cifras no valen para esta tabla.

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
