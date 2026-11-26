# Repository Guidelines

## Project Overview

Borderless Shatter is a free alternative to **Borderless Gaming**: a Windows tool that forces game windows into borderless fullscreen. It takes ideas from the open-source edition of Borderless Gaming (C# WinForms, GPL-2.0, up to 9.6.0): https://github.com/andrewmd5/Borderless-Gaming

The resident is a tray process that owns the managed-window state; the GUI panel, the MCP server and the CLI are clients of its named pipe. Current features: core engine, islands, frameless windows, IPC, GUI with GPUI, MCP server, CLI and English/Spanish i18n; the IPC protocol and design constraints are in `ARCHITECTURE.md`. Roadmap: keyboard shortcuts to move the active window to an island. The implementation language is **Rust**: modern, small exe with no runtime.

Shattered Islands is a clean-room implementation: no code or text from GPL projects is copied, and behaviour is documented from public sources.

## Architecture & Data Flow

### Crate (src/, lib + bin)

Short map; module roles, the event loop, the IPC protocol, data files and invariants are in [`ARCHITECTURE.md`](ARCHITECTURE.md), the source of detail.

- Entry: `main.rs` (dispatch by subcommand: none = resident, `gui`, `mcp`, anything else = CLI) and `tray.rs` (the resident's loop).
- Core: `engine.rs` (owns managed-window state), `window_state.rs`, `islands.rs`, `guard.rs`, `diagnose.rs`, `watcher.rs`, `win32/`.
- Persistence: `profiles.rs` (profiles.json v2), `settings.rs` (settings.json: language), `state_store.rs` (state.json).
- Clients of the pipe: `ipc/` (protocol, server, client), `gui/` (GPUI panel), `mcp.rs` (stdio MCP server), `cli.rs` (subcommands).
- Text: `i18n.rs` and `locales/<lang>/*.json` (catalogs).
- Tests: `tests/` (`cli`, `mcp`, `i18n`, `properties`, `e2e`) and `examples/testwin.rs`.

## Key Directories

| Path | Purpose |
|---|---|
| `docs/` | `GAMES.md` (per-game results, organised by engine and use case) and the two banner SVGs (`banner-dark.svg`, `banner-light.svg`). |
| `ARCHITECTURE.md`, `CONTRIBUTING.md` | Code organisation; build, test and contribution rules. |
| `locales/` | i18n catalogs: `en/` (source of truth) and `es/`, one JSON file per area. |
| `tests/`, `examples/testwin.rs` | Integration suites and the fake game window they drive. |
| `src/` | The crate (Rust): library + `borderless-shatter` binary. |
| `target/` | Build output, ignored by git. |

## Development Commands

```bash
cargo build                                # debug: target/debug/borderless-shatter.exe (console visible)
cargo build --release                      # release: windows subsystem, LTO, stripped
cargo test                                 # lib unit tests + properties, i18n, cli, mcp tests; ignored suites: see Testing & QA
cargo clippy --all-targets -- -D warnings  # must be clean
cargo fmt --check
borderless-shatter.exe mcp                 # MCP server over stdio
borderless-shatter.exe list-windows        # CLI; see `borderless-shatter help` (resident must be running)
```

- Profiles: `%APPDATA%\BorderlessShatter\profiles.json` (created empty on first run). For manual tests, run the exe with `APPDATA` and `LOCALAPPDATA` pointing to temp folders, and `BORDERLESS_SHATTER_INSTANCE` set to a short suffix (1 to 32 characters of `A-Za-z0-9-`; an invalid value is refused, see Testing & QA), so the real profiles and state are untouched.
- Quit a running instance without the tray: post `WM_QUIT` (0x0012) to its GUI thread; it restores every managed window before exiting.

## Code Conventions & Common Patterns

- **Errors**: `Win32Error` (thiserror, `win32/`) and `ProfileError` (`profiles.rs`) are typed. `anyhow` appears only in `gui/mod.rs`, `mcp.rs` and `tray.rs`. Non-test code has no `unwrap`/`expect`; tests may unwrap.
- **Unsafe**: confined to Win32 calls and FFI callbacks (`win32/`, `watcher.rs`, `ipc/pipe.rs`, `diagnose.rs`, `engine.rs`, `tray.rs`, `gui/`, `cli.rs`, `mcp.rs`, `i18n.rs`). Each unsafe site carries a `// SAFETY:` comment. FFI callbacks only filter, collect or post; they do no other work.
- **Handles**: `HWND` crosses module and state boundaries as `isize` via `win32::hwnd_to_isize` / `hwnd_from_isize`. Kernel handles we own (the single-instance mutex, process handles) are wrapped in `OwnedHandle`, which calls `CloseHandle` on drop.
- **Pure logic**: style masks, rect math and profile parsing/matching live in `window_state.rs` and `profiles.rs`, which import no `windows` types. Their unit tests sit in `#[cfg(test)]` modules in the same files.
- **Event handling**: the resident's message-loop thread never calls `thread::sleep`. Delayed work is armed with `SetTimer` on the message-only window (`win32::set_timer`) and killed on its first `WM_TIMER`. The wndproc only queues events (plus killing that timer); the main loop drains the queue after each dispatch.
- **Restore**: `Engine::capture` records original style, ex-style, rect and maximized state before any style or position change (the only earlier touch is `SW_RESTORE` on maximized or iconic windows, to read the normal rect). `restore` attempts every step even after a failure, always forgets the window, and returns the first error.
- **Language and i18n**: the language is `BORDERLESS_SHATTER_LANG` > `settings.json` (`language`: `system`, `en`, `es`; written only by the panel) > the OS UI language. Code, identifiers, comments, commit messages and product docs are in English. User-facing text (GUI, tray, CLI output) goes through the i18n catalogs: English is the default and the source of truth, Spanish is the second shipped language, and a missing key falls back to English; never hard-code a user-facing string. MCP tool descriptions and the engine's `ErrorBody.message` stay English only (they are protocol); clients match error codes, never message text.

## Runtime/Tooling

- **Rust**: the channel pinned in `rust-toolchain.toml` (with `rustfmt` and `clippy`), host `x86_64-pc-windows-msvc`; the MSVC toolchain and linker are required. Run `cargo fmt` and `cargo clippy` before commits.

## Testing & QA

Tiers, fastest first. The first two need no desktop session; the rest create real windows or resident processes.

| Tier | Command | Desktop | Covers |
|---|---|---|---|
| Unit | `cargo test --lib` | no | Pure logic in the crate's `#[cfg(test)]` modules: style and rect math, profile parsing and matching. |
| Property | `cargo test --test properties` | no | Invariants over generated inputs (proptest). |
| Process | `cargo test --test mcp -- --ignored --test-threads=1`, `cargo test --test cli -- --ignored --test-threads=1` | yes | `mcp` and `cli` against an isolated resident. |
| Pipe | `cargo test --lib ipc::pipe -- --ignored` | yes | The named pipe against a real server. |
| End-to-end | `cargo build --examples && cargo test --test e2e -- --ignored --test-threads=1` | yes | A real resident and fake game windows, measured with Win32. |

- `cargo e2e` is an alias for the end-to-end run (`.cargo/config.toml`). Build the examples first: the fake game is an example binary.
- nextest is optional (`.config/nextest.toml`). It runs the `e2e`, `mcp` and `cli` binaries one at a time, with a 60 s timeout per period that terminates after three periods. Doctests are not run by nextest; use `cargo test --doc`.

Isolation (end-to-end and process tests):

- Each harness owns `target/e2e-tmp/<test>-<pid>`, used as `APPDATA` and `LOCALAPPDATA`. The directory is removed on drop.
- A random instance suffix (`e2e-<pid>-<nanos>`) gives the resident its own single-instance mutex and pipe. Valid suffixes are 1 to 32 characters of `A-Za-z0-9-` (an empty value counts as unset). A set but invalid value (or one that is not Unicode) is refused, never ignored: the resident, GUI and MCP exit with `fatal:` and code 1, the CLI exits with code 2 and prints `cli.error.invalid_instance`. The resident and the GUI are GUI-subsystem exes, so `fatal:` is invisible when they start from Explorer or the panel: start them from a console to see it.
- With a suffix, the resident's state file is `state.<suffix>.json`. `profiles.json` and `settings.json` stay shared through `APPDATA`, so the temp `APPDATA` is what keeps them isolated.
- The test process never sets environment variables. The harness passes them only to the processes it starts (`Harness::command`). A process a test starts itself is registered with `Harness::track_pid`.
- Tests touch only the windows they created.
- End-to-end cases run single-threaded. Each starts its own resident.
- A 150 s watchdog kills the tracked processes and exits with code 101. On drop, the harness asks the resident to quit (`WM_QUIT`, so it restores managed windows) and kills it only if that fails.

Determinism: proptest uses the fixed seed `0xB0_5EED`, no failure persistence and no `proptest-regressions` files. The default is 256 cases; go deeper with `PROPTEST_CASES=2000 cargo test --test properties`.

Manual runs: point both `APPDATA` and `LOCALAPPDATA` at a temp folder and set a short `BORDERLESS_SHATTER_INSTANCE` (an invalid one is refused: exit 1 with `fatal:` for the resident, GUI and MCP, exit 2 for the CLI). `LOCALAPPDATA` holds `state.json`, so isolating only `APPDATA` is not enough.

Known gaps: the GUI and tray UI, multi-monitor layouts, non-primary DPI, elevated windows and real games are not covered. A killed resident can leave a ghost tray icon behind; the harness quits gracefully first to avoid that. `.cargo/mutants.toml` configures cargo-mutants for pure logic only; mutation runs are not part of routine QA.

Win32 smoke scripts live outside the repo.

## Hard Rules

- **No GPL code.** Shattered Islands is a clean-room implementation: no code or text from GPL projects is copied, and behaviour is documented from public sources. The project is MIT. Borderless Gaming (GPL-2.0, https://github.com/andrewmd5/Borderless-Gaming) and ihateborders (GPL-3.0, https://github.com/Z1xus/ihateborders, Rust) may be consulted for behaviour and facts (Win32 calls, flags, order, pitfalls), never copied or closely paraphrased: no code, comments or file layout. Cite them as references in notes, not in source.
- **Keep project docs current.** When behaviour, the protocol, a design constraint or test results change, update the matching file in the same change: `README.md` (usage, profiles), `ARCHITECTURE.md` (code organisation, IPC protocol, design constraints), `CONTRIBUTING.md` (build, test, contribution rules), `docs/GAMES.md` (per-game results). `CHANGELOG.md` is generated by git-cliff at release time; never edit it by hand. These files describe the product, never the working method.
- **English in the repo.** Every file in the repo is written in English except the Spanish i18n catalog.
