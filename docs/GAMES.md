# Game compatibility

Record of which games and applications have been tested with Borderless Shatter and how each one behaved.

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

## Notepad

Single monitor of 4096x1152 (virt.) = 5120x1440 physical. Rect (virt.) `[0,0,4096,1152]` = `rcMonitor`; the relative result (rect == `rcMonitor`) is still valid.

| Value | Before | After |
|---|---|---|
| style | `0x14CF0000` | `0x14000000` |
| ex-style | `0x110` | `0x10` |
| rect | — | `[0,0,4096,1152]` (virt.) |

- `WM_QUIT` → exact restore.
- With a 1500 ms delay: 5/5 exact.

## Total War: Warhammer III

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

This is consistent with the immutable size observed. There is a precedent in the same engine: in Shogun 2, setting `fix_res false; fix_window_pos false; gfx_fullscreen false;` made Borderless Gaming work (issue #197, 2015).

**What could be tested (untested):** with the game closed, `fix_res false`, `fix_window_pos false`, and, for a physical 2560x1440 half, `x_res 2048`, `y_res 1152` (virtual pixels, because the game is DPI-unaware: 2048 × 1.25 = 2560).

**Reproduction:** `testwin --preset warscape` imitates this behavior (see below).

## Phoenix Wright: Ace Attorney Trilogy

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

- With `-popupwindow -screen-width 2560 -screen-height 1440` (the size arguments were not respected in the test with 2048x1152).
- Set the resolution in PlayerPrefs to the island's size before launching.

### With `-popupwindow`

Launched with `-popupwindow -screen-width 2048 -screen-height 1152`:

- style `0x94000000` (`WS_POPUP`, no frame bits), ex-style `0`, physical client 1280x720: the size arguments were ignored.
- Registry: `Screenmanager Resolution Width_h182942802` / `Height_h2627697771` = 1280 / 720 (before, 1600 / 900 were read without the `_h` names).
- Manual `SetWindowPos` to `[2560,0,5120,1440]`: Unity reverts it in under 20 ms to 1280x720 at `[3677,0,4957,720]`. At `[0,0,2560,1440]`: `[2397,0,3677,720]`.
- **Limitation of the app:** Borderless Shatter does not touch this window, because `has_targetable_style` requires frame bits and `capture()` returns `Ok(false)`. See D-14.

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

## How to add a game

Record, for each game:

- **Process**: executable name and whether it is elevated.
- **Engine**.
- **Game window mode**: windowed, native borderless or fullscreen.
- **Relevant configuration files**: path and options that affect size, position or mode.
- **Values before/after**: style, ex-style and rect, immediate and with delay.
- **Whether the game reverts**: which value and how many ms later.
- **Restore**: whether it is exact after `WM_QUIT`.
- Borderless Shatter version or commit, date and status (✅/⚠️/❌/❓); add the row to the summary.
