# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.0] - 2026-11-26

### 🚀 Features

- *(engine)* Add islands, guard, state store and settings
- *(ipc)* Add named-pipe JSON-lines protocol
- *(i18n)* Add English and Spanish interface strings
- *(gui)* Add GPUI panel
- *(mcp)* Add stdio MCP server
- *(cli)* Add command-line client

### 📚 Documentation

- Add architecture, contributing and design guides

### 🧪 Testing

- Add property, end-to-end, MCP and CLI suites

### ⚙️ Miscellaneous Tasks

- Release on merge with git-cliff

### 💼 Other

- Pin toolchain and add lint, deny and nextest config
## [0.1.0] - 2026-11-25

### Added

- Prototype v0.1 (`f07d5c9`): removes the frame and fits the window to the monitor resolution (`rcMonitor`) of the processes listed in the profile, and restores the exact original state on exit or when pressing "Restaurar todo".
- Detection of new windows through `EVENT_OBJECT_SHOW` and `EVENT_SYSTEM_FOREGROUND` hooks (out of context) and a message-only window to receive them.
- Configurable delay per profile (`delay_ms`) with `SetTimer`: the window is positioned immediately and the style change is applied afterwards.
- JSON profiles (`version: 1`) with `enabled`, `monitor`, `offsets`, `delay_ms` and `clip_cursor`; an invalid file is reported and never overwritten.
- Exclusion list of system processes and of the application itself.
- Single instance and manifest with `asInvoker` and DPI `PerMonitorV2`.
- Tray icon with "Aplicar", "Restaurar todo", "Recargar" and "Salir".
- MIT license.
- Test tool `examples/testwin` (`07ca9a2`) with six modes that reproduce behaviors observed in games: `normal`, `fixed`, `revert-style`, `force-size`, `warscape` and `late-frame`.
- testwin: combinable behaviors (style, revert style, force size, reposition, late frame, DPI) and presets measured on real games: `warscape` (TW:WH3), `unity` and `unity-popup` (Phoenix Wright), `notepad`.

### Changed

- Documentation corrected to physical pixels (monitor 5120x1440 at 125%; islands of 2560x1440).

### Fixed

Fixes applied before the first commit (included in `f07d5c9`):

- A maximized window that is minimized is captured in its restored state, so the correct original rectangle is saved.
- `restore()` does not stop at the first error: it tries the style, the rectangle and the maximization, and returns the first error.
- The delay target is calculated once at capture; the deferred step does not read the profile again.
- A window restored from the tray is not applied again on the next focus.
- "Aplicar" from the tray respects the exclusions and the name format (path or file name).
