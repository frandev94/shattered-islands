//! Harness of the end-to-end tests (`tests/e2e.rs`): an isolated resident process, fake game
//! windows (`examples/testwin.rs`), an independent Win32 measurement of those windows and a guard
//! that always cleans up.
//!
//! Isolation: every [`Harness`] owns a temp dir (`<target>/e2e-tmp/<test>-<pid>`) used as
//! `APPDATA`/`LOCALAPPDATA`, and a random `BORDERLESS_SHATTER_INSTANCE` suffix that gives the
//! resident its own mutex and pipe. The test process never sets environment variables.
//! Nothing here touches a window that the harness did not create.

#![allow(dead_code)] // each helper is used by a subset of the tests

use std::cell::RefCell;
use std::ffi::c_void;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use borderless_shatter::ipc::pipe::{Client, pipe_name_for};
use borderless_shatter::ipc::{
    ApplyResult, CallError, DiagnoseResult, ErrorBody, HelloResult, ListIslandsResult,
    ListWindowsResult, RestoreResult, WindowInfo,
};
use borderless_shatter::window_state::Rect;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, HWND_MESSAGE, IsWindow, IsWindowVisible,
    PostThreadMessageW, WM_QUIT,
};
use windows::core::{BOOL, PCWSTR};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// A hung test must not leave windows or processes behind: after this the watchdog kills them.
const WATCHDOG: Duration = Duration::from_secs(150);

pub const FRAME_BITS: u32 = 0x00CF_0000;
pub const EX_FRAME_BITS: u32 = 0x020E_0381;

// ------------------------------------------------------------------ measurement

/// Style, ex-style and outer rect of a window, read straight from Win32 (not through the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snap {
    pub style: u32,
    pub exstyle: u32,
    pub rect: Rect,
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

fn from_native(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

pub fn window_exists(hwnd: isize) -> bool {
    // SAFETY: plain query on any handle value.
    unsafe { IsWindow(Some(hwnd_of(hwnd))) }.as_bool()
}

pub fn try_snap(hwnd: isize) -> Option<Snap> {
    let h = hwnd_of(hwnd);
    let mut r = RECT::default();
    // SAFETY: `r` is a valid out pointer; a dead handle just makes the call fail.
    unsafe { GetWindowRect(h, &mut r) }.ok()?;
    // SAFETY: plain queries on the same handle.
    let (style, exstyle) = unsafe {
        (
            GetWindowLongPtrW(h, GWL_STYLE) as u32,
            GetWindowLongPtrW(h, GWL_EXSTYLE) as u32,
        )
    };
    Some(Snap {
        style,
        exstyle,
        rect: from_native(r),
    })
}

pub fn snap(hwnd: isize) -> Snap {
    try_snap(hwnd).unwrap_or_else(|| panic!("window {hwnd:#x} does not exist"))
}

/// Client-area size read with `GetClientRect`.
pub fn client_size(hwnd: isize) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: `r` is a valid out pointer.
    unsafe { GetClientRect(hwnd_of(hwnd), &mut r) }.expect("GetClientRect");
    (r.right - r.left, r.bottom - r.top)
}

/// `rcMonitor` of the primary monitor (the one whose origin is 0,0).
pub fn primary_monitor() -> Rect {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: DEFAULTTOPRIMARY always yields a valid monitor; `info` has cbSize set.
    let ok = unsafe {
        let m = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        GetMonitorInfoW(m, &mut info)
    };
    assert!(ok.as_bool(), "GetMonitorInfoW failed");
    from_native(info.rcMonitor)
}

pub fn size_of_rect(r: Rect) -> (i32, i32) {
    (r.right - r.left, r.bottom - r.top)
}

pub fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16)
        .unwrap_or_else(|e| panic!("bad hex string {s:?}: {e}"))
}

/// Polls `f` every 10 ms; `true` if it held before `timeout`.
pub fn poll(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn wait_until(what: &str, timeout: Duration, f: impl FnMut() -> bool) {
    assert!(poll(timeout, f), "timed out waiting for {what}");
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` handed to EnumWindows by `windows_of`.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    BOOL(1)
}

/// Top-level windows plus message-only windows owned by process `pid`.
fn windows_of(pid: u32) -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the pointer outlives the synchronous enumeration and matches the callback's cast.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize)) };
    let mut prev: Option<HWND> = None;
    // SAFETY: plain query; null class/title match any message-only window.
    while let Ok(h) =
        unsafe { FindWindowExW(Some(HWND_MESSAGE), prev, PCWSTR::null(), PCWSTR::null()) }
    {
        all.push(h);
        prev = Some(h);
    }
    all.into_iter()
        .filter(|&h| {
            let mut owner = 0u32;
            // SAFETY: `owner` is a valid out pointer.
            unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
            owner == pid
        })
        .collect()
}

/// Ids of the threads that own a window of `pid` (the resident's GUI thread is one of them).
fn window_threads(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for h in windows_of(pid) {
        let mut owner = 0u32;
        // SAFETY: `owner` is a valid out pointer.
        let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
        if tid != 0 && !out.contains(&tid) {
            out.push(tid);
        }
    }
    out
}

fn visible_window_of(pid: u32) -> Option<isize> {
    windows_of(pid).into_iter().find_map(|h| {
        // SAFETY: plain query.
        let visible = unsafe { IsWindowVisible(h) }.as_bool();
        visible.then_some(h.0 as isize)
    })
}

/// Posts `WM_QUIT` to each GUI thread of process `pid` in turn until `exited` reports that it
/// has exited; `false` if it is still running after the last thread's `per_thread` wait.
fn quit_process(pid: u32, per_thread: Duration, mut exited: impl FnMut() -> bool) -> bool {
    let mut threads = Vec::new();
    poll(Duration::from_secs(5), || {
        threads = window_threads(pid);
        !threads.is_empty() || exited()
    });
    if exited() {
        return true;
    }
    for tid in threads {
        // SAFETY: plain thread-message post; a wrong thread just rejects or ignores it.
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if poll(per_thread, &mut exited) {
            return true;
        }
    }
    false
}

fn child_exited(child: &mut Child) -> bool {
    child.try_wait().ok().flatten().is_some()
}

/// Stops a process that the harness did not spawn itself (see [`Harness::track_pid`]): `WM_QUIT`
/// first, `TerminateProcess` if it is still running afterwards. Closes `handle`.
fn stop_tracked(pid: u32, handle: HANDLE) {
    // SAFETY: `handle` is an open process handle with SYNCHRONIZE access; 0 ms only polls.
    let exited = || unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
    if !quit_process(pid, Duration::from_secs(2), exited) {
        // SAFETY: same handle, opened with PROCESS_TERMINATE.
        unsafe {
            let _ = TerminateProcess(handle, 1);
            WaitForSingleObject(handle, 5_000);
        }
    }
    // SAFETY: the handle is owned by the harness and closed exactly once, here.
    let _ = unsafe { CloseHandle(handle) };
}

// -------------------------------------------------------------------- IPC calls

pub fn call<T: DeserializeOwned>(c: &mut Client, method: &str, params: Value) -> T {
    match c.call(method, params) {
        Ok(v) => serde_json::from_value(v.clone())
            .unwrap_or_else(|e| panic!("{method}: unexpected result shape ({e}): {v}")),
        Err(CallError::Remote(e)) => {
            panic!("{method} failed: {:?}: {} {:?}", e.code, e.message, e.data)
        }
        Err(CallError::Ipc(e)) => panic!("{method}: transport error: {e}"),
    }
}

/// The `error` of a call that must fail.
pub fn call_err(c: &mut Client, method: &str, params: Value) -> ErrorBody {
    match c.call(method, params) {
        Ok(v) => panic!("{method} unexpectedly succeeded: {v}"),
        Err(CallError::Remote(e)) => e,
        Err(CallError::Ipc(e)) => panic!("{method}: transport error: {e}"),
    }
}

pub fn hello(c: &mut Client) -> HelloResult {
    call(c, "hello", json!({}))
}

pub fn list_islands(c: &mut Client) -> ListIslandsResult {
    call(c, "list_islands", json!({}))
}

/// Resolved rect of `layout/island` according to `list_islands`.
pub fn island_rect(c: &mut Client, layout: &str, island: &str) -> Rect {
    let list = list_islands(c);
    let l = list
        .layouts
        .iter()
        .find(|l| l.id == layout)
        .unwrap_or_else(|| panic!("layout {layout} not listed"));
    l.islands
        .iter()
        .find(|i| i.id == island)
        .unwrap_or_else(|| panic!("island {island} not in {layout}"))
        .rect
}

pub fn windows(c: &mut Client, only_managed: bool) -> Vec<WindowInfo> {
    let r: ListWindowsResult = call(c, "list_windows", json!({ "only_managed": only_managed }));
    r.windows
}

pub fn place(c: &mut Client, hwnd: isize, island: &str) -> ApplyResult {
    call(
        c,
        "place",
        json!({ "window": { "hwnd": hwnd }, "island": island }),
    )
}

pub fn restore(c: &mut Client, hwnd: isize) -> RestoreResult {
    call(c, "restore", json!({ "window": { "hwnd": hwnd } }))
}

pub fn diagnose(
    c: &mut Client,
    hwnd: isize,
    island: &str,
    observe_ms: u64,
    keep: bool,
) -> DiagnoseResult {
    call(
        c,
        "diagnose",
        json!({
            "window": { "hwnd": hwnd },
            "island": island,
            "observe_ms": observe_ms,
            "keep": keep,
        }),
    )
}

// ---------------------------------------------------------------------- harness

/// A fake game window started by the harness; `orig` is its state before the app touched it.
pub struct TestWin {
    pub hwnd: isize,
    pub pid: u32,
    pub log: PathBuf,
    pub orig: Snap,
}

impl TestWin {
    /// Lines logged by the fake game itself (its own account of what happened to it).
    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

pub struct Harness {
    dir: PathBuf,
    suffix: String,
    resident: Option<Child>,
    testwins: Vec<Child>,
    starts: u32,
    done: Arc<AtomicBool>,
    pids: Arc<Mutex<Vec<u32>>>,
    /// Processes registered with [`Harness::track_pid`], with a handle that pins their pid.
    tracked: RefCell<Vec<(u32, HANDLE)>>,
}

/// `<target>/<profile>`: where Cargo puts the main binary (and `examples/`). The test exe itself
/// may live elsewhere (cargo's build-dir layout), so it is not used to find them.
fn profile_dir() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_borderless-shatter"))
        .parent()
        .expect("the main binary lives in <target>/<profile>")
}

impl Harness {
    /// Per-test setup. Also makes the calling thread per-monitor-DPI-aware so that every rect it
    /// reads is physical, like the app's.
    pub fn new(test: &str) -> Self {
        // SAFETY: thread-scoped setting without pointers; the previous context is not needed.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let suffix = format!("e2e-{}-{nanos:08x}", std::process::id());
        let tmp = profile_dir()
            .parent()
            .expect("target dir")
            .join("e2e-tmp")
            .join(format!("{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("appdata")).expect("create temp APPDATA");
        fs::create_dir_all(tmp.join("local")).expect("create temp LOCALAPPDATA");

        let done = Arc::new(AtomicBool::new(false));
        let pids = Arc::new(Mutex::new(Vec::<u32>::new()));
        {
            let (done, pids) = (done.clone(), pids.clone());
            std::thread::spawn(move || {
                let end = Instant::now() + WATCHDOG;
                while Instant::now() < end {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                eprintln!("e2e watchdog: test hung, killing its processes");
                for pid in pids.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                    let _ = Command::new("taskkill")
                        .args(["/F", "/PID", &pid.to_string()])
                        .output();
                }
                std::process::exit(101);
            });
        }
        Self {
            dir: tmp,
            suffix,
            resident: None,
            testwins: Vec::new(),
            starts: 0,
            done,
            pids,
            tracked: RefCell::new(Vec::new()),
        }
    }

    fn track(&self, pid: u32) {
        self.pids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
    }

    /// `%APPDATA%\BorderlessShatter\profiles.json` of this harness' isolated resident.
    pub fn profiles_path(&self) -> PathBuf {
        self.dir
            .join("appdata")
            .join("BorderlessShatter")
            .join("profiles.json")
    }

    /// Writes `profiles.json` (call before [`Harness::start_resident`] to seed it).
    pub fn write_profiles(&self, text: &str) {
        let path = self.profiles_path();
        fs::create_dir_all(path.parent().expect("parent")).expect("create profiles dir");
        fs::write(path, text).expect("write profiles.json");
    }

    /// `borderless-shatter.exe <args>` with this harness' isolation (`APPDATA`, `LOCALAPPDATA`,
    /// `BORDERLESS_SHATTER_INSTANCE`), `CREATE_NO_WINDOW` and a null stdin; not spawned. Usable
    /// without a resident. Register what it spawns with [`Harness::track_pid`].
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_borderless-shatter"));
        cmd.args(args)
            .env("APPDATA", self.dir.join("appdata"))
            .env("LOCALAPPDATA", self.dir.join("local"))
            .env("BORDERLESS_SHATTER_INSTANCE", &self.suffix)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null());
        cmd
    }

    /// Makes the guard stop process `pid` like its own resident (`WM_QUIT` to each GUI thread,
    /// then `TerminateProcess`), e.g. a resident started by `mcp`; the watchdog also kills it.
    pub fn track_pid(&self, pid: u32) {
        self.track(pid);
        // SAFETY: plain FFI call; the returned handle is owned here and closed by the guard.
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => self.tracked.borrow_mut().push((pid, handle)),
            Err(e) => panic!("cannot open tracked process {pid}: {e}"),
        }
    }

    fn log_file(&mut self, prefix: &str) -> fs::File {
        self.starts += 1;
        fs::File::create(self.dir.join(format!("{prefix}-{}.log", self.starts)))
            .expect("create log file")
    }

    /// Starts the isolated resident and connects to it.
    pub fn start_resident(&mut self) -> Client {
        assert!(self.resident.is_none(), "a resident is already running");
        let log = self.log_file("resident");
        let child = self
            .command(&[])
            .stdout(Stdio::from(log.try_clone().expect("clone log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start the resident (borderless-shatter.exe)");
        self.track(child.id());
        self.resident = Some(child);
        self.connect()
    }

    pub fn resident_pid(&self) -> u32 {
        self.resident.as_ref().expect("no resident").id()
    }

    pub fn pipe_name(&self) -> String {
        pipe_name_for(Some(&self.suffix)).expect("pipe name")
    }

    pub fn connect(&self) -> Client {
        match Client::connect_named(&self.pipe_name(), Duration::from_secs(15)) {
            Ok(c) => c,
            Err(e) => panic!("cannot connect to the resident: {e}"),
        }
    }

    /// `true` if a pipe client can still connect within `timeout`.
    pub fn pipe_reachable(&self, timeout: Duration) -> bool {
        Client::connect_named(&self.pipe_name(), timeout).is_ok()
    }

    /// Raw duplex connection to the pipe (to speak malformed or abandoned conversations).
    pub fn raw_pipe(&self) -> fs::File {
        let name = self.pipe_name();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::OpenOptions::new().read(true).write(true).open(&name) {
                Ok(f) => return f,
                Err(e) if Instant::now() < end => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("cannot open the pipe {name}: {e}"),
            }
        }
    }

    /// `TerminateProcess` on the resident (an abrupt exit: nothing is restored).
    pub fn kill_resident(&mut self) {
        let mut child = self.resident.take().expect("no resident");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Posts `WM_QUIT` to the resident's GUI thread(s); its exit status once it has exited.
    pub fn quit_resident(&mut self) -> Option<ExitStatus> {
        let child = self.resident.as_mut().expect("no resident");
        let pid = child.id();
        if quit_process(pid, Duration::from_secs(3), || child_exited(child)) {
            child.try_wait().ok().flatten()
        } else {
            None
        }
    }

    /// Starts a second resident with the same isolation and returns how it ended; `None` if it
    /// was still running after `timeout` (it is then killed).
    pub fn second_instance_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let log = self.log_file("second");
        let mut child = self
            .command(&[])
            .stdout(Stdio::from(log.try_clone().expect("clone log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start the second resident");
        self.track(child.id());
        let mut status = None;
        poll(timeout, || {
            status = child.try_wait().expect("try_wait");
            status.is_some()
        });
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        status
    }

    /// Starts a fake game window; `args` are `testwin` arguments (`--preset notepad`, ...).
    /// It sits at 100,100 with title `e2e-win-<n>` unless `args` say otherwise.
    pub fn testwin(&mut self, args: &[&str]) -> TestWin {
        let exe = profile_dir().join("examples").join("testwin.exe");
        assert!(
            exe.exists(),
            "{} is missing: run `cargo build --examples` first",
            exe.display()
        );
        let n = self.testwins.len() + 1;
        let log = self.dir.join(format!("testwin-{n}.log"));
        let mut cmd = Command::new(&exe);
        cmd.args([
            "--pos",
            "100,100",
            "--title",
            &format!("e2e-win-{n}"),
            "--log",
        ])
        .arg(&log)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("start testwin");
        let pid = child.id();
        self.track(pid);
        let mut hwnd = None;
        let found = poll(Duration::from_secs(10), || {
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!("testwin exited early ({status}); args {args:?}");
            }
            hwnd = visible_window_of(pid);
            hwnd.is_some()
        });
        self.testwins.push(child);
        assert!(found, "testwin window did not appear");
        let hwnd = hwnd.expect("hwnd");
        std::thread::sleep(Duration::from_millis(150));
        let orig = snap(hwnd);
        TestWin {
            hwnd,
            pid,
            log,
            orig,
        }
    }

    /// Kills one of this harness' fake games and waits for its window to vanish.
    pub fn kill_testwin(&mut self, w: &TestWin) {
        let child = self
            .testwins
            .iter_mut()
            .find(|c| c.id() == w.pid)
            .expect("not a harness testwin");
        let _ = child.kill();
        let _ = child.wait();
        wait_until(
            "the killed window to disappear",
            Duration::from_secs(5),
            || !window_exists(w.hwnd),
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Graceful first: a killed resident would leave a ghost tray icon behind.
        if let Some(mut resident) = self.resident.take() {
            let pid = resident.id();
            if !quit_process(pid, Duration::from_secs(2), || child_exited(&mut resident)) {
                let _ = resident.kill();
                let _ = resident.wait();
            }
        }
        for (pid, handle) in self.tracked.take() {
            stop_tracked(pid, handle);
        }
        for child in &mut self.testwins {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking()
            && let Ok(entries) = fs::read_dir(&self.dir)
        {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".log") {
                    eprintln!(
                        "--- {name} ---\n{}",
                        fs::read_to_string(e.path()).unwrap_or_default()
                    );
                }
            }
        }
        self.done.store(true, Ordering::Relaxed);
        for _ in 0..10 {
            if fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
