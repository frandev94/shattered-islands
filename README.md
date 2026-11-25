# Borderless Shatter

Utilidad de Windows que quita el marco a las ventanas de juegos y apps y las ajusta al monitor, con restauración exacta.

## Qué hace

Versión 0.1:

- Quita el marco (título, bordes, botones) a las ventanas de los procesos que tengan perfil y las ajusta al rectángulo del monitor.
- Restaura la ventana exactamente como estaba (estilo, posición, tamaño, estado maximizado) cuando se pide y al salir.
- Detecta ventanas nuevas por eventos de Windows, sin sondeo.
- Vive en la bandeja del sistema.
- Instancia única: una segunda ejecución no crea otro icono.
- No necesita permisos de administrador.

## Estado

Prototipo v0.1.

Probado (2026-11-25, un solo monitor de 4096x1152):

- Notepad: aplica y restaura de forma exacta; con retardo de 1500 ms, 5 de 5 exactas.
- Ventana de prueba `testwin`, 12 escenarios (inmediato y con retardo de 1500 ms): restauración exacta en todos.
- Total War: WARHAMMER III: el juego revierte los cambios (ver [Limitaciones conocidas](#limitaciones-conocidas)); la restauración es exacta.

Sin probar: varios monitores, ventanas de procesos elevados, Phoenix Wright: Ace Attorney Trilogy.

Detalle por juego en [`docs/GAMES.md`](docs/GAMES.md).

## Requisitos y compilación

- Windows 10/11 x64.
- Rust estable con el toolchain MSVC.

```
cargo build --release
```

El ejecutable queda en `target\release\borderless-shatter.exe`.

## Uso

1. Ejecuta `borderless-shatter.exe`. Aparece el icono en la bandeja.
2. Crea o edita [`profiles.json`](#perfiles) y elige «Recargar perfiles».
3. Al abrirse una ventana de un proceso con perfil, se le quita el marco y se ajusta al monitor.

Menú de la bandeja:

| Acción | Efecto |
|---|---|
| `Aplicar: <proceso>` (una por perfil) | Aplica el perfil a las ventanas de ese proceso ahora |
| `Restaurar todas` | Devuelve todas las ventanas a su estado original |
| `Recargar perfiles` | Vuelve a leer `profiles.json` y reconstruye el menú |
| `Salir` | Restaura las ventanas y cierra el programa |

Una ventana restaurada desde la bandeja no se vuelve a tratar automáticamente.

## Perfiles

Fichero: `%APPDATA%\BorderlessShatter\profiles.json`. Si no existe, se crea vacío al arrancar. La versión del formato es `1`; una versión mayor se rechaza.

```json
{
  "version": 1,
  "profiles": [
    {
      "process": "Warhammer3.exe",
      "enabled": true,
      "monitor": "current",
      "offsets": { "left": 0, "top": 0, "right": 0, "bottom": 0 },
      "delay_ms": 0,
      "clip_cursor": false
    }
  ]
}
```

| Campo | Tipo | Por defecto | Descripción |
|---|---|---|---|
| `version` | número | obligatorio | Versión del formato (`1`) |
| `profiles` | lista | vacía | Perfiles |
| `process` | texto | obligatorio | Nombre del ejecutable |
| `enabled` | booleano | `true` | Si es `false`, el perfil no se aplica |
| `monitor` | `"current"` o número | `"current"` | Monitor actual de la ventana, o índice del monitor (entero ≥ 0) |
| `offsets` | objeto | todo `0` | `left` y `top` desplazan el origen; `right` y `bottom` ajustan el ancho y el alto: `ancho = monitor − left + right`, `alto = monitor − top + bottom`. Cada clave omitida vale `0` |
| `delay_ms` | número | `0` | Espera en milisegundos antes de aplicar |
| `clip_cursor` | booleano | `false` | Aceptado e ignorado en v0.1 |

Procesos excluidos siempre (nunca se tocan): `explorer.exe`, `dwm.exe`, `shellexperiencehost.exe`, `startmenuexperiencehost.exe`, `searchhost.exe`, `textinputhost.exe`, `applicationframehost.exe`, `systemsettings.exe`, `lockapp.exe`, `taskmgr.exe`, `borderless-shatter.exe`.

## Juego de prueba

`testwin` es una ventana que imita comportamientos de juegos reales:

```
cargo run --example testwin -- --mode warscape
```

Opciones: `--mode`, `--client WxH` (por defecto 1280x720), `--pos X,Y` (por defecto 100,100), `--delay MS` (por defecto 1500), `--title TEXT`, `--log PATH`.

| Modo | Comportamiento |
|---|---|
| `normal` | Ventana normal con marco; acepta los cambios |
| `fixed` | Ventana de tamaño fijo; acepta los cambios |
| `revert-style` | Restaura su marco casi al instante; conserva el tamaño |
| `force-size` | Acepta quitar el marco pero fuerza su tamaño original en 0,0 |
| `warscape` | Combina `revert-style` y `force-size`: reproduce Total War: WARHAMMER III |
| `late-frame` | Acepta los cambios y recupera el marco ~1500 ms después |

## Limitaciones conocidas

- Juegos que se defienden: Total War: WARHAMMER III revierte el estilo (~8 ms) y fuerza el tamaño (~2 ms). Su causa está en `preferences.script.txt` (`fix_res true`, `fix_window_pos true`). Cambiar esos ajustes para que funcione está sin probar.
- Ventanas de procesos elevados: sin probar; sin permisos de administrador, Windows puede impedir modificarlas.
- Un cierre abrupto del programa (fin de proceso, corte de luz) deja las ventanas ya modificadas sin marco; solo la restauración normal las devuelve.
- Solo se ha probado con un monitor.

## Hoja de ruta

Resumen de [`docs/VISION.md`](docs/VISION.md): el objetivo son «islas», rectángulos con nombre sobre el monitor, para dividir una pantalla 32:9 en dos de 16:9 y colocar un juego en cada una.

- **v0.1** (hecho): borderless a monitor completo, restauración exacta y bandeja.
- **v0.2**: islas fijas en JSON (primer preset: dos mitades 16:9), cada perfil indica su isla, reaplicado con tope de reintentos.
- **v0.3**: mover la ventana activa a una isla desde la bandeja, con atajo de teclado.
- **Después**: editor visual de islas, contenedor con `SetParent` solo si hace falta, ajustes por juego.

## Documentación

- [`docs/VISION.md`](docs/VISION.md): visión e islas.
- [`docs/PROTOTYPE.md`](docs/PROTOTYPE.md): especificación de la v0.1.
- [`docs/DECISIONS.md`](docs/DECISIONS.md): decisiones de diseño.
- [`docs/GAMES.md`](docs/GAMES.md): resultados por juego.
- [`CHANGELOG.md`](CHANGELOG.md): historial de cambios.

## Licencia

MIT. Implementación propia: no se copia código de otros repositorios. [Borderless Gaming](https://github.com/andrewmd5/Borderless-Gaming) (GPL-2.0) e [ihateborders](https://github.com/Z1xus/ihateborders) (GPL-3.0) se usaron solo como referencia de comportamiento.
