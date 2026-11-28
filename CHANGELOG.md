# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.8] - 2026-11-28

### 🚀 Features

- *(profiles)* Match windows by title or title regex
- *(gui)* Choose a window title rule for a profile in the panel

### 🧪 Testing

- *(e2e)* Cover title rules, late titles and restore

## [0.2.7] - 2026-11-27

### 🚀 Features

- *(app)* Start at sign-in from the tray and the panel

### 📚 Documentation

- Describe the headless GUI tests
- Describe start at sign-in

### 🧪 Testing

- *(gui)* Run the profiles view headless in cargo test

## [0.2.6] - 2026-11-27

### 🚀 Features

- *(gui)* Keep typed profile values when the profile changes outside

### 🐛 Bug Fixes

- *(gui)* Mark coupled profile fields and merge on every reload safely
- *(gui)* Translate the profiles notices when they are shown

### 📚 Documentation

- Note the e2e duration and the fast release profile
- Describe the profiles editor merge
- Note coupled fields in the profiles merge

### ⚙️ Miscellaneous Tasks

- *(e2e)* Build the release case without LTO and keep the cache after failures
- *(e2e)* Build the release examples as plain binaries

## [0.2.5] - 2026-11-27

### 🐛 Bug Fixes

- *(profiles)* Reject duplicate keys and arrays where objects are expected in profiles.json

### 📚 Documentation

- Describe strict loading of profiles.json

## [0.2.4] - 2026-11-27

### 🐛 Bug Fixes

- *(cli)* Reject duplicate keys in the profiles set body

### 📚 Documentation

- Add the clean-up rule for extra checkouts to AGENTS.md
- *(win32)* Point the unsafe-sites note at AGENTS.md
- Note that the CLI rejects duplicate keys in the profiles set body

### 🧪 Testing

- *(e2e)* Re-adopt state.json entries with legacy and unresolvable island refs

### ⚙️ Miscellaneous Tasks

- *(release)* Publish the sha256 in text format and leave a blank line between changelog sections
- *(test)* Run the test workflow on pull requests, on demand and from release only
- *(deny)* Drop licence allowances no dependency uses
- *(release)* Describe a missing release tag accurately in the gate error

## [0.2.3] - 2026-11-26

### 🐛 Bug Fixes

- *(ipc)* Reject duplicate keys and arrays where objects are expected
- *(ipc)* Validate params of parameterless methods and shorten duplicate-key errors

### 📚 Documentation

- Add dark and light header banners
- Describe what main does, add banner, process diagram and roadmap
- Thicken the banner's main crack
- Draw every reader of settings.json in the process diagram
- Add AGENTS.md and the manual release steps (#36)
- Document the e2e workflow and the local fallback
- Correct the e2e order and trigger paths
- Add the agent release rule to AGENTS.md
- Describe strict IPC decoding
- List the nullable params fields

### 🧪 Testing

- *(e2e)* Expect profiles.json.prev after the first save
- *(e2e)* Add a session probe for hosted runners
- *(e2e)* Force a client size that fits small runner screens

### ⚙️ Miscellaneous Tasks

- *(e2e)* Run the window suites on windows-latest
- *(e2e)* Run the probe contract tests and bound the suite step
## [0.2.2] - 2026-11-26

### 🐛 Bug Fixes

- *(profiles)* Keep unknown fields of layouts and islands

### 📚 Documentation

- Move design docs out of the repo
- Add IPC protocol and design constraints to the architecture guide
- Organize game results by engine and use case
- Describe kept unknown fields and replace_extras

### 🚜 Refactor

- Inline design reasons in comments

### ⚙️ Miscellaneous Tasks

- Release only when user-facing commits landed
## [0.2.1] - 2026-11-26

### 📚 Documentation

- Clean up design docs

### 🧪 Testing

- Use English strings in the notes filter test

### ⚙️ Miscellaneous Tasks

- Remove stray files
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
