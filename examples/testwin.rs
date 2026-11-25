//! Fake "game" window used to reproduce, locally, what real games do against a borderless tool.
//!
//! Behaviours are composable flags; `--preset` / `--mode` expand into flags and any flag given after
//! them overrides. Timed reactions are asynchronous (one `SetTimer` per reaction kind, a later
//! change re-arms it); a delay of 0 ms means "synchronously, inside the caller's call".
//!
//! Usage: testwin [--preset warscape|unity|unity-popup|notepad]
//!                [--mode normal|fixed|revert-style|force-size|warscape|late-frame]
//!                [--style frame|fixed|popup] [--revert-style MS] [--force-size MS]
//!                [--reposition keep|center-on-request|offset:X,Y|game-unity] [--late-frame MS]
//!                [--dpi aware|unaware] [--client WxH] [--pos X,Y] [--delay MS]
//!                [--title TEXT] [--log PATH]
//!
//! `--client` is in the window's own DPI space: with `--dpi unaware` Windows virtualizes and
//! bitmap-stretches the window (client 1920x1080 becomes 2400x1350 physical at 125 %).
//! `--delay` is only the delay of `--mode late-frame` (default 1500).
//! Limit: `SetTimer` clamps to USER_TIMER_MINIMUM (10 ms) and ticks at the system timer resolution
//! (~15.6 ms), so reactions of 1-15 ms fire late; the log shows the real wait of each reaction.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::sync::LazyLock;
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, HGDIOBJ,
    InvalidateRect, PAINTSTRUCT, SetBkColor, SetTextColor, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_UNAWARE, GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

#[derive(Clone, Copy, Debug)]
enum Style {
    /// WS_OVERLAPPEDWINDOW.
    Frame,
    /// OVERLAPPED | CAPTION | SYSMENU | MINIMIZEBOX | CLIPSIBLINGS (0x14CA0000 with VISIBLE).
    Fixed,
    /// WS_POPUP | CLIPSIBLINGS (0x94000000 with VISIBLE); ex style stays 0.
    Popup,
}

#[derive(Clone, Copy, Debug)]
enum Reposition {
    /// Keep the position the caller set.
    Keep,
    /// Centre the forced size on the rect the caller requested.
    CenterOnRequest,
    /// Requested left/top plus a fixed offset.
    Offset(i32, i32),
    /// Approximation of the measured PWAAT rule x_final = x0 + (x_req - x_ref) / 2 (y = requested).
    /// The anchor (x_ref, x0) is this window's position at its first reaction, so that one is kept
    /// and later requests move it at half speed; the real game's x0/x_ref are unknown.
    GameUnity,
}

struct Cfg {
    name: String,
    style: Style,
    revert_style: Option<u32>,
    force_size: Option<u32>,
    reposition: Reposition,
    late_frame: Option<u32>,
    dpi_aware: bool,
    client: (i32, i32),
    pos: (i32, i32),
    delay: u32,
    title: Option<String>,
    log: Option<String>,
    start: Instant,
}

static CFG: LazyLock<Cfg> = LazyLock::new(parse_args);

fn cfg() -> &'static Cfg {
    &CFG
}

fn num<T: std::str::FromStr>(s: &str) -> T {
    s.trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad number `{s}`"))
}

fn pair(s: &str, sep: char) -> (i32, i32) {
    let (a, b) = s
        .split_once(sep)
        .unwrap_or_else(|| panic!("expected A{sep}B, got `{s}`"));
    (num(a), num(b))
}

/// Flags of a preset or legacy `--mode` name.
fn expand(name: &str, delay: u32) -> String {
    let flags = match name {
        "warscape" => {
            "--style fixed --revert-style 8 --force-size 2 --reposition keep --dpi unaware --client 1920x1080"
        }
        "unity" => {
            "--style fixed --force-size 60 --reposition game-unity --dpi aware --client 1280x720"
        }
        "unity-popup" => {
            "--style popup --force-size 10 --reposition game-unity --dpi aware --client 1280x720"
        }
        "notepad" => "--style frame --dpi aware",
        "normal" => "--style frame",
        "fixed" => "--style fixed",
        "revert-style" => "--style fixed --revert-style 0",
        "force-size" => "--style fixed --force-size 0 --reposition keep",
        "late-frame" => return format!("--style frame --late-frame {delay}"),
        other => panic!("unknown preset/mode {other}"),
    };
    flags.into()
}

fn apply(c: &mut Cfg, flag: &str, val: &str) {
    match flag {
        "--style" => {
            c.style = match val {
                "frame" => Style::Frame,
                "fixed" => Style::Fixed,
                "popup" => Style::Popup,
                other => panic!("unknown style {other}"),
            }
        }
        "--revert-style" => c.revert_style = Some(num(val)),
        "--force-size" => c.force_size = Some(num(val)),
        "--late-frame" => c.late_frame = Some(num(val)),
        "--reposition" => {
            c.reposition = match (val, val.strip_prefix("offset:")) {
                ("keep", _) => Reposition::Keep,
                ("center-on-request", _) => Reposition::CenterOnRequest,
                ("game-unity", _) => Reposition::GameUnity,
                (_, Some(xy)) => {
                    let (x, y) = pair(xy, ',');
                    Reposition::Offset(x, y)
                }
                _ => panic!("unknown reposition {val}"),
            }
        }
        "--dpi" => {
            c.dpi_aware = match val {
                "aware" => true,
                "unaware" => false,
                other => panic!("unknown dpi mode {other}"),
            }
        }
        "--client" => c.client = pair(val, 'x'),
        "--pos" => c.pos = pair(val, ','),
        "--title" => c.title = Some(val.into()),
        "--log" => c.log = Some(val.into()),
        "--delay" => {} // read up front for `--mode late-frame`
        other => panic!("unknown argument {other}"),
    }
}

fn parse_args() -> Cfg {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut c = Cfg {
        name: "custom".into(),
        style: Style::Frame,
        revert_style: None,
        force_size: None,
        reposition: Reposition::Keep,
        late_frame: None,
        dpi_aware: true,
        client: (1280, 720),
        pos: (100, 100),
        delay: 1500,
        title: None,
        log: None,
        start: Instant::now(),
    };
    if let Some(i) = args.iter().position(|a| a == "--delay") {
        c.delay = num(args.get(i + 1).expect("missing value for --delay"));
    }
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let val = it
            .next()
            .unwrap_or_else(|| panic!("missing value for {flag}"));
        if flag == "--preset" || flag == "--mode" {
            c.name = val.clone();
            let flags = expand(val, c.delay);
            let mut words = flags.split_whitespace();
            while let (Some(f), Some(v)) = (words.next(), words.next()) {
                apply(&mut c, f, v);
            }
        } else {
            apply(&mut c, flag, val);
        }
    }
    c
}

fn ms(v: Option<u32>) -> String {
    v.map_or("off".into(), |v| format!("{v} ms"))
}

/// Active flags, one string per on-screen line.
fn describe(c: &Cfg) -> [String; 2] {
    [
        format!(
            "style={:?} revert-style={} force-size={}",
            c.style,
            ms(c.revert_style),
            ms(c.force_size)
        ),
        format!(
            "reposition={:?} late-frame={} dpi={} client={}x{}",
            c.reposition,
            ms(c.late_frame),
            if c.dpi_aware { "aware" } else { "unaware" },
            c.client.0,
            c.client.1
        ),
    ]
}

// Reaction kinds; the value is also the SetTimer id.
const T_REVERT: usize = 1;
const T_FORCE: usize = 2;
const T_LATE: usize = 3;

#[derive(Default)]
struct State {
    /// Re-entrancy guard: set while this window changes itself.
    guard: Cell<bool>,
    ready: Cell<bool>,
    saved_style: Cell<u32>,
    saved_ex: Cell<u32>,
    /// When each reaction kind was (re-)armed; index = kind - 1.
    armed: [Cell<Option<Instant>>; 3],
    /// Window rect right after the caller's last size/position change.
    requested: Cell<RECT>,
    /// `Reposition::GameUnity` anchor (x_ref, x0).
    anchor: Cell<Option<(i32, i32)>>,
    reverts: Cell<u32>,
    forced: Cell<u32>,
    rebuilds: Cell<u32>,
    last: RefCell<String>,
}

thread_local! { static ST: State = State::default(); }

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

/// Log a reaction and keep it for the on-screen display.
fn reaction(msg: String) {
    log(&msg);
    ST.with(|s| *s.last.borrow_mut() = msg);
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

fn rect_str(r: &RECT) -> String {
    format!("[{},{},{},{}]", r.left, r.top, r.right, r.bottom)
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: hwnd is live and `r` is a valid out pointer.
    unsafe { GetClientRect(hwnd, &mut r).ok() };
    (r.right - r.left, r.bottom - r.top)
}

/// Window size needed for a `client` area under the given style at `dpi`.
fn outer_size(client: (i32, i32), style: u32, ex: u32, dpi: u32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: client.0,
        bottom: client.1,
    };
    // SAFETY: `r` is a valid in/out pointer.
    unsafe {
        AdjustWindowRectExForDpi(&mut r, WINDOW_STYLE(style), false, WINDOW_EX_STYLE(ex), dpi).ok()
    };
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

fn frame_changed(hwnd: HWND) {
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    // SAFETY: hwnd is live.
    unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, flags).ok() };
}

/// Arm reaction `kind` to fire in `delay` ms (re-arms a pending one); 0 runs it right now.
fn react(hwnd: HWND, kind: usize, delay: u32) {
    ST.with(|s| s.armed[kind - 1].set(Some(Instant::now())));
    log(&format!("armed reaction {kind} in {delay} ms"));
    if delay == 0 {
        fire(hwnd, kind);
    } else {
        // SAFETY: hwnd is live; WM_TIMER is delivered to the window procedure.
        unsafe { SetTimer(Some(hwnd), kind, delay, None) };
    }
}

fn fire(hwnd: HWND, kind: usize) {
    // SAFETY: hwnd is live; killing an unarmed timer just fails.
    unsafe { _ = KillTimer(Some(hwnd), kind) };
    let waited = ST
        .with(|s| s.armed[kind - 1].take())
        .map_or(0, |t| t.elapsed().as_millis());
    match kind {
        T_REVERT => revert_style(hwnd, waited),
        T_FORCE => force_size(hwnd, waited),
        _ => late_frame(hwnd, waited),
    }
    invalidate(hwnd);
}

/// The style/ex style this window wants; minimize/maximize are state, not style, so they follow.
fn wanted(hwnd: HWND) -> (u32, u32) {
    let live = (WS_MINIMIZE | WS_MAXIMIZE).0;
    let (st, _) = style_of(hwnd);
    let (saved, ex) = ST.with(|s| (s.saved_style.get(), s.saved_ex.get()));
    ((saved & !live) | (st & live), ex)
}

fn revert_style(hwnd: HWND, waited: u128) {
    let (st, ex) = style_of(hwnd);
    let (want_st, want_ex) = wanted(hwnd);
    if (st, ex) == (want_st, want_ex) {
        return;
    }
    guarded(|| {
        // SAFETY: hwnd is live.
        unsafe {
            if st != want_st {
                SetWindowLongPtrW(hwnd, GWL_STYLE, want_st as isize);
            }
            if ex != want_ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want_ex as isize);
            }
        }
        frame_changed(hwnd);
    });
    ST.with(|s| s.reverts.set(s.reverts.get() + 1));
    reaction(format!(
        "revert-style after {waited} ms: 0x{st:08X}/0x{ex:08X} -> 0x{want_st:08X}/0x{want_ex:08X}"
    ));
}

fn late_frame(hwnd: HWND, waited: u128) {
    let (style, _) = style_of(hwnd);
    let new = style | (WS_CAPTION | WS_THICKFRAME).0;
    guarded(|| {
        // SAFETY: hwnd is live.
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new as isize) };
        frame_changed(hwnd);
    });
    ST.with(|s| s.rebuilds.set(s.rebuilds.get() + 1));
    reaction(format!(
        "late-frame after {waited} ms: style 0x{style:08X} -> 0x{new:08X}"
    ));
}

fn force_size(hwnd: HWND, waited: u128) {
    let c = cfg();
    // SAFETY: hwnd is live.
    if client_size(hwnd) == c.client || unsafe { IsIconic(hwnd).as_bool() } {
        return;
    }
    let (style, ex) = style_of(hwnd);
    // SAFETY: hwnd is live.
    let (w, h) = outer_size(c.client, style, ex, unsafe { GetDpiForWindow(hwnd) });
    let (req, now) = (ST.with(|s| s.requested.get()), window_rect(hwnd));
    let (x, y) = match c.reposition {
        Reposition::Keep => (now.left, now.top),
        Reposition::CenterOnRequest => (
            (req.left + req.right - w) / 2,
            (req.top + req.bottom - h) / 2,
        ),
        Reposition::Offset(dx, dy) => (req.left + dx, req.top + dy),
        Reposition::GameUnity => {
            let (x_ref, x0) = ST.with(|s| {
                let a = s.anchor.get().unwrap_or((req.left, now.left));
                s.anchor.set(Some(a));
                a
            });
            (x0 + (req.left - x_ref) / 2, req.top)
        }
    };
    guarded(|| {
        // SAFETY: hwnd is live.
        unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE).ok() };
    });
    ST.with(|s| s.forced.set(s.forced.get() + 1));
    let (res, size) = (window_rect(hwnd), client_size(hwnd));
    reaction(format!(
        "force-size after {waited} ms: requested {} -> result {} client={}x{}",
        rect_str(&req),
        rect_str(&res),
        size.0,
        size.1
    ));
}

fn on_style_changed(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    // SAFETY: for WM_STYLECHANGED lparam points to a valid STYLESTRUCT.
    let ss = unsafe { *(lparam.0 as *const STYLESTRUCT) };
    let name = match wparam.0 as i32 {
        x if x == GWL_STYLE.0 => "GWL_STYLE",
        x if x == GWL_EXSTYLE.0 => "GWL_EXSTYLE",
        _ => return,
    };
    log(&format!(
        "WM_STYLECHANGED {name} 0x{:08X} -> 0x{:08X}",
        ss.styleOld, ss.styleNew
    ));
    if ST.with(|s| s.ready.get() && !s.guard.get()) {
        let c = cfg();
        if let Some(d) = c.revert_style
            && style_of(hwnd) != wanted(hwnd)
        {
            react(hwnd, T_REVERT, d);
        }
        if let Some(d) = c.late_frame {
            react(hwnd, T_LATE, d);
        }
    }
    invalidate(hwnd);
}

fn on_pos_changed(hwnd: HWND) {
    let r = window_rect(hwnd);
    let size = client_size(hwnd);
    log(&format!(
        "WM_WINDOWPOSCHANGED rect={} client={}x{}",
        rect_str(&r),
        size.0,
        size.1
    ));
    let c = cfg();
    // SAFETY: hwnd is live.
    let iconic = unsafe { IsIconic(hwnd).as_bool() };
    if let Some(d) = c.force_size
        && ST.with(|s| s.ready.get() && !s.guard.get())
        && !iconic
        && size != c.client
    {
        ST.with(|s| s.requested.set(r));
        react(hwnd, T_FORCE, d);
    }
    invalidate(hwnd);
}

fn on_paint(hwnd: HWND) {
    let c = cfg();
    let (style, ex) = style_of(hwnd);
    let r = window_rect(hwnd);
    let (cw, ch) = client_size(hwnd);
    let (reverts, forced, rebuilds, last) = ST.with(|s| {
        (
            s.reverts.get(),
            s.forced.get(),
            s.rebuilds.get(),
            s.last.borrow().clone(),
        )
    });
    // SAFETY: hwnd is live.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let [flags1, flags2] = describe(c);
    let lines = [
        format!("preset/mode: {}", c.name),
        flags1,
        flags2,
        format!("style: 0x{style:08X}  exstyle: 0x{ex:08X}  window dpi: {dpi}"),
        format!(
            "window rect: {} ({}x{})",
            rect_str(&r),
            r.right - r.left,
            r.bottom - r.top
        ),
        format!("client: {cw}x{ch}"),
        format!("style reverts: {reverts}  forced resizes: {forced}  late frames: {rebuilds}"),
        format!("last: {last}"),
    ];
    let mut ps = PAINTSTRUCT::default();
    let full = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    // SAFETY: standard BeginPaint/EndPaint pairing on a live window; every GDI object created here is deleted.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let (bg, border) = (
            CreateSolidBrush(COLORREF(0x00604030)),
            CreateSolidBrush(COLORREF(0x000000FF)),
        );
        FillRect(hdc, &full, bg);
        FrameRect(hdc, &full, border);
        SetTextColor(hdc, COLORREF(0x00FFFFFF));
        SetBkColor(hdc, COLORREF(0x00604030));
        for (i, line) in lines.iter().enumerate() {
            let text: Vec<u16> = line.encode_utf16().collect();
            _ = TextOutW(hdc, 8, 8 + 18 * i as i32, &text);
        }
        for br in [bg, border] {
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
        WM_TIMER if (T_REVERT..=T_LATE).contains(&wparam.0) => fire(hwnd, wparam.0),
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
    // SAFETY: process-wide setting; must precede the first window of the process.
    unsafe {
        SetProcessDpiAwarenessContext(if c.dpi_aware {
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
        } else {
            DPI_AWARENESS_CONTEXT_UNAWARE
        })
        .expect("SetProcessDpiAwarenessContext failed");
    }
    let style = WS_VISIBLE
        | match c.style {
            Style::Frame => WS_OVERLAPPEDWINDOW,
            Style::Fixed => {
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPSIBLINGS
            }
            Style::Popup => WS_POPUP | WS_CLIPSIBLINGS,
        };
    // SAFETY: plain query.
    let (w, h) = outer_size(c.client, style.0, 0, unsafe { GetDpiForSystem() });
    let title = c
        .title
        .clone()
        .unwrap_or_else(|| format!("Shatter TestWin [{}]", c.name));
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();

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
        let [flags1, flags2] = describe(c);
        log(&format!(
            "created style=0x{st:08X} ex=0x{ex:08X} dpi={} name={} {flags1} {flags2}",
            GetDpiForWindow(hwnd),
            c.name
        ));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
