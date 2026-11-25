# Changelog

Formato basado en [Keep a Changelog](https://keepachangelog.com/es-ES/1.1.0/).

## [Sin publicar]

### Añadido

- Prototipo v0.1 (`f307532`): quita el marco y ajusta la ventana a la resolución del monitor (`rcMonitor`) de los procesos que indique el perfil, y restaura el estado original exacto al salir o al pulsar «Restaurar todo».
- Detección de ventanas nuevas mediante hooks `EVENT_OBJECT_SHOW` y `EVENT_SYSTEM_FOREGROUND` (fuera de contexto) y ventana de solo mensajes para recibirlos.
- Retraso configurable por perfil (`delay_ms`) con `SetTimer`: la ventana se posiciona de inmediato y el ajuste de estilo se aplica después.
- Perfiles JSON (`version: 1`) con `enabled`, `monitor`, `offsets`, `delay_ms` y `clip_cursor`; un archivo inválido se informa y nunca se sobrescribe.
- Lista de exclusiones de procesos del sistema y de la propia aplicación.
- Instancia única y manifiesto con `asInvoker` y DPI `PerMonitorV2`.
- Icono de bandeja con «Aplicar», «Restaurar todo», «Recargar» y «Salir».
- Licencia MIT.
- Herramienta de prueba `examples/testwin` (`27f1555`) con seis modos que reproducen comportamientos observados en juegos: `normal`, `fixed`, `revert-style`, `force-size`, `warscape` y `late-frame`.

### Corregido

Correcciones aplicadas antes del primer commit (incluidas en `f307532`):

- Una ventana maximizada que se minimiza se captura en estado restaurado, para guardar el rectángulo original correcto.
- `restore()` no se detiene al primer error: intenta el estilo, el rectángulo y la maximización, y devuelve el primer error.
- El destino del retraso se calcula una sola vez en la captura; el paso diferido no vuelve a leer el perfil.
- Una ventana restaurada desde la bandeja no se vuelve a aplicar en el siguiente foco.
- «Aplicar» desde la bandeja respeta las exclusiones y el formato de nombre (ruta o nombre de archivo).
