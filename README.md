# Borderless Shatter

Windows utility that removes the frame from game and app windows and fits them to the monitor, with exact restore.

## What it does

Version 0.2:

- Removes the frame (title bar, borders, buttons) from the windows of processes that have a profile and fits them to the monitor, or to an *island*: a named rectangle of a monitor (for example, one half of a 32:9 screen).
- Handles windows that already have no frame, and keeps a guard that re-asserts the placement a limited number of times against games that fight back.
- Restores the window exactly as it was (style, position, size, maximized state) when requested and on exit; after an abrupt shutdown it re-adopts the modified windows from `state.json`.
- Detects new windows through Windows events, without polling.
- Lives in the system tray, with a panel (GPUI) to manage windows, islands, profiles and diagnostics.
- Offers an MCP server and a command-line client, both clients of the same named pipe.
- Single instance: a second run does not create another icon.
- Does not need administrator permissions.

## Status

Version 0.2. The earlier prototype (v0.1) was tested on 2026-11-25 on one 5120x1440 physical monitor at 125% scale:

- Notepad: applies and restores exactly; with a 1500 ms delay, 5 of 5 exact.
- Test window `testwin`, 12 scenarios (immediate and with a 1500 ms delay): exact restore in all of them.
- Total War: WARHAMMER III: the game reverts the changes (see [Known limitations](#known-limitations)); the restore is exact.

Not tested: multiple monitors, windows of elevated processes, Phoenix Wright: Ace Attorney Trilogy.

Per-game details in [`docs/GAMES.md`](docs/GAMES.md).

## Requirements and build

- Windows 10/11 x64.
- Stable Rust with the MSVC toolchain.

```
cargo build --release
```

The executable ends up in `target\release\borderless-shatter.exe`. It weighs about 18 MB because of the GUI. A clean release build takes about 5 min and a debug build about 1.5 min (GPUI).

Use the release build for everyday work: it is a Windows-subsystem program. The debug build is a console program and opens a console window.

The Windows manifest (asInvoker, PerMonitorV2, Common Controls and SegmentHeap) is provided by `gpui-pre` through `gpui-kit`; the project has no `build.rs` or `app.manifest` of its own (D-17).

Tests: `cargo test` runs the unit, property and doc tests. The suites that open real windows are ignored by default; see [`CONTRIBUTING.md`](CONTRIBUTING.md) and the "Testing & QA" section of [`AGENTS.md`](AGENTS.md).

## Usage

1. Run `borderless-shatter.exe`. The icon appears in the tray.
2. Double-click the icon, or choose "Open panel", to open the panel (the subcommand `borderless-shatter gui`, a separate process that talks to the resident process through the named pipe, D-16). A left click on the icon does nothing; a right click opens the menu.
3. Create or edit profiles in the panel, or in [`profiles.json`](#profiles) followed by "Reload profiles".
4. When a window of a process with a profile opens, its frame is removed and it is fitted to its monitor or island.

Tray menu:

| Action | Effect |
|---|---|
| `Open panel` | Opens the panel |
| `Apply: <process>` (one per profile) | Applies the profile to that process's windows now |
| `Place active window in island` | Submenu with one entry per `layout/island` |
| `Restore all` | Returns all windows to their original state |
| `Reload profiles` | Re-reads `profiles.json` and rebuilds the menu |
| `Quit` | Restores the windows and closes the program |

A window restored from the tray is not automatically handled again.

Panel views:

- **Windows**: lists the open windows with their managed state (and the island or "fights back" flag), with search, "Managed only" and "Show all" filters, and "Restore all".
- **Islands**: shows the monitors and their islands to scale, with the window that occupies each one.
- **Island editor**: creates and edits layouts and islands by dragging, snapping or typing values. The built-in preset is read-only: "Duplicate to edit" makes an editable copy.
- **Profiles**: edits the per-game profiles (island, game resolution, delay, enabled).
- **Diagnostics**: applies a profile to a window and measures how the game reverts the style and the size, with a verdict and recommendations.

Language: the panel has a language selector (System, English, Spanish). The choice is stored in `settings.json`. The language is picked in this order: the `BORDERLESS_SHATTER_LANG` environment variable (`en` or `es`), then `settings.json`, then the Windows display language (Spanish if it is Spanish, English otherwise). The tray menu picks the new language the next time the program starts. MCP tool descriptions and the engine's error messages are always English.

## Profiles

File: `%APPDATA%\BorderlessShatter\profiles.json`. If it does not exist, it is created empty at startup. The format version is `2`; a higher version is rejected and the file is left untouched. A version `1` file is read as is and migrated on the next save; the original is first copied once to `profiles.v1.bak`. Every save also keeps the replaced file as `profiles.json.prev`. An invalid file is reported and never overwritten.

```json
{
  "version": 2,
  "layouts": [
    {
      "id": "ultrawide-thirds",
      "monitor": "primary",
      "islands": [
        { "id": "left", "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 1.0 } },
        { "id": "right-px", "rect": { "unit": "px", "x": 2560, "y": 0, "w": 2560, "h": 1440 } }
      ]
    }
  ],
  "profiles": [
    {
      "process": "Warhammer3.exe",
      "enabled": true,
      "island": "halves-16-9/left",
      "game_resolution": { "width": 2048, "height": 1152 },
      "delay_ms": 0
    },
    {
      "process": "notepad.exe",
      "monitor": "current",
      "offsets": { "left": 0, "top": 0, "right": 0, "bottom": 0 }
    }
  ]
}
```

| Field | Type | Default | Description |
|---|---|---|---|
| `version` | number | required | Format version (`1` or `2`) |
| `layouts` | list | empty | Your layouts, merged over the built-in presets |
| `profiles` | list | empty | Profiles |
| `process` | text | required | Executable name |
| `enabled` | boolean | `true` | If `false`, the profile is not applied |
| `island` | text | none | `"layout/island"`; overrides `monitor` and `offsets` |
| `game_resolution` | `{width, height}` | none | Declared game resolution in physical pixels; the window is centred in the island |
| `monitor` | `"current"` or number | `"current"` | Current monitor of the window, or monitor index (integer ≥ 0); ignored when `island` is set |
| `offsets` | object | all `0` | `left` and `top` shift the origin; `right` and `bottom` adjust the width and height: `width = monitor − left + right`, `height = monitor − top + bottom`. Each omitted key is `0`; ignored when `island` is set |
| `delay_ms` | number | `0` | Wait in milliseconds before applying (at most 60000) |
| `clip_cursor` | boolean | `false` | Accepted and ignored |

Fields this version does not know are kept when the file is saved.

## Islands

An island is a named rectangle of a monitor. A layout groups the islands of one monitor; an island is referred to as `"layout/island"`.

| Layout field | Description |
|---|---|
| `id` | Layout id |
| `monitor` | `"primary"`, a monitor index, or a device name such as `"\\.\\DISPLAY1"` |
| `islands` | List of `{ "id", "rect" }` |

`rect` is `{ x, y, w, h, unit }`. With `"unit": "fraction"` (the default) the values are fractions of the monitor, `0` to `1`, with at most 6 decimals. With `"unit": "px"` they are physical pixels from the monitor's top-left corner. The edges are rounded, not the sizes, so neighbouring islands never leave a gap.

Built-in preset, read-only (to change it, duplicate it into your own layout; a layout of yours with the same id replaces it):

| Island | Rectangle |
|---|---|
| `halves-16-9/left` | Left half of the primary monitor |
| `halves-16-9/right` | Right half of the primary monitor |

On a 5120x1440 monitor each half is 2560x1440, 16:9. The ids used before v0.2 final still work as aliases: `mitades-16-9/izquierda` is `halves-16-9/left` and `mitades-16-9/derecha` is `halves-16-9/right`. Your files are not rewritten because of an alias.

## Data files

| File | Location | Purpose |
|---|---|---|
| `profiles.json` | `%APPDATA%\BorderlessShatter\` | Profiles and layouts (see [Profiles](#profiles)) |
| `profiles.json.prev` | same | The file as it was before the last save |
| `profiles.v1.bak` | same | The original version 1 file, copied once on migration |
| `settings.json` | same | `{ "version": 1, "language": "system" \| "en" \| "es" }`; written only by the panel; an invalid or newer file is ignored and never overwritten |
| `state.json` | `%LOCALAPPDATA%\BorderlessShatter\` | Original state of the managed windows, used to re-adopt them after an abrupt shutdown |
| `state.<suffix>.json` | same | The same, for an isolated instance started with `BORDERLESS_SHATTER_INSTANCE=<suffix>` (1 to 32 characters of `A-Za-z0-9-`; it also gives that instance its own pipe and single-instance lock). An empty value counts as unset. A set but invalid value (other characters, too long, or not Unicode) is refused: the resident, `gui` and `mcp` exit with `fatal:` and code 1, the CLI exits with code 2 and prints `cli.error.invalid_instance`; it never falls back to the real instance |
| `state.json.bad` | same | A state file that could not be read, moved aside so its data is not lost |

## MCP server

`borderless-shatter mcp` serves the Model Context Protocol over stdio: one tool per pipe method (`list_windows`, `apply`, `place`, `restore`, `restore_all`, `list_islands`, `get_profiles`, `set_profiles`, `diagnose`). Results come back as `structuredContent`; a resident error comes back as a tool result with `isError: true` and `code: message`. If the resident is not running, the first tool call starts it, and `_meta` then reports `started_resident`.

Register it in your MCP client with the path of the release executable. For example, in a client's `mcp.json`:

```json
{
  "mcpServers": {
    "borderless-shatter": {
      "type": "stdio",
      "command": "C:\\path\\to\\borderless-shatter.exe",
      "args": ["mcp"]
    }
  }
}
```

Other clients take the same two values: `"command"` and `"args": ["mcp"]`. Prefer the release build; the debug build opens a console window unless the client starts it without one.

## Command line

```
borderless-shatter <subcommand> [options] [--pretty]
```

The subcommands talk to the resident program, which must be running; the CLI never starts it.

| Subcommand | Options |
|---|---|
| `list-windows` | `--process P`, `--include-hidden`, `--only-managed` |
| `apply` | window, `--island LAYOUT/ISLAND`, `--monitor N\|primary\|\\.\DISPLAYn`, `--offsets LEFT,TOP,RIGHT,BOTTOM`, `--delay-ms N`, `--no-profile` |
| `place` | window, `--island LAYOUT/ISLAND`, `--game-resolution WIDTHxHEIGHT` |
| `restore` | window |
| `restore-all` | |
| `list-islands` | |
| `profiles get` | |
| `profiles set` | `<file.json>` or `-` for stdin, `--expected-revision R` |
| `diagnose` | window, `--island`, `--monitor`, `--observe-ms N` (at most 15000), `--keep` |
| `help`, `--help` | Prints the usage |

A window is `--hwnd N`, or `--process P` with an optional `--title T`.

The result is JSON on stdout; `--pretty` indents it. Exit codes:

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | The resident answered with an error: `{"error":{"code","message","data"}}` on stdout |
| `2` | Usage error, on stderr; the resident was not contacted |
| `3` | The resident is unreachable, or the transport or protocol failed |

The release build is a Windows-subsystem program, so `cmd` and PowerShell do not wait for it when it is typed at the prompt, and its output is not captured unless you ask for it:

- In PowerShell, pipe through a cmdlet to wait and capture the output, and set the console encoding for non-ASCII text:

  ```powershell
  [Console]::OutputEncoding = [Text.UTF8Encoding]::new()
  $out = borderless-shatter list-windows --only-managed | Out-String
  borderless-shatter list-islands | ConvertFrom-Json
  borderless-shatter profiles get | Out-File profiles-copy.json
  ```

- In `cmd`, run it through `cmd /c`, which waits for it: `cmd /c borderless-shatter restore-all`. The exit code is then in `%ERRORLEVEL%`.

## Test program

`testwin` (`examples/testwin.rs`) is a fake window that reproduces how games defend themselves when a tool removes their frame or changes their size. It lets you test the app without depending on a specific game.

```
cargo run --example testwin -- --preset warscape
cargo run --example testwin -- --preset unity --log testwin.log
cargo run --example testwin -- --style fixed --revert-style 8 --force-size 2 --reposition keep --dpi unaware --client 1920x1080
```

Presets (measured on real games; details in `docs/GAMES.md`):

| Preset | Reproduces |
|---|---|
| `notepad` | Cooperative application: accepts the change without reacting (`--style frame --dpi aware`) |
| `warscape` | Total War: WARHAMMER III: restores the frame in ~8 ms and forces its size in ~2 ms |
| `unity` | Phoenix Wright (Unity, windowed): removes the style without restoring it and forces 1280x720 at ~60 ms |
| `unity-popup` | Phoenix Wright with `-popupwindow` (no frame): returns to 1280x720 in less than 20 ms |

Flags, as they appear in the code:

- `--preset NAME` and `--mode NAME`: expand a preset or an old mode; flags that come after override them.
- `--style frame|fixed|popup` (default `frame`).
- `--revert-style MS`: restores the style after MS milliseconds.
- `--force-size MS`: forces the size after MS milliseconds.
- `--reposition keep|center-on-request|offset:X,Y|game-unity` (default `keep`).
- `--late-frame MS`: restores the frame after MS milliseconds.
- `--dpi aware|unaware` (default `aware`). With `unaware`, Windows virtualizes the window: 1920x1080 appears as 2400x1350 physical pixels at 125%.
- `--client WxH` (default 1280x720, in the window's DPI space), `--pos X,Y` (default 100,100), `--delay MS` (default 1500; only used by `late-frame`), `--title TEXT`, `--log PATH`.

The old `--mode` names (`normal`, `fixed`, `revert-style`, `force-size`, `warscape`, `late-frame`) still work.

## Known limitations

- Games that defend themselves: Total War: WARHAMMER III reverts the style (~8 ms) and forces its size (~2 ms). The cause is in `preferences.script.txt` (`fix_res true`, `fix_window_pos true`). Changing those settings to make it work is untested.
- Windows of elevated processes: untested; without administrator permissions, Windows may prevent modifying them.
- While the program is not running, modified windows stay without a frame; at the next start it re-adopts them from `state.json` and can restore them.
- `clip_cursor` (lock the mouse to the island) is accepted and ignored.
- Only tested with one monitor. `list_islands` skips layouts whose monitor is disconnected; the panel shows them as unavailable.
- The CLI cannot start the resident; the MCP server can.
- A release-build CLI does not make the shell wait for it; use the PowerShell and `cmd /c` forms above. A console shim executable is planned.

## Roadmap

Summary of [`docs/VISION.md`](docs/VISION.md): the goal is "islands", named rectangles on the monitor, to split a 32:9 screen into two 16:9 ones and place one game in each.

- **v0.1** (done): borderless at full monitor size, exact restore and tray.
- **v0.2** (this version): islands in JSON (first preset: two 16:9 halves), windows that already have no frame, guard with re-assertions, IPC over a named pipe, GUI with GPUI, MCP server and CLI.
- **v0.3**: keyboard shortcuts to move the active window to an island.
- **Later**: container with `SetParent` only if needed, per-game settings, console shim for the CLI.

## Documentation

- [`docs/VISION.md`](docs/VISION.md): vision and islands.
- [`docs/V0.2.md`](docs/V0.2.md): v0.2 specification.
- [`docs/GUI.md`](docs/GUI.md): the panel.
- [`docs/PROTOTYPE.md`](docs/PROTOTYPE.md): v0.1 specification.
- [`docs/DECISIONS.md`](docs/DECISIONS.md): design decisions.
- [`docs/GAMES.md`](docs/GAMES.md): results per game.
- [`ARCHITECTURE.md`](ARCHITECTURE.md): how the code is organised.
- [`CONTRIBUTING.md`](CONTRIBUTING.md): building, testing and contributing.
- [`CHANGELOG.md`](CHANGELOG.md): change history.

## License

MIT. Own implementation: no code is copied from other projects. [Borderless Gaming](https://github.com/andrewmd5/Borderless-Gaming) (GPL-2.0) and [ihateborders](https://github.com/Z1xus/ihateborders) (GPL-3.0) were used only as behavior references.
