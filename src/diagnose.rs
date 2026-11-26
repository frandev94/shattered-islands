//! `diagnose` engine: a read-only high-frequency sampler plus a pure analysis of the timeline it
//! produces (what the game reverted after our writes, how fast, and what to change in the game).

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::islands::Size;
use crate::win32::{get_styles, hwnd_from_isize, window_rect};
use crate::window_state::{Rect, STYLE_REMOVE};

/// A deviation this long after our last write counts as a late revert, not part of the
/// immediate fight (the `late-frame` pattern, D-09).
const LATE_REVERT_MS: f64 = 1000.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Sample {
    /// Milliseconds since the sampler started.
    pub t_ms: f64,
    pub style: u32,
    pub exstyle: u32,
    pub rect: Rect,
}

/// Closes the timer handle on drop.
struct Ticker(Option<HANDLE>);

impl Ticker {
    /// ~1 ms periodic high-resolution timer; without one (pre-1803 Windows) `wait` just sleeps.
    fn new() -> Self {
        // SAFETY: null attributes/name are valid; the handle is closed in `Drop`.
        let created = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        };
        let Ok(h) = created else { return Self(None) };
        let due = -10_000i64; // relative due time in 100 ns units = 1 ms
        // SAFETY: `h` is a valid timer handle, `due` outlives the call, no completion routine.
        if unsafe { SetWaitableTimer(h, &due, 1, None, None, false) }.is_err() {
            // SAFETY: `h` was just created and is not used afterwards.
            let _ = unsafe { CloseHandle(h) };
            return Self(None);
        }
        Self(Some(h))
    }

    fn wait(&self) {
        match self.0 {
            // SAFETY: `h` is a valid timer handle owned by `self`.
            Some(h) => {
                let _ = unsafe { WaitForSingleObject(h, 100) };
            }
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: the handle came from `CreateWaitableTimerExW` and is closed only here.
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

pub struct Sampler {
    thread: Option<JoinHandle<Vec<Sample>>>,
}

impl Sampler {
    /// Waits for the sampler to stop (deadline or window gone) and returns every sample taken.
    pub fn finish(self) -> Vec<Sample> {
        self.thread.and_then(|t| t.join().ok()).unwrap_or_default()
    }
}

/// Starts a background, strictly read-only sampler of style, ex-style and rect of `hwnd`
/// (no window writes). `t_ms` counts from this call, so the caller can stamp its own writes with
/// the same clock origin.
pub fn start_sampler(hwnd: isize, observe_ms: u64) -> Sampler {
    let t0 = Instant::now();
    let thread = std::thread::Builder::new()
        .name("diagnose-sampler".into())
        .spawn(move || sample_loop(hwnd, observe_ms, t0))
        .ok();
    Sampler { thread }
}

fn sample_loop(hwnd: isize, observe_ms: u64, t0: Instant) -> Vec<Sample> {
    let h = hwnd_from_isize(hwnd);
    let deadline = Duration::from_millis(observe_ms);
    let mut out = Vec::with_capacity(observe_ms.min(20_000) as usize + 16);
    let ticker = Ticker::new();
    loop {
        // A failed read means the window is gone.
        let (Ok((style, exstyle)), Ok(rect)) = (get_styles(h), window_rect(h)) else {
            break;
        };
        let elapsed = t0.elapsed();
        out.push(Sample {
            t_ms: elapsed.as_secs_f64() * 1000.0,
            style,
            exstyle,
            rect,
        });
        if elapsed >= deadline {
            break;
        }
        ticker.wait();
    }
    out
}

/// One write of ours. `op` is `"set_styles"` or `"set_pos"`; `t_ms` must be taken **after** the
/// call returns, on the sampler's clock, so samples after it reflect the write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WriteMark {
    pub t_ms: f64,
    pub op: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accepted,
    StyleReverted,
    SizeForced,
    PositionForced,
    StyleAndSizeReverted,
    LateRevert,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Latencies {
    pub style: Option<f64>,
    pub rect: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Recommendation {
    pub code: String,
    /// English text; clients render their own sentence from `code` and `data`.
    pub text: String,
    /// The numbers behind `text`, for clients that render it themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Analysis {
    pub verdict: Verdict,
    pub forced_size: Option<Size>,
    pub revert_latency_ms: Latencies,
    pub recommendations: Vec<Recommendation>,
}

fn first_write(writes: &[WriteMark], op: &str) -> Option<f64> {
    writes
        .iter()
        .filter(|w| w.op == op)
        .map(|w| w.t_ms)
        .min_by(f64::total_cmp)
}

fn same_size(a: Rect, b: Rect) -> bool {
    a.width() == b.width() && a.height() == b.height()
}

/// Classifies what the window did after our writes. Pure: the verdict comes only from samples
/// taken after the first write of each kind. `dpi` is the measured effective DPI of the
/// monitor the window was placed on.
pub fn analyse(
    target: Rect,
    writes: &[WriteMark],
    samples: &[Sample],
    dpi_awareness: &str,
    dpi: u32,
    process: &str,
) -> Analysis {
    let t_style = first_write(writes, "set_styles");
    let t_pos = first_write(writes, "set_pos");

    // The frame is back after we removed it. A window that never had frame bits can't trip this.
    let style_dev = t_style.and_then(|t| {
        samples
            .iter()
            .find(|s| s.t_ms > t && s.style & STYLE_REMOVE != 0)
    });
    let after_pos = |t: f64| {
        samples
            .iter()
            .filter(move |s| s.t_ms > t && s.rect != target)
    };
    let rect_dev = t_pos.and_then(|t| after_pos(t).next());
    let size_dev = t_pos.and_then(|t| after_pos(t).rfind(|s| !same_size(s.rect, target)));
    let pos_dev = t_pos.is_some() && rect_dev.is_some();

    let forced_size = size_dev.map(|s| Size {
        width: s.rect.width(),
        height: s.rect.height(),
    });
    let revert_latency_ms = Latencies {
        style: style_dev.zip(t_style).map(|(s, t)| (s.t_ms - t).max(0.0)),
        rect: rect_dev.zip(t_pos).map(|(s, t)| (s.t_ms - t).max(0.0)),
    };

    // Late: the first deviation of any kind came long after the last write preceding it.
    let first_dev = [style_dev, rect_dev]
        .into_iter()
        .flatten()
        .map(|s| s.t_ms)
        .min_by(f64::total_cmp);
    let late = first_dev.is_some_and(|d| {
        writes
            .iter()
            .map(|w| w.t_ms)
            .filter(|&t| t < d)
            .max_by(f64::total_cmp)
            .is_some_and(|last| d - last >= LATE_REVERT_MS)
    });

    let verdict = match (style_dev.is_some(), forced_size.is_some(), pos_dev) {
        _ if late => Verdict::LateRevert,
        (true, true, _) => Verdict::StyleAndSizeReverted,
        (true, false, _) => Verdict::StyleReverted,
        (false, true, _) => Verdict::SizeForced,
        (false, false, true) => Verdict::PositionForced,
        (false, false, false) => Verdict::Accepted,
    };
    let recommendations = recommend(
        verdict,
        forced_size.as_ref(),
        target,
        dpi_awareness,
        dpi,
        process,
    );
    Analysis {
        verdict,
        forced_size,
        revert_latency_ms,
        recommendations,
    }
}

fn rec(code: &str, text: String, data: Option<serde_json::Value>) -> Recommendation {
    Recommendation {
        code: code.into(),
        text,
        data,
    }
}

fn recommend(
    verdict: Verdict,
    forced: Option<&Size>,
    target: Rect,
    dpi_awareness: &str,
    dpi: u32,
    process: &str,
) -> Vec<Recommendation> {
    let (w, h) = (target.width(), target.height());
    let unaware = dpi_awareness == "unaware";
    let name = process.to_lowercase();
    let warscape = unaware
        || verdict == Verdict::StyleAndSizeReverted
        || name.contains("warhammer")
        || name.contains("shogun");
    let mut out = Vec::new();

    match verdict {
        Verdict::Accepted => return out,
        Verdict::StyleReverted | Verdict::LateRevert => {
            let n = crate::guard::GuardConfig::default().max_reasserts;
            out.push(rec(
                "style_reverted",
                format!(
                    "{process} restores the window frame after it is removed. Check that its \
                     settings do not force framed windowed mode; Borderless Shatter re-asserts \
                     the style up to {n} times per cycle and stops insisting if the game fights \
                     back."
                ),
                Some(json!({ "max_reasserts": n })),
            ))
        }
        Verdict::PositionForced => out.push(rec(
            "position_forced",
            format!(
                "{process} moves the window back after it is moved, although it keeps the size. \
                 Look for a game option that locks or centres the window position."
            ),
            None,
        )),
        Verdict::SizeForced | Verdict::StyleAndSizeReverted => {}
    }

    if forced.is_some() {
        if warscape {
            // Virtual pixels: Windows scales a DPI-unaware game by the monitor's dpi/96.
            let scale_by = if unaware { dpi } else { 96 };
            let scale = |v: i32| (f64::from(v) * 96.0 / f64::from(scale_by)).round() as i32;
            let (fx, fy) = (scale(w), scale(h));
            let how = if unaware {
                let pct = (f64::from(dpi) * 100.0 / 96.0).round();
                format!(
                    "the game is DPI-unaware: use the island size ({w}x{h}) divided by dpi/96 \
                     (this monitor is at {pct}%, dpi {dpi}: {fx}x{fy})"
                )
            } else {
                format!("use the island size ({w}x{h})")
            };
            out.push(rec(
                "warscape_resolution",
                format!(
                    "Warscape engine: with the game closed and after backing up \
                     preferences.script.txt, set fix_res false, fix_window_pos false, x_res {fx} \
                     and y_res {fy} ({how})."
                ),
                Some(json!({
                    "x_res": fx, "y_res": fy, "island_width": w, "island_height": h,
                    "dpi_unaware": unaware, "dpi": dpi
                })),
            ));
        } else {
            out.push(rec(
                "unity_args",
                format!(
                    "Unity engine: launch the game with -popupwindow -screen-width {w} \
                     -screen-height {h} (the island size; Unity asks for a supported resolution, \
                     if it does not honour it try another)."
                ),
                Some(json!({ "width": w, "height": h })),
            ));
        }
    }
    if let Some(f) = forced {
        out.push(rec(
            "game_resolution",
            format!(
                "If you cannot change the game, add game_resolution \
                 {{\"width\": {}, \"height\": {}}} to its profile: the window will be centred in \
                 the island at that size instead of fighting.",
                f.width, f.height
            ),
            Some(json!({ "width": f.width, "height": f.height })),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u32 = 0x14CF_0000;
    const BORDERLESS: u32 = 0x1400_0000;
    const POPUP: u32 = 0x9400_0000;

    fn rect(l: i32, t: i32, r: i32, b: i32) -> Rect {
        Rect {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    fn target() -> Rect {
        rect(2560, 0, 5120, 1440)
    }

    fn original() -> Rect {
        rect(231, 223, 1516, 971)
    }

    /// One sample per millisecond over `[0, end_ms)` from a state function.
    fn timeline(end_ms: u32, f: impl Fn(f64) -> (u32, Rect)) -> Vec<Sample> {
        (0..end_ms)
            .map(|i| {
                let t = f64::from(i);
                let (style, rect) = f(t);
                Sample {
                    t_ms: t,
                    style,
                    exstyle: 0,
                    rect,
                }
            })
            .collect()
    }

    fn mark(t: f64, op: &str) -> WriteMark {
        WriteMark {
            t_ms: t,
            op: op.into(),
        }
    }

    fn writes() -> Vec<WriteMark> {
        vec![mark(0.4, "set_styles"), mark(0.8, "set_pos")]
    }

    fn codes(a: &Analysis) -> Vec<&str> {
        a.recommendations.iter().map(|r| r.code.as_str()).collect()
    }

    #[test]
    fn notepad_accepts() {
        let s = timeline(3000, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else {
                (BORDERLESS, target())
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "notepad.exe");
        assert_eq!(a.verdict, Verdict::Accepted);
        assert_eq!(a.forced_size, None);
        assert_eq!(a.revert_latency_ms, Latencies::default());
        assert!(a.recommendations.is_empty());
    }

    #[test]
    fn warscape_reverts_style_and_size() {
        let s = timeline(200, |t| match t {
            t if t < 1.0 => (FRAME, original()),
            t if t < 3.0 => (BORDERLESS, target()),
            t if t < 9.0 => (BORDERLESS, rect(0, 0, 2420, 1399)),
            _ => (FRAME, rect(0, 0, 2420, 1399)),
        });
        let a = analyse(target(), &writes(), &s, "unaware", 120, "Warhammer3.exe");
        assert_eq!(a.verdict, Verdict::StyleAndSizeReverted);
        let f = a.forced_size.as_ref().unwrap();
        assert_eq!((f.width, f.height), (2420, 1399));
        assert!((a.revert_latency_ms.style.unwrap() - 8.6).abs() < 1e-9);
        assert!((a.revert_latency_ms.rect.unwrap() - 2.2).abs() < 1e-9);
        assert_eq!(codes(&a), ["warscape_resolution", "game_resolution"]);
        // 2560x1440 at 125 % -> 2048x1152 virtual pixels.
        let text = &a.recommendations[0].text;
        assert!(
            text.contains("x_res 2048") && text.contains("y_res 1152"),
            "{text}"
        );
        assert!(text.contains("fix_res false"));
    }

    #[test]
    fn unity_forces_size_keeps_style() {
        let s = timeline(300, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 63.0 {
                (BORDERLESS, target())
            } else {
                (BORDERLESS, rect(3200, 360, 4480, 1080))
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "PWAAT.exe");
        assert_eq!(a.verdict, Verdict::SizeForced);
        let f = a.forced_size.as_ref().unwrap();
        assert_eq!((f.width, f.height), (1280, 720));
        assert_eq!(a.revert_latency_ms.style, None);
        assert!((a.revert_latency_ms.rect.unwrap() - 62.2).abs() < 1e-9);
        assert_eq!(codes(&a), ["unity_args", "game_resolution"]);
        assert!(
            a.recommendations[0]
                .text
                .contains("-popupwindow -screen-width 2560 -screen-height 1440")
        );
        assert!(a.recommendations[1].text.contains("\"width\": 1280"));
    }

    #[test]
    fn unity_popup_without_style_write_forces_size_fast() {
        // Already frameless (0x94000000): we only write the rect, and Unity reverts it at ~21 ms.
        let s = timeline(300, |t| {
            if t < 1.0 {
                (POPUP, original())
            } else if t < 22.0 {
                (POPUP, target())
            } else {
                (POPUP, rect(2560, 0, 3840, 720))
            }
        });
        let w = vec![mark(0.4, "set_pos")];
        let a = analyse(target(), &w, &s, "per_monitor_v2", 96, "PWAAT.exe");
        assert_eq!(a.verdict, Verdict::SizeForced);
        assert_eq!(a.revert_latency_ms.style, None);
        assert!((a.revert_latency_ms.rect.unwrap() - 21.6).abs() < 1e-9);
        assert_eq!(codes(&a), ["unity_args", "game_resolution"]);
    }

    #[test]
    fn late_frame_is_a_late_revert() {
        let s = timeline(2000, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 1500.0 {
                (BORDERLESS, target())
            } else {
                (FRAME, target())
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "testwin.exe");
        assert_eq!(a.verdict, Verdict::LateRevert);
        assert_eq!(a.forced_size, None);
        assert!((a.revert_latency_ms.style.unwrap() - 1499.6).abs() < 1e-9);
        assert_eq!(codes(&a), ["style_reverted"]);
    }

    #[test]
    fn position_only_deviation() {
        let s = timeline(100, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 30.0 {
                (BORDERLESS, target())
            } else {
                (BORDERLESS, rect(2000, 0, 4560, 1440))
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "x.exe");
        assert_eq!(a.verdict, Verdict::PositionForced);
        assert_eq!(a.forced_size, None);
        assert_eq!(codes(&a), ["position_forced"]);
    }

    #[test]
    fn style_only_revert_is_not_late_when_fast() {
        let s = timeline(100, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 15.0 {
                (BORDERLESS, target())
            } else {
                (FRAME, target())
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "x.exe");
        assert_eq!(a.verdict, Verdict::StyleReverted);
    }

    fn data_of<'a>(a: &'a Analysis, code: &str) -> Option<&'a serde_json::Value> {
        a.recommendations
            .iter()
            .find(|r| r.code == code)
            .and_then(|r| r.data.as_ref())
    }

    fn size_forced_timeline() -> Vec<Sample> {
        timeline(200, |t| match t {
            t if t < 1.0 => (FRAME, original()),
            t if t < 3.0 => (BORDERLESS, target()),
            t if t < 9.0 => (BORDERLESS, rect(0, 0, 2420, 1399)),
            _ => (FRAME, rect(0, 0, 2420, 1399)),
        })
    }

    fn warscape_data(awareness: &str, dpi: u32) -> Option<serde_json::Value> {
        let a = analyse(
            target(),
            &writes(),
            &size_forced_timeline(),
            awareness,
            dpi,
            "Warhammer3.exe",
        );
        data_of(&a, "warscape_resolution").cloned()
    }

    #[test]
    fn warscape_data_for_dpi_unaware_game_uses_the_measured_dpi() {
        for (dpi, x_res, y_res) in [(96, 2560, 1440), (120, 2048, 1152), (144, 1707, 960)] {
            assert_eq!(
                warscape_data("unaware", dpi),
                Some(serde_json::json!({
                    "x_res": x_res, "y_res": y_res, "island_width": 2560, "island_height": 1440,
                    "dpi_unaware": true, "dpi": dpi
                })),
                "dpi {dpi}"
            );
        }
    }

    #[test]
    fn warscape_data_for_dpi_aware_game_is_the_island_size() {
        for dpi in [96, 120, 144] {
            assert_eq!(
                warscape_data("per_monitor_v2", dpi),
                Some(serde_json::json!({
                    "x_res": 2560, "y_res": 1440, "island_width": 2560, "island_height": 1440,
                    "dpi_unaware": false, "dpi": dpi
                })),
                "dpi {dpi}"
            );
        }
    }

    #[test]
    fn warscape_text_names_the_measured_scale() {
        let a = analyse(
            target(),
            &writes(),
            &size_forced_timeline(),
            "unaware",
            144,
            "Warhammer3.exe",
        );
        let text = &a.recommendations[0].text;
        assert!(text.contains("at 150%, dpi 144: 1707x960"), "{text}");
        assert!(!text.contains("125"), "{text}");
    }

    #[test]
    fn unity_args_data_is_the_island_size() {
        let s = timeline(300, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 63.0 {
                (BORDERLESS, target())
            } else {
                (BORDERLESS, rect(3200, 360, 4480, 1080))
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "PWAAT.exe");
        assert_eq!(
            data_of(&a, "unity_args"),
            Some(&serde_json::json!({ "width": 2560, "height": 1440 }))
        );
    }

    #[test]
    fn game_resolution_data_is_the_forced_size() {
        let a = analyse(
            target(),
            &writes(),
            &size_forced_timeline(),
            "unaware",
            96,
            "Warhammer3.exe",
        );
        assert_eq!(
            data_of(&a, "game_resolution"),
            Some(&serde_json::json!({ "width": 2420, "height": 1399 }))
        );
    }

    #[test]
    fn style_reverted_data_is_the_reassert_limit() {
        let s = timeline(100, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 15.0 {
                (BORDERLESS, target())
            } else {
                (FRAME, target())
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "x.exe");
        assert_eq!(
            data_of(&a, "style_reverted"),
            Some(&serde_json::json!({
                "max_reasserts": crate::guard::GuardConfig::default().max_reasserts
            }))
        );
    }

    #[test]
    fn position_forced_has_no_numbers() {
        let s = timeline(100, |t| {
            if t < 1.0 {
                (FRAME, original())
            } else if t < 30.0 {
                (BORDERLESS, target())
            } else {
                (BORDERLESS, rect(2000, 0, 4560, 1440))
            }
        });
        let a = analyse(target(), &writes(), &s, "per_monitor_v2", 96, "x.exe");
        assert_eq!(codes(&a), ["position_forced"]);
        assert_eq!(a.recommendations[0].data, None);
    }

    #[test]
    fn recommendation_data_is_optional_on_the_wire() {
        let r: Recommendation = serde_json::from_str(r#"{"code":"c","text":"t"}"#).unwrap();
        assert_eq!(r.data, None);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"code":"c","text":"t"}"#
        );
    }

    #[test]
    fn verdict_serialises_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&Verdict::StyleAndSizeReverted).unwrap(),
            "\"style_and_size_reverted\""
        );
        assert_eq!(
            serde_json::to_string(&Verdict::LateRevert).unwrap(),
            "\"late_revert\""
        );
    }
}
