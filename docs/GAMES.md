# Game compatibility

Record of which games and applications have been tested with Borderless Shatter and how each one behaved. Results are grouped by engine, because the engine decides most of how a game reacts; the use cases section groups them by the situation you want to solve.

## How to read this page

Statuses:

- ✅ works
- ⚠️ works with settings
- ❌ defends itself (the game undoes the change)
- ❓ untested

What is measured in each test:

- **style**: window style (`GWL_STYLE`).
- **ex-style**: extended style (`GWL_EXSTYLE`).
- **rect**: window rectangle.
- **restore**: whether, when closing Borderless Shatter (`WM_QUIT`), the three values above return exactly to the original.

> **Note on pixels.** The monitor is a single one, **5120x1440 physical**, at 125% (120 DPI). The first tests were done with non-DPI-aware scripts and Windows virtualized the readings: 4096x1152 was the virtualized view (÷1.25). Values marked **(virt.)** come from that virtualized space; for the physical value, multiply by 1.25. The relative results ("rect == `rcMonitor`", styles) are still valid. Borderless Shatter is PerMonitorV2 and works in physical pixels.

## Summary

| Game / app | Engine | Version / commit | Date | Status | Note |
|---|---|---|---|---|---|
| Notepad | Native Win32 | prototype v0.1 | 2026-11-25 | ✅ | Applies and restores exactly, 5/5 with delay |
| Total War: Warhammer III | Warscape | prototype v0.1 | 2026-11-25 | ❌ | The game reverts style and size; cause in `preferences.script.txt` |
| Phoenix Wright: Ace Attorney Trilogy | Unity | `3ecb3c9` | 2026-11-25 | ⚠️ | Removes the frame; Unity forces 1280x720 and repositions |

## Engines

### Native Win32

Applications that use the plain Windows frame and do not react to style or size changes.

**Engine notes:** a cooperative window accepts any rect; the frame is removed and the restore is exact. Nothing to configure.

#### Notepad

Single monitor of 4096x1152 (virt.) = 5120x1440 physical. Rect (virt.) `[0,0,4096,1152]` = `rcMonitor`; the relative result (rect == `rcMonitor`) is still valid.

| Value | Before | After |
|---|---|---|
| style | `0x14CF0000` | `0x14000000` |
| ex-style | `0x110` | `0x10` |
| rect | — | `[0,0,4096,1152]` (virt.) |

- `WM_QUIT` → exact restore.
- With a 1500 ms delay: 5/5 exact.

### Warscape

**Engine notes:**

- **DPI:** unaware. Windows virtualizes the window and scales its bitmap, so the game's own resolution settings (`x_res`, `y_res`) are in virtual pixels: divide a physical target by `dpi/96` (2560 physical = `x_res 2048` at 125%).
- **Window size and position are locked by the game's settings** (`fix_res`, `fix_window_pos` in `preferences.script.txt`); with them on, the engine undoes our style write and our `SetWindowPos`.
- **Precedent in the same engine:** in Shogun 2, setting `fix_res false; fix_window_pos false; gfx_fullscreen false;` made Borderless Gaming work (issue #197, 2015).

#### Total War: Warhammer III

Process `Warhammer3.exe`, in windowed mode, not elevated. **DPI:** unaware (awareness 0, `GetDpiForWindow` 96); Windows virtualizes it and scales the bitmap. Physical window `[1350,6,3770,1405]`, physical client 2400x1350 = `x_res 1920` × 1.25 and `y_res 1080` × 1.25. The rects in the timeline (e.g. `[0,0,1936,1119]`) are (virt.).

**Original state:** style `0x14CA0000` (without `THICKFRAME`), ex-style `0x100`.

**Measured timeline:**

- Our style write: the game reverts it about 8 ms later.
- Our `SetWindowPos` to `rcMonitor`: the game reverts it about 2 ms later to `[0,0,1936,1119]` (client 1920x1080 plus frame), keeping the position 0,0.
- With delay: same behavior.
- Restore: exact.

**Cause:** the file `%APPDATA%\The Creative Assembly\Warhammer3\scripts\preferences.script.txt` contains:

- `fix_res true` ("Forbid window resizing")
- `fix_window_pos true` ("Forbid window repositioning")
- `x_res 1920`, `y_res 1080`

This is consistent with the immutable size observed.

**What could be tested (untested):** with the game closed and a copy of the file kept, `fix_res false`, `fix_window_pos false`, and, for a physical 2560x1440 half, `x_res 2048`, `y_res 1152` (virtual pixels, because the game is DPI-unaware: 2048 × 1.25 = 2560). Borderless Shatter only recommends these settings; it does not write to game files.

**Reproduction:** `testwin --preset warscape` imitates this behavior (see [Test program](#test-program-testwin)).

### Unity

**Engine notes:**

- Detected from `UnityPlayer.dll`, a `<game>_Data` folder and the window class `UnityWndClass`.
- Per-monitor DPI aware in the game tested (awareness 2): physical = logical, no conversion.
- Command-line arguments that affect the window, from the [Unity Player command line arguments reference](https://docs.unity.com/en-us/engine/6000.6/manual/unity-editor/command-line-arguments/player.md) (checked 2026-11-26): `-popupwindow` creates the window as a dialog window without a frame; `-screen-width` and `-screen-height` override the default size and "must be an integer from a supported resolution"; `-window-mode` (Windows only) accepts `exclusive` or `borderless`. `-window-mode borderless` is untested here.
- Unity moves and resizes its own window shortly after any external `SetWindowPos`; the size comes back in under 200 ms, and the final position changes from run to run.

#### Phoenix Wright: Ace Attorney Trilogy

Process `PWAAT.exe` (Steam, `<Steam library>\steamapps\common\Phoenix Wright Ace Attorney Trilogy\`), Unity engine (`UnityPlayer.dll`, `PWAAT_Data`, window class `UnityWndClass`), 32-bit, in windowed mode, not elevated and launched without Unity arguments. Single monitor of 5120x1440 physical. **DPI:** per-monitor aware (awareness 2, DPI 120): physical = logical; client in a 1280x720 physical window. The coordinates in the timeline below (x=2126, 1732, 1852…) are (virt.) from the previous script; ×1.25 in physical.

**Configuration:** registry `HKCU\Software\CAPCOM\Phoenix Wright: Ace Attorney Trilogy / …` (read only):

- `Screenmanager Resolution Width`: 1600
- `Screenmanager Resolution Height`: 900
- `Screenmanager Is Fullscreen mode`: 0

It does not match the real client, which is 1280x720. The origin of 1280x720 is unknown.

**Original state:** style `0x14CA0000`, ex-style `0x100`, rect `[231,223,1516,971]`, client 1280x720.

**Measured timeline, immediate:**

| Moment | Event | Values |
|---|---|---|
| +48 ms | The style is removed and not restored | style `0x14000000`, ex-style `0` |
| +69 ms | Our `SetWindowPos` to `rcMonitor` | rect `rcMonitor` |
| +130 ms | Unity resizes to its client and repositions | `[2126,0,3406,720]` |
| +150 ms | Our app re-asserts the window | — |
| +186 ms | Unity reverts the size again | final 1280x720 at x=2126 |

**Measured timeline, with a 3000 ms delay:**

| Moment | Event | Values |
|---|---|---|
| ~+48 ms | Unity had already returned the window to its size | 1280x720 at x=1732 |
| ~3053 ms | The style is removed and not restored | style `0x14000000`, ex-style `0` |
| ~3091 ms | Our `SetWindowPos` to `rcMonitor` | rect `rcMonitor` |
| ~3153 ms (≈ +62 ms) | Unity reverts the size | final 1280x720 at x=1852 |

The final position changes from one test to another (2126 in the immediate one, 1852 in the delayed one). Its origin is unknown.

- `WM_QUIT` → exact restore, in both tests.

**Comparison:**

- Versus TW:WH3 (`warscape`): here Unity does not restore the style, so the frame stays removed. Both games revert the size.
- Versus `testwin --mode force-size`: same behavior (style accepted, size forced), plus the repositioning.

**Status ⚠️:** the frame stays removed, but the size does not. The legend has no partial status; ⚠️ is used because the window does end up without a frame.

**Untested ideas:**

- With `-popupwindow -screen-width 2560 -screen-height 1440` (the size arguments were not respected in the test with 2048x1152, which may not be a supported resolution).
- Set the resolution in PlayerPrefs to the island's size before launching.

##### With `-popupwindow`

Launched with `-popupwindow -screen-width 2048 -screen-height 1152`:

- style `0x94000000` (`WS_POPUP`, no frame bits), ex-style `0`, physical client 1280x720: the size arguments were ignored.
- Registry: `Screenmanager Resolution Width_h182942802` / `Height_h2627697771` = 1280 / 720 (before, 1600 / 900 were read without the `_h` names).
- Manual `SetWindowPos` to `[2560,0,5120,1440]`: Unity reverts it in under 20 ms to 1280x720 at `[3677,0,4957,720]`. At `[0,0,2560,1440]`: `[2397,0,3677,720]`.
- **Limitation of the v0.1 app:** Borderless Shatter did not touch this window, because the v0.1 guard (`has_targetable_style`) required frame bits and `capture()` returned `Ok(false)`. From v0.2 the guard decides by the target rect, so a window that already has no frame is managed (`testwin --preset unity-popup` reproduces it).

## Use cases

Each case points to the games on this page that show it. This page records no real-game run inside an island yet; the island figures below come from manual placement or from `testwin`.

### Two games side by side on halves

Preset `halves-16-9`: two 2560x1440 islands on a 5120x1440 monitor. Each game needs to accept a placement at a rect that is not the whole monitor.

- [Notepad](#notepad): accepts any rect and restores exactly.
- [Phoenix Wright](#phoenix-wright-ace-attorney-trilogy): moves to the island rect but forces its client to 1280x720; set the game resolution (`game_resolution` hint in the profile) so the window is centred in the island.
- [Total War: Warhammer III](#total-war-warhammer-iii): not placed until its `fix_res` and `fix_window_pos` settings change.

### DPI-unaware game on a scaled monitor

The engine works in physical pixels; a DPI-unaware game gets the physical rect and Windows scales its bitmap, but its own resolution settings are virtual pixels (divide by `dpi/96`).

- [Total War: Warhammer III](#total-war-warhammer-iii): awareness 0, DPI 96, client 2400x1350 = `x_res 1920` × 1.25.
- Contrast: [Phoenix Wright](#phoenix-wright-ace-attorney-trilogy) is per-monitor aware; physical = logical.

### Game that fights back

The game puts its frame or its size back after our write. The guard re-asserts a few times, then stops and marks `fights_back`; `diagnose` measures what the game does.

- [Total War: Warhammer III](#total-war-warhammer-iii): reverts style (~8 ms) and size (~2 ms); the cause is in its settings file.
- [Phoenix Wright](#phoenix-wright-ace-attorney-trilogy): keeps the removed style but forces the size (~60 ms) and repositions.
- Test double for a late revert: `testwin --late-frame 1500` (no real game recorded).

### Game that already has no frame

A window created without frame bits (Unity with `-popupwindow`, style `0x94000000`) still needs placing and restoring.

- [Phoenix Wright with `-popupwindow`](#with--popupwindow): `WS_POPUP`, no frame bits; Unity reverts a manual placement in under 20 ms.

### Cooperative application

An application that does not react to our changes; the baseline for exact restore.

- [Notepad](#notepad): 5/5 exact, with and without delay.

## Test program `testwin`

`examples/testwin.rs` is a fake window that imitates game behaviors. Usage: `testwin [--preset warscape|unity|unity-popup|notepad] [--style frame|fixed|popup] [--revert-style MS] [--force-size MS] [--reposition keep|center-on-request|offset:X,Y|game-unity] [--late-frame MS] [--dpi aware|unaware] [--client WxH] [--pos X,Y] [--delay MS] [--title TEXT] [--log PATH]`. The old `--mode` names still work.

Fidelity of the presets (physical pixels, monitor 5120x1440 at 125%, validated 2026-11-25):

| Preset | Real game | What matches | Differences |
|---|---|---|---|
| `warscape` | TW:WH3 | Restores the style and forces the size (window 2420x1399, client 2400x1350). Keeps position 0,0 and reverts the app's re-assertion. | Timings only: restores the style in 15–40 ms versus ~8 ms real, and forces the size in ~7 ms versus ~2 ms. `SetTimer` has a minimum of 10 ms and a tick of ~15.6 ms. |
| `unity` | Phoenix Wright (Unity, windowed) | The removed style stays removed. The size is forced to 1280x720 about 62 ms after our `SetWindowPos` (real: ~60 ms). | Repositioning: the real game moved the window (x=2126 / 1852); testwin keeps 0,0. The repositioning rule is not reproduced. |
| `unity-popup` | Phoenix Wright with `-popupwindow` | Style `0x94000000` untouched, as in the real one. A manual `SetWindowPos` to `[2560,0,5120,1440]` returns to 1280x720 in 21 ms (real: < 20 ms). | Position after reverting: testwin `[2560,0,3840,720]`; real `[3677,0,4957,720]`. |
| `notepad` | Notepad (cooperative application) | Accepts borderless 5120x1440 without reacting. | No measurements. |

The restore was exact in all runs of the app.

The previous matrix of 6 modes (12 scenarios) was done with the earlier version, in virtualized pixels; its figures do not apply to this table.

## Contributing a result

Copy this template into the engine's section (add a new `###` section if the engine is not listed), and add a row to the summary table. Write `Engine: unknown` when you cannot name the engine from a primary or official source; never guess.

```markdown
#### <Game or app> <✅|⚠️|❌|❓>

- **Engine:** <name, or `unknown`> (source: <link or file that shows it>)
- **Process:** `<exe>`, elevated: <yes|no>
- **Window mode:** <windowed | native borderless | fullscreen>
- **DPI awareness:** <unaware | system | per-monitor>, DPI <n>
- **Configuration files:** <path and the options that affect size, position or mode>
- **Before / after:** style `0x…`, ex-style `0x…`, rect `[l,t,r,b]`, immediate and with delay
- **Does the game revert it?** <which value, how many ms later>
- **Restore:** <exact after `WM_QUIT` or not>
- **Borderless Shatter version or commit, date**
- **Use cases it shows:** <links to the use cases above>
```
