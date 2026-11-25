//! Fake "game" window used to reproduce game behaviours locally.
//!
//! Usage: testwin [--mode normal|fixed|revert-style|force-size|warscape|late-frame]
//!                [--client WxH] [--pos X,Y] [--delay MS] [--title TEXT] [--log PATH]

use std::cell::Cell;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, HGDIOBJ,
    InvalidateRect, PAINTSTRUCT, SetBkColor, SetTextColor, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Fixed,
    RevertStyle,
    ForceSize,
    Warscape,
    LateFrame,
}

struct Cfg {
    mode: Mode,
    mode_name: String,
    client: (i32, i32),
    pos: (i32, i32),
    delay: u32,
    title: String,
    log: Option<String>,
    start: Instant,
}

static CFG: LazyLock<Cfg> = LazyLock::new(parse_args);

#[derive(Default)]
struct State {
    /// Re-entrancy guard: set while this window changes itself.
    guard: Cell<bool>,
    ready: Cell<bool>,
    saved_style: Cell<u32>,
    saved_ex: Cell<u32>,
    reverts: Cell<u32>,
    forced: Cell<u32>,
    timer_pending: Cell<bool>,
}

thread_local! { static ST: State = State::default(); }

const TIMER_ID: usize = 1;

fn cfg() -> &'static Cfg {
    &CFG
}

fn parse_args() -> Cfg {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode_name = "normal".to_string();
    let (mut client, mut pos, mut delay) = ((1280, 720), (100, 100), 1500u32);
    let (mut title, mut log) = (None, None);
    let pair = |s: &str, sep: char| -> (i32, i32) {
        let (a, b) = s
            .split_once(sep)
            .unwrap_or_else(|| panic!("expected A{sep}B, got `{s}`"));
        (
            a.trim().parse().expect("bad number"),
            b.trim().parse().expect("bad number"),
        )
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let val = it
            .next()
            .unwrap_or_else(|| panic!("missing value for {flag}"))
            .clone();
        match flag.as_str() {
            "--mode" => mode_name = val,
            "--client" => client = pair(&val, 'x'),
            "--pos" => pos = pair(&val, ','),
            "--delay" => delay = val.parse().expect("bad --delay"),
            "--title" => title = Some(val),
            "--log" => log = Some(val),
            other => panic!("unknown argument {other}"),
        }
    }
    let mode = match mode_name.as_str() {
        "normal" => Mode::Normal,
        "fixed" => Mode::Fixed,
        "revert-style" => Mode::RevertStyle,
        "force-size" => Mode::ForceSize,
        "warscape" => Mode::Warscape,
        "late-frame" => Mode::LateFrame,
        other => panic!("unknown mode {other}"),
    };
    let title = title.unwrap_or_else(|| format!("Shatter TestWin [{mode_name}]"));
    Cfg {
        mode,
        mode_name,
        client,
        pos,
        delay,
        title,
        log,
        start: Instant::now(),
    }
}

fn reverts_style(m: Mode) -> bool {
    matches!(m, Mode::RevertStyle | Mode::Warscape)
}

fn forces_size(m: Mode) -> bool {
    matches!(m, Mode::ForceSize | Mode::Warscape)
}

fn log(msg: &str) {
    let c = cfg();
    let Some(path) = &c.log else { return };
    let line = format!("t={} {msg}\n", c.start.elapsed().as_millis());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn style_of(hwnd: HWND) -> (u32, u32) {
    // SAFETY: hwnd is a live window of this thread.
    unsafe {
        (
            GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
        )
    }
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: hwnd is live and `r` is a valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut r).ok() };
    r
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: hwnd is live and `r` is a valid out pointer.
    unsafe { GetClientRect(hwnd, &mut r).ok() };
    (r.right - r.left, r.bottom - r.top)
}

/// Window size needed for a `client` area under the given style.
fn outer_size(client: (i32, i32), style: u32, ex: u32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: client.0,
        bottom: client.1,
    };
    // SAFETY: `r` is a valid in/out pointer.
    unsafe { AdjustWindowRectEx(&mut r, WINDOW_STYLE(style), false, WINDOW_EX_STYLE(ex)).ok() };
    (r.right - r.left, r.bottom - r.top)
}

fn invalidate(hwnd: HWND) {
    // SAFETY: hwnd is live.
    unsafe { _ = InvalidateRect(Some(hwnd), None, false) };
}

/// Run `f` with the re-entrancy guard raised.
fn guarded(f: impl FnOnce()) {
    ST.with(|s| s.guard.set(true));
    f();
    ST.with(|s| s.guard.set(false));
}

fn frame_changed(hwnd: HWND, extra: SET_WINDOW_POS_FLAGS) {
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | extra;
    // SAFETY: hwnd is live.
    unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, flags).ok() };
}

fn on_style_changed(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    // SAFETY: for WM_STYLECHANGED lparam points to a valid STYLESTRUCT.
    let ss = unsafe { *(lparam.0 as *const STYLESTRUCT) };
    let which = wparam.0 as i32;
    let (name, saved) = if which == GWL_STYLE.0 {
        ("GWL_STYLE", ST.with(|s| s.saved_style.get()))
    } else if which == GWL_EXSTYLE.0 {
        ("GWL_EXSTYLE", ST.with(|s| s.saved_ex.get()))
    } else {
        return;
    };
    let (guard, ready) = ST.with(|s| (s.guard.get(), s.ready.get()));
    let mut note = String::new();
    if ready && !guard {
        let c = cfg();
        if reverts_style(c.mode) && ss.styleNew != saved {
            guarded(|| {
                // SAFETY: hwnd is live.
                unsafe { SetWindowLongPtrW(hwnd, WINDOW_LONG_PTR_INDEX(which), saved as isize) };
                frame_changed(hwnd, SET_WINDOW_POS_FLAGS(0));
            });
            ST.with(|s| s.reverts.set(s.reverts.get() + 1));
            note = format!(" -> reverted to 0x{saved:08X}");
        } else if c.mode == Mode::LateFrame && !ST.with(|s| s.timer_pending.replace(true)) {
            // SAFETY: hwnd is live; WM_TIMER is delivered to the window procedure.
            unsafe { SetTimer(Some(hwnd), TIMER_ID, c.delay, None) };
            note = format!(" -> frame rebuild in {} ms", c.delay);
        }
    }
    log(&format!(
        "WM_STYLECHANGED {name} style=0x{:08X}{note}",
        ss.styleNew
    ));
    invalidate(hwnd);
}

fn on_late_frame(hwnd: HWND) {
    // SAFETY: hwnd is live.
    unsafe { _ = KillTimer(Some(hwnd), TIMER_ID) };
    ST.with(|s| s.timer_pending.set(false));
    let (style, _) = style_of(hwnd);
    let new = style | (WS_CAPTION | WS_THICKFRAME).0;
    guarded(|| {
        // SAFETY: hwnd is live.
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new as isize) };
        frame_changed(hwnd, SET_WINDOW_POS_FLAGS(0));
    });
    ST.with(|s| s.reverts.set(s.reverts.get() + 1));
    log(&format!(
        "WM_TIMER late-frame style 0x{style:08X} -> 0x{new:08X}"
    ));
    invalidate(hwnd);
}

fn on_pos_changed(hwnd: HWND) {
    let r = window_rect(hwnd);
    let size = client_size(hwnd);
    log(&format!(
        "WM_WINDOWPOSCHANGED rect=[{},{},{},{}] client={}x{}",
        r.left, r.top, r.right, r.bottom, size.0, size.1
    ));
    let c = cfg();
    // SAFETY: hwnd is live.
    let iconic = unsafe { IsIconic(hwnd).as_bool() };
    if forces_size(c.mode)
        && ST.with(|s| s.ready.get() && !s.guard.get())
        && !iconic
        && size != c.client
    {
        let (style, ex) = style_of(hwnd);
        let (w, h) = outer_size(c.client, style, ex);
        guarded(|| {
            // SAFETY: hwnd is live.
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .ok()
            };
        });
        ST.with(|s| s.forced.set(s.forced.get() + 1));
        log(&format!(
            "forced size back to {w}x{h} (client {}x{})",
            c.client.0, c.client.1
        ));
    }
    invalidate(hwnd);
}

fn on_paint(hwnd: HWND) {
    let (style, ex) = style_of(hwnd);
    let r = window_rect(hwnd);
    let (cw, ch) = client_size(hwnd);
    let (reverts, forced) = ST.with(|s| (s.reverts.get(), s.forced.get()));
    let lines = [
        format!("mode: {}", cfg().mode_name),
        format!("style: 0x{style:08X}  exstyle: 0x{ex:08X}"),
        format!(
            "window rect: [{},{},{},{}] ({}x{})",
            r.left,
            r.top,
            r.right,
            r.bottom,
            r.right - r.left,
            r.bottom - r.top
        ),
        format!("client: {cw}x{ch}"),
        format!("reverts/rebuilds: {reverts}  forced resizes: {forced}"),
    ];
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: standard BeginPaint/EndPaint pairing on a live window; every GDI object created here is deleted.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let cell = 64;
        let (a, b, border) = (
            CreateSolidBrush(COLORREF(0x00604030)),
            CreateSolidBrush(COLORREF(0x00907060)),
            CreateSolidBrush(COLORREF(0x000000FF)),
        );
        for (row, y) in (0..ch).step_by(cell as usize).enumerate() {
            for (col, x) in (0..cw).step_by(cell as usize).enumerate() {
                let rc = RECT {
                    left: x,
                    top: y,
                    right: (x + cell).min(cw),
                    bottom: (y + cell).min(ch),
                };
                FillRect(hdc, &rc, if (row + col) % 2 == 0 { a } else { b });
            }
        }
        FrameRect(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            },
            border,
        );
        SetTextColor(hdc, COLORREF(0x00FFFFFF));
        SetBkColor(hdc, COLORREF(0x00000000));
        for (i, line) in lines.iter().enumerate() {
            let text: Vec<u16> = line.encode_utf16().collect();
            _ = TextOutW(hdc, 8, 8 + 18 * i as i32, &text);
        }
        for br in [a, b, border] {
            _ = DeleteObject(HGDIOBJ(br.0));
        }
        _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_STYLECHANGED => on_style_changed(hwnd, wparam, lparam),
        WM_WINDOWPOSCHANGED => {
            // SAFETY: forwarding the original message; DefWindowProc generates WM_SIZE/WM_MOVE.
            let res = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            on_pos_changed(hwnd);
            return res;
        }
        WM_TIMER if wparam.0 == TIMER_ID => on_late_frame(hwnd),
        WM_PAINT => on_paint(hwnd),
        WM_CLOSE => log("WM_CLOSE"),
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: forwarding the original message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn main() {
    let c = cfg();
    let style = match c.mode {
        Mode::Normal | Mode::LateFrame => WS_OVERLAPPEDWINDOW,
        _ => WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
    } | WS_VISIBLE;
    let (w, h) = outer_size(c.client, style.0, 0);
    let title: Vec<u16> = c.title.encode_utf16().chain(Some(0)).collect();

    // SAFETY: plain Win32 setup on the main thread; pointers outlive the calls that use them.
    unsafe {
        let hinst = GetModuleHandleW(None)
            .expect("GetModuleHandleW failed")
            .into();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).expect("LoadCursorW failed"),
            lpszClassName: w!("ShatterTestWin"),
            ..Default::default()
        };
        assert!(RegisterClassW(&class) != 0, "RegisterClassW failed");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ShatterTestWin"),
            PCWSTR(title.as_ptr()),
            style,
            c.pos.0,
            c.pos.1,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("CreateWindowExW failed");
        let (st, ex) = style_of(hwnd);
        ST.with(|s| {
            s.saved_style.set(st);
            s.saved_ex.set(ex);
            s.ready.set(true);
        });
        log(&format!(
            "created style=0x{st:08X} ex=0x{ex:08X} mode={}",
            c.mode_name
        ));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
