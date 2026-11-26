//! Session probe for CI: can this desktop session run the window suites?
//!
//! Launches `testwin.exe` (built next to this exe by `cargo build --examples`) and measures it as a
//! foreign window, the way the product does: WinEvent SHOW/FOREGROUND hooks, visibility,
//! foreground, a style roundtrip and a rect roundtrip. Prints one JSON line on stdout and exits 0
//! when the session is usable, 1 when it is not. It opens a window: CI only.
//!
//! Usage: session_probe

use std::cell::{Cell, RefCell};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EnumWindows,
    GWL_STYLE, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, MSG, OBJID_WINDOW, PM_REMOVE, PeekMessageW,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, TranslateMessage, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WS_CAPTION,
    WS_THICKFRAME,
};
use windows::core::BOOL;

const TITLE: &str = "Shatter Session Probe";
/// Window class registered by `examples/testwin.rs`.
const CLASS: &str = "ShatterTestWin";
const SHOW_TIMEOUT: Duration = Duration::from_secs(10);
const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// Outer rect requested by the rect roundtrip: 640x360 at 100,100.
const WANT: RECT = RECT {
    left: 100,
    top: 100,
    right: 740,
    bottom: 460,
};

thread_local! {
    /// Process id of the testwin child; 0 until it is spawned.
    static CHILD_PID: Cell<u32> = const { Cell::new(0) };
    /// Windows of the child seen by EVENT_OBJECT_SHOW / EVENT_SYSTEM_FOREGROUND.
    static SHOWN: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    static FOREGROUND: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default)]
struct Probe {
    window: bool,
    visible: bool,
    show_event: bool,
    style_roundtrip: bool,
    rect_roundtrip: bool,
    set_foreground: bool,
    is_foreground: bool,
    foreground_event: bool,
    error: Option<String>,
}

impl Probe {
    /// Foreground is only recorded: the suites do not depend on it.
    fn usable(&self) -> bool {
        self.window
            && self.visible
            && self.show_event
            && self.style_roundtrip
            && self.rect_roundtrip
    }

    /// Keeps the first failure: later ones are usually its consequence.
    fn fail(&mut self, error: String) {
        self.error.get_or_insert(error);
    }

    fn line(&self) -> String {
        serde_json::json!({
            "window": self.window,
            "visible": self.visible,
            "show_event": self.show_event,
            "style_roundtrip": self.style_roundtrip,
            "rect_roundtrip": self.rect_roundtrip,
            "set_foreground": self.set_foreground,
            "is_foreground": self.is_foreground,
            "foreground_event": self.foreground_event,
            "usable": self.usable(),
            "error": self.error,
        })
        .to_string()
    }
}

extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.0.is_null() {
        return;
    }
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer; GetWindowThreadProcessId accepts any handle value.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 || pid != CHILD_PID.get() {
        return;
    }
    let seen = if event == EVENT_OBJECT_SHOW {
        &SHOWN
    } else {
        &FOREGROUND
    };
    seen.with_borrow_mut(|s| s.push(hwnd.0 as isize));
}

struct Hooks(Vec<HWINEVENTHOOK>);

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            // SAFETY: `h` was returned by SetWinEventHook and is unhooked exactly once.
            let _ = unsafe { UnhookWinEvent(h) };
        }
    }
}

/// Kills the testwin child however the probe ends.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let deadline = Instant::now() + KILL_TIMEOUT;
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            sleep(Duration::from_millis(10));
        }
    }
}

/// Dispatches every queued message; out-of-context WinEvent callbacks run in here.
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid buffer for each call; messages belong to this thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Pumps messages until `done` holds or `timeout` passes; returns `done`'s last answer.
fn pump_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        pump();
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(10));
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` handed to EnumWindows by `testwin_window`.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    BOOL(1)
}

/// The top-level testwin window of process `pid`, skipping helper windows such as the IME's.
fn testwin_window(pid: u32) -> Option<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the pointer outlives the synchronous enumeration and matches the callback's cast.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize)) };
    all.into_iter().find(|&h| {
        let mut owner = 0u32;
        let mut class = [0u16; 64];
        // SAFETY: `owner` and `class` are valid out buffers; both calls accept any handle value.
        let n = unsafe {
            GetWindowThreadProcessId(h, Some(&mut owner));
            GetClassNameW(h, &mut class)
        };
        let n = usize::try_from(n).unwrap_or(0).min(class.len());
        owner == pid && String::from_utf16_lossy(&class[..n]) == CLASS
    })
}

fn style_of(h: HWND) -> u32 {
    // SAFETY: GetWindowLongPtrW accepts any handle value; styles live in the low 32 bits.
    unsafe { GetWindowLongPtrW(h, GWL_STYLE) as u32 }
}

/// Runs the steps; `Err` stops the probe early, `p.fail` records a failed check and goes on.
fn measure(p: &mut Probe) -> Result<(), String> {
    // SAFETY: process-wide setting made before this process creates or measures any window.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        p.fail(format!("SetProcessDpiAwarenessContext failed: {e}"));
    }

    // Same events and flags as src/watcher.rs, so the probe sees what the product sees.
    let mut hooks = Hooks(Vec::with_capacity(2));
    for event in [EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND] {
        // SAFETY: `on_event` has the WINEVENTPROC signature and lives for the whole process;
        // no module handle is needed for WINEVENT_OUTOFCONTEXT.
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(on_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.0.is_null() {
            return Err(format!(
                "SetWinEventHook failed: {}",
                windows::core::Error::from_thread()
            ));
        }
        hooks.0.push(hook);
    }

    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let testwin = exe.with_file_name("testwin.exe");
    if !testwin.is_file() {
        return Err(format!(
            "{} not found: run `cargo build --examples` first",
            testwin.display()
        ));
    }
    // Declared after `hooks`, so the child is killed before the hooks are removed.
    let child = KillOnDrop(
        Command::new(&testwin)
            .args(["--mode", "normal", "--title", TITLE])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawning {} failed: {e}", testwin.display()))?,
    );
    let pid = child.0.id();
    CHILD_PID.set(pid);

    let mut window = None;
    p.show_event = pump_until(SHOW_TIMEOUT, || {
        window = window.or_else(|| testwin_window(pid));
        window.is_some_and(|h| SHOWN.with_borrow(|s| s.contains(&(h.0 as isize))))
    });
    let Some(h) = window else {
        return Err(format!(
            "no {CLASS} window of pid {pid} within {}s",
            SHOW_TIMEOUT.as_secs()
        ));
    };
    p.window = true;
    if !p.show_event {
        p.fail(format!(
            "no EVENT_OBJECT_SHOW for the testwin window within {}s",
            SHOW_TIMEOUT.as_secs()
        ));
    }
    // SAFETY: IsWindowVisible accepts any handle value.
    p.visible = unsafe { IsWindowVisible(h) }.as_bool();
    if !p.visible {
        p.fail("the testwin window is not visible".into());
    }

    // SAFETY: SetForegroundWindow accepts any handle value; failure is only recorded.
    p.set_foreground = unsafe { SetForegroundWindow(h) }.as_bool();
    // A FOREGROUND event since launch counts: if testwin was activated when it showed,
    // SetForegroundWindow changes nothing and raises no new event.
    p.foreground_event = pump_until(FOREGROUND_TIMEOUT, || {
        FOREGROUND.with_borrow(|f| f.contains(&(h.0 as isize)))
    });
    // SAFETY: plain query.
    p.is_foreground = unsafe { GetForegroundWindow() } == h;

    let frame = (WS_CAPTION | WS_THICKFRAME).0;
    let style = style_of(h);
    // SAFETY: GWL_STYLE of a live window; a failed write shows up in the readback below.
    unsafe { SetWindowLongPtrW(h, GWL_STYLE, (style & !frame) as isize) };
    // SAFETY: plain FFI call with by-value arguments.
    if let Err(e) = unsafe {
        SetWindowPos(
            h,
            None,
            WANT.left,
            WANT.top,
            WANT.right - WANT.left,
            WANT.bottom - WANT.top,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    } {
        p.fail(format!("SetWindowPos failed: {e}"));
    }
    let after = style_of(h);
    p.style_roundtrip = after & frame == 0;
    if !p.style_roundtrip {
        p.fail(format!(
            "style 0x{after:08X} still has frame bits 0x{frame:08X} (was 0x{style:08X})"
        ));
    }
    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid out pointer for the duration of the call.
    match unsafe { GetWindowRect(h, &mut rect) } {
        Ok(()) => {
            p.rect_roundtrip = rect == WANT;
            if !p.rect_roundtrip {
                p.fail(format!(
                    "window rect {rect:?} differs from requested {WANT:?}"
                ));
            }
        }
        Err(e) => p.fail(format!("GetWindowRect failed: {e}")),
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut p = Probe::default();
    if let Err(e) = measure(&mut p) {
        p.fail(e);
    }
    println!("{}", p.line());
    if p.usable() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::Probe;
    use serde_json::Value;

    fn all_true() -> Probe {
        Probe {
            window: true,
            visible: true,
            show_event: true,
            style_roundtrip: true,
            rect_roundtrip: true,
            ..Probe::default()
        }
    }

    #[test]
    fn line_has_exactly_the_contract_keys() {
        let v: Value = serde_json::from_str(&Probe::default().line()).expect("json");
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "error",
                "foreground_event",
                "is_foreground",
                "rect_roundtrip",
                "set_foreground",
                "show_event",
                "style_roundtrip",
                "usable",
                "visible",
                "window",
            ]
        );
        assert_eq!(v["error"], Value::Null);
        assert_eq!(v["usable"], Value::Bool(false));
    }

    #[test]
    fn line_is_a_single_line_with_the_error_string() {
        let p = Probe {
            error: Some("no window".into()),
            ..Probe::default()
        };
        let line = p.line();
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["error"], "no window");
    }

    #[test]
    fn usable_needs_the_five_window_checks_but_not_foreground() {
        let p = all_true();
        assert!(p.usable());
        let v: Value = serde_json::from_str(&p.line()).expect("json");
        assert_eq!(v["usable"], Value::Bool(true));
        assert_eq!(v["window"], Value::Bool(true));
        assert_eq!(v["set_foreground"], Value::Bool(false));
        for clear in [
            |p: &mut Probe| p.window = false,
            |p: &mut Probe| p.visible = false,
            |p: &mut Probe| p.show_event = false,
            |p: &mut Probe| p.style_roundtrip = false,
            |p: &mut Probe| p.rect_roundtrip = false,
        ] {
            let mut p = all_true();
            clear(&mut p);
            assert!(!p.usable());
        }
    }
}
