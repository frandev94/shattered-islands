//! Resident engine: owns the original state of every managed window (exact restore), resolves
//! targets (islands, monitors, `game_resolution`), runs the re-assert guard and answers IPC
//! requests. Single-threaded: everything here runs on the message-loop thread; the pipe thread
//! only enqueues requests (`WM_SHATTER_IPC`) and waits for the reply on a channel.
//!
//! Nothing blocks: apply replies after the settle window and `diagnose` after `observe_ms`, both
//! driven by one periodic `SetTimer` tick that only runs while some operation is in flight.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, WM_TIMER};

use crate::diagnose::{self, Sample, Sampler, WriteMark};
use crate::guard::{self, Action, Decision, Guard, GuardConfig, Observation, SkipReason};
use crate::ipc::{
    self, ApplyParams, ApplyResult, ClientSize, DiagnoseParams, DiagnoseResult, ErrorBody,
    ErrorCode, GetProfilesParams, GetProfilesResult, HelloParams, HelloResult, Id, IslandInfo,
    LayoutInfo, ListIslandsParams, ListIslandsResult, ListWindowsParams, ListWindowsResult,
    MAX_DELAY_MS, PENDING_GRACE_MS, PlaceParams, Request, Response, RestoreAllParams,
    RestoreAllResult, RestoreFailure, RestoreParams, RestoreResult, SetProfilesParams,
    SetProfilesResult, TimelineEntry, TimelineSource, WindowInfo, WindowMonitor, WindowRef,
};
use crate::islands::{self, IslandError, Layout, MonitorSel, Size};
use crate::profiles::{self, ProfileError, ProfileFile, Warning};
use crate::state_store::{StateStore, StoredEntry};
use crate::watcher;
use crate::win32::{self, HWND, ShowCmd, Win32Error};
use crate::window_state::{self, Offsets, Rect, STYLE_REMOVE, WindowState};

/// WM_APP + 5: the pipe thread queued a request. The loop drains the queue after every message,
/// so the window procedure does not need to do anything with it.
pub const WM_SHATTER_IPC: u32 = 0x8000 + 5;

/// One request handed from the pipe thread to the engine.
pub type IpcItem = (Request, oneshot::Sender<Response>);

const TIMER_TICK: usize = 1;
/// SetTimer granularity is ~15.6 ms anyway; asking for less only wastes wakeups.
const TICK_MS: u32 = 15;
/// `diagnose` waits this long past `observe_ms` so the sampler has finished.
const DIAG_MARGIN_MS: u64 = 30;
/// Timeline entries kept in a `diagnose` reply so the line stays below the 1 MiB protocol limit.
const MAX_TIMELINE: usize = 2000;
/// WS_EX_TOOLWINDOW
const WS_EX_TOOLWINDOW: u32 = 0x80;
/// Largest absolute offset / `game_resolution` side in pixels; real desktops are far below this.
const MAX_COORD: i32 = 1_000_000;

// ------------------------------------------------------------------ message window

/// What the message window's procedure hands to the main loop.
enum Event {
    Window(isize),
    Changed(isize),
    MoveSize(isize, bool),
    Tick,
}

thread_local! {
    /// Filled by `wnd_proc`, drained by `Engine::drain` after each `DispatchMessageW`.
    ///
    /// The procedure can run nested inside tray-icon's modal menu loop, so it only enqueues:
    /// the real work (foreign-window Win32 calls) happens at one known place with exclusive
    /// access to the engine, never re-entrantly from inside a menu.
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

/// Window procedure of the message-only window; pass it to `win32::create_message_window`.
///
/// # Safety
/// Only meant to be called by Windows as a window procedure.
pub unsafe extern "system" fn wnd_proc(
    h: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let event = match msg {
        watcher::WM_SHATTER_WINDOW => Event::Window(wparam.0 as isize),
        watcher::WM_SHATTER_CHANGED => Event::Changed(wparam.0 as isize),
        watcher::WM_SHATTER_MOVESIZE => Event::MoveSize(wparam.0 as isize, lparam.0 != 0),
        WM_TIMER if wparam.0 == TIMER_TICK => Event::Tick,
        WM_TIMER => {
            win32::kill_timer(h, wparam.0);
            return LRESULT(0);
        }
        _ => return win32::def_window_proc(h, msg, wparam, lparam),
    };
    EVENTS.with_borrow_mut(|queue| queue.push(event));
    LRESULT(0)
}

// ------------------------------------------------------------------ small helpers

fn err(code: ErrorCode, msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(code, msg)
}

fn base_name(s: &str) -> &str {
    s.rsplit(['\\', '/']).next().unwrap_or(s)
}

fn warning(code: &str, data: Value) -> Warning {
    Warning {
        code: code.into(),
        data,
    }
}

fn foreground() -> HWND {
    // SAFETY: plain FFI call without arguments.
    unsafe { GetForegroundWindow() }
}

/// Maps a failed write to the protocol error; an access denial on an elevated target is UIPI
/// (the resident runs `asInvoker`, so it cannot write to elevated windows).
fn w32(e: &Win32Error, pid: u32) -> ErrorBody {
    let Win32Error::Call { call, source } = e;
    if source.code() == windows::core::HRESULT::from_win32(5)
        && win32::is_elevated(pid) == Some(true)
    {
        return err(
            ErrorCode::AccessDenied,
            "access denied: the window belongs to an elevated process and Borderless Shatter is not elevated",
        )
        .with_data(json!({ "elevated": true }));
    }
    let hresult = source.code().0;
    let code = win32_code(hresult);
    // The OS description is localized; only the code goes into the (English) message.
    let what = if code == hresult as u32 {
        format!("HRESULT {code:#010x}")
    } else {
        format!("os error {code}")
    };
    err(ErrorCode::Win32Error, format!("{call} failed ({what})")).with_data(json!({
        "call": call,
        "code": code,
        "hresult": format!("{:#010x}", hresult as u32),
    }))
}

/// `HRESULT_FROM_WIN32` wraps the code in facility 7; an HRESULT of another facility has no
/// Win32 code, so it is returned unchanged.
fn win32_code(hresult: i32) -> u32 {
    let v = hresult as u32;
    if v >> 16 == 0x8007 { v & 0xFFFF } else { v }
}

fn island_err(e: IslandError) -> ErrorBody {
    let code = match &e {
        IslandError::BadRef(_) => ErrorCode::InvalidParams,
        IslandError::IslandNotFound(_) => ErrorCode::IslandNotFound,
        IslandError::MonitorUnavailable(_) => ErrorCode::MonitorUnavailable,
    };
    err(code, e.to_string())
}

fn profile_err(e: ProfileError) -> ErrorBody {
    let code = match &e {
        ProfileError::UnsupportedVersion(_) => ErrorCode::UnsupportedVersion,
        ProfileError::RevisionConflict => ErrorCode::RevisionConflict,
        _ => ErrorCode::ProfileError,
    };
    err(code, e.to_string())
}

/// `island_ref` as the ids of the layout and island it resolves to, so engine output and
/// `state.json` use canonical refs; a ref that does not resolve is kept as given.
fn canonical_island(layouts: &[Layout], island_ref: Option<String>) -> Option<String> {
    island_ref.map(|r| islands::canonical_ref(layouts, &r).unwrap_or(r))
}

fn window_not_found(raw: isize) -> ErrorBody {
    err(
        ErrorCode::WindowNotFound,
        format!("window {raw} does not exist"),
    )
}

fn empty_profiles() -> ProfileFile {
    ProfileFile {
        version: profiles::SUPPORTED_VERSION,
        layouts: vec![],
        profiles: vec![],
        extra: Default::default(),
    }
}

/// Warnings for the settled state of a placed window: forced size, forced position, frame back.
fn outcome_warnings(
    target: Rect,
    fin: Rect,
    frame_back: bool,
    after_ms: Option<u64>,
) -> Vec<Warning> {
    let mut out = Vec::new();
    if fin.width() != target.width() || fin.height() != target.height() {
        out.push(warning(
            "size_forced",
            json!({ "w": fin.width(), "h": fin.height(), "after_ms": after_ms }),
        ));
    } else if fin != target {
        out.push(warning(
            "position_forced",
            json!({ "x": fin.left, "y": fin.top, "after_ms": after_ms }),
        ));
    }
    if frame_back {
        out.push(warning("style_reverted", json!({ "after_ms": after_ms })));
    }
    out
}

/// Our writes (state of the window right after each) merged with the changes the sampler saw.
fn build_timeline(marks: &[WriteMark], samples: &[Sample]) -> Vec<TimelineEntry> {
    let entry = |t_ms: f64, source, op: Option<String>, s: Option<&Sample>| TimelineEntry {
        t_ms,
        source,
        op,
        style: ipc::hex(s.map_or(0, |s| s.style)),
        exstyle: ipc::hex(s.map_or(0, |s| s.exstyle)),
        rect: s.map_or_else(Rect::default, |s| s.rect),
    };
    let mut out: Vec<TimelineEntry> = marks
        .iter()
        .map(|m| {
            let after = samples.iter().find(|s| s.t_ms >= m.t_ms).or(samples.last());
            entry(m.t_ms, TimelineSource::Us, Some(m.op.clone()), after)
        })
        .collect();
    let mut prev: Option<&Sample> = None;
    for s in samples {
        if prev.is_none_or(|p| (p.style, p.exstyle, p.rect) != (s.style, s.exstyle, s.rect)) {
            out.push(entry(s.t_ms, TimelineSource::Window, None, Some(s)));
        }
        prev = Some(s);
    }
    out.sort_by(|a, b| a.t_ms.total_cmp(&b.t_ms));
    out.truncate(MAX_TIMELINE);
    out
}

/// Reads the window against its target.
fn observe(h: HWND, target: Rect) -> Option<Observation> {
    let (style, exstyle) = win32::get_styles(h).ok()?;
    let rect = win32::window_rect(h).ok()?;
    let frame_back = window_state::borderless_styles(style, exstyle) != (style, exstyle);
    Some(if rect == target && !frame_back {
        Observation::AtTarget
    } else {
        Observation::Deviated { rect, frame_back }
    })
}

/// Timestamps of our own writes for `diagnose`, taken after each call returns.
struct Marks {
    t0: Instant,
    list: Vec<WriteMark>,
}

impl Marks {
    fn mark(&mut self, op: &str) {
        self.list.push(WriteMark {
            t_ms: self.t0.elapsed().as_secs_f64() * 1000.0,
            op: op.into(),
        });
    }
}

/// Writes the style only if `borderless_styles` changes it and the rect only if it differs; a
/// style change still needs `SetWindowPos` (FRAMECHANGED) so the frame is actually dropped.
fn write_geometry(
    h: HWND,
    target: Rect,
    pid: u32,
    mut marks: Option<&mut Marks>,
) -> Result<(), ErrorBody> {
    let (style, exstyle) = win32::get_styles(h).map_err(|e| w32(&e, pid))?;
    let (new_style, new_ex) = window_state::borderless_styles(style, exstyle);
    let styles_changed = (new_style, new_ex) != (style, exstyle);
    if styles_changed {
        win32::set_styles(h, new_style, new_ex).map_err(|e| w32(&e, pid))?;
        if let Some(m) = marks.as_deref_mut() {
            m.mark("set_styles");
        }
    }
    if styles_changed || win32::window_rect(h).map_err(|e| w32(&e, pid))? != target {
        win32::set_pos(h, target, win32::SWP_APPLY).map_err(|e| w32(&e, pid))?;
        if let Some(m) = marks {
            m.mark("set_pos");
        }
    }
    Ok(())
}

/// Auto-apply only: popups without a title (menus, tooltips) must not be resized now that the
/// guard no longer requires frame bits. Windows with a frame, or a title and no tool-window
/// style, qualify.
fn auto_candidate_style(style: u32, exstyle: u32, has_title: bool) -> bool {
    style & STYLE_REMOVE != 0 || (exstyle & WS_EX_TOOLWINDOW == 0 && has_title)
}

fn auto_candidate(h: HWND) -> bool {
    win32::get_styles(h).is_ok_and(|(style, exstyle)| {
        auto_candidate_style(style, exstyle, !win32::window_title(h).is_empty())
    })
}

/// Saves `entries`. A state file `readopt` could not read (`unreadable`) is first moved to
/// `<file>.bad`, replacing an older bad copy, so its restore data is never overwritten; while
/// that move fails nothing is written.
fn save_state(
    store: &StateStore,
    unreadable: &mut Option<PathBuf>,
    entries: &[StoredEntry],
) -> Result<(), String> {
    if let Some(path) = unreadable.as_deref() {
        match std::fs::rename(path, path.with_extension("json.bad")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the unreadable {} could not be moved aside, so it is not overwritten: {}",
                    path.display(),
                    crate::io_text(&e)
                ));
            }
            _ => *unreadable = None,
        }
    }
    store.save(entries).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ state

/// Pending IPC reply plus what `apply` has to report when the guard settles.
struct Reply {
    id: Id,
    tx: oneshot::Sender<Response>,
}

impl Reply {
    fn send<T: Serialize>(self, r: Result<T, ErrorBody>) {
        let resp = match r.and_then(|v| {
            serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
        }) {
            Ok(v) => Response::ok(self.id, v),
            Err(e) => Response::err(self.id, e),
        };
        let _ = self.tx.send(resp);
    }

    fn fail(self, e: ErrorBody) {
        let _ = self.tx.send(Response::err(self.id, e));
    }
}

/// An `apply` waiting for the guard to settle (or give up).
struct Pending {
    reply: Option<Reply>,
    /// When settling started (ms since engine start).
    started: u64,
    /// First deviation seen while settling.
    first_dev: Option<u64>,
    /// The frame came back at some point of the settle window, even if re-asserted since.
    frame_back_seen: bool,
    deadline: u64,
    warnings: Vec<Warning>,
}

/// Original state of a managed window plus the rect it is forced to, fixed at capture time.
struct Managed {
    state: WindowState,
    pid: u32,
    process: String,
    target: Rect,
    /// Canonical `"layout/island"` it was placed in.
    island: Option<String>,
    /// `None` for windows re-adopted from disk and for `diagnose --keep`: restorable, not guarded.
    guard: Option<Guard>,
    watching: bool,
    /// Style pass still waiting for `delay_ms`.
    delayed_until: Option<u64>,
    /// Guard cycle in progress: the tick polls the window and asks the guard for its next step.
    settling: bool,
    reassert_due: Option<u64>,
    /// The user is dragging or resizing the window (no re-assert meanwhile).
    moving: bool,
    pending: Option<Pending>,
    forced: Option<Rect>,
}

/// What the plan builder decided for one window (request fields over profile fields).
#[derive(Default, Debug, PartialEq)]
struct Plan {
    island: Option<String>,
    monitor: Option<MonitorSel>,
    /// A profile's v0.1 monitor index that no longer exists falls back to the current monitor.
    lenient: bool,
    offsets: Offsets,
    game_resolution: Option<Size>,
    delay_ms: u64,
}

struct PlanReq {
    island: Option<String>,
    monitor: Option<MonitorSel>,
    offsets: Option<Offsets>,
    delay_ms: Option<u64>,
    use_profile: bool,
    game_resolution: Option<Size>,
}

enum Outcome {
    Done(ApplyResult),
    /// The reply (if any) was stored and is answered when the guard settles.
    Started,
}

fn offsets_in_range(o: &Offsets) -> bool {
    [o.left, o.top, o.right, o.bottom]
        .iter()
        .all(|v| (-MAX_COORD..=MAX_COORD).contains(v))
}

/// Request fields override profile fields; a profile's `island` overrides its monitor/offsets.
/// Pure: the caller supplies the process name and the profile file.
fn build_plan(file: &ProfileFile, name: &str, req: PlanReq) -> Result<Plan, ErrorBody> {
    if req.delay_ms.is_some_and(|d| d > MAX_DELAY_MS) {
        return Err(err(
            ErrorCode::InvalidParams,
            format!("delay_ms out of range (max {MAX_DELAY_MS})"),
        ));
    }
    if req.offsets.as_ref().is_some_and(|o| !offsets_in_range(o)) {
        return Err(err(
            ErrorCode::InvalidParams,
            format!("offsets out of range (±{MAX_COORD})"),
        ));
    }
    let prof = if req.use_profile {
        profiles::find(file, name)
    } else {
        None
    };
    if let Some(p) = prof
        && !offsets_in_range(&p.offsets)
    {
        return Err(err(
            ErrorCode::ProfileError,
            format!("offsets out of range in the profile for {}", p.process),
        ));
    }
    let (p_island, p_monitor, p_offsets) = match prof {
        Some(p) => match &p.island {
            Some(i) => (Some(i.clone()), None, Offsets::default()),
            None => (None, p.monitor.index().map(MonitorSel::Index), p.offsets),
        },
        None => (None, None, Offsets::default()),
    };
    let mut plan = Plan {
        delay_ms: req
            .delay_ms
            .or(prof.map(|p| p.delay_ms.min(MAX_DELAY_MS)))
            .unwrap_or(0),
        game_resolution: req.game_resolution.or(prof.and_then(|p| p.game_resolution)),
        ..Plan::default()
    };
    if let Some(i) = req.island {
        plan.island = Some(i);
    } else if req.monitor.is_some() || req.offsets.is_some() {
        match req.monitor {
            Some(m) => plan.monitor = Some(m),
            None => {
                plan.monitor = p_monitor;
                plan.lenient = true;
            }
        }
        plan.offsets = req.offsets.unwrap_or(p_offsets);
    } else {
        plan.island = p_island;
        plan.monitor = p_monitor;
        plan.lenient = true;
        plan.offsets = p_offsets;
    }
    Ok(plan)
}

/// A `diagnose` measuring: sampler running, reply owed at `deadline`.
struct Diag {
    raw: isize,
    process: String,
    target: Rect,
    deadline: u64,
    sampler: Sampler,
    marks: Marks,
    keep: bool,
    reply: Reply,
}

pub struct Engine {
    msg_hwnd: HWND,
    ipc_rx: mpsc::Receiver<IpcItem>,
    t0: Instant,
    profiles: ProfileFile,
    profiles_path: Option<PathBuf>,
    profiles_revision: String,
    store: Option<StateStore>,
    state_unreadable: Option<PathBuf>,
    managed: HashMap<isize, Managed>,
    diags: Vec<Diag>,
    /// Windows the user restored; automatic handling skips them until an explicit apply.
    user_restored: HashSet<isize>,
    /// Last foreground window that is not ours: the tray menu steals the foreground while open.
    last_foreground: isize,
    tick_armed: bool,
    status: Option<String>,
    menu_dirty: bool,
}

impl Engine {
    /// `msg_hwnd` owns the tick timer; `ipc_rx` is fed by the pipe handler.
    pub fn new(msg_hwnd: HWND, ipc_rx: mpsc::Receiver<IpcItem>) -> Self {
        let mut status = None;
        let profiles_path = match profiles::default_path() {
            Ok(p) => Some(p),
            Err(e) => {
                status = Some(format!("profiles: {e}"));
                None
            }
        };
        let (profiles, profiles_revision) = match profiles_path.as_deref().map(profiles::load) {
            Some(Ok(loaded)) => loaded,
            Some(Err(e)) => {
                eprintln!("profiles: {e}");
                status = Some(format!("profiles: {e}"));
                (empty_profiles(), String::new())
            }
            None => (empty_profiles(), String::new()),
        };
        let store = match StateStore::default_path() {
            Ok(p) => Some(StateStore::open(p)),
            Err(e) => {
                status.get_or_insert_with(|| format!("state: {e}"));
                None
            }
        };
        Self {
            msg_hwnd,
            ipc_rx,
            t0: Instant::now(),
            profiles,
            profiles_path,
            profiles_revision,
            store,
            state_unreadable: None,
            managed: HashMap::new(),
            diags: Vec::new(),
            user_restored: HashSet::new(),
            last_foreground: 0,
            tick_armed: false,
            status,
            menu_dirty: false,
        }
    }

    pub fn profiles(&self) -> &ProfileFile {
        &self.profiles
    }

    /// Presets plus the user's layouts, as shown in the tray submenu.
    pub fn layouts(&self) -> Vec<Layout> {
        islands::merged_layouts(&self.profiles.layouts)
    }

    /// Latest message for the tray tooltip, once.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
    }

    /// True once after the profiles changed, so the tray rebuilds its menu.
    pub fn take_menu_dirty(&mut self) -> bool {
        std::mem::take(&mut self.menu_dirty)
    }

    fn now(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("{msg}");
        self.status = Some(msg);
    }

    // -------------------------------------------------------------- lifecycle

    /// Re-adopts windows recorded by a previous run, then applies profiles to open windows.
    pub fn startup(&mut self) {
        self.readopt();
        let fg = foreground();
        if !fg.0.is_null() && win32::window_pid(fg) != std::process::id() {
            self.last_foreground = win32::hwnd_to_isize(fg);
        }
        for h in watcher::initial_scan() {
            self.on_window(h);
        }
    }

    /// Entries whose hwnd still exists with the same pid become managed again; the rest are dropped.
    fn readopt(&mut self) {
        let Some(store) = &self.store else { return };
        let entries = match store.load() {
            Ok(e) => e,
            // Nothing is re-adopted, and the file is kept: the next save moves it aside first.
            Err(e) => {
                self.state_unreadable = Some(store.path().to_path_buf());
                self.note(format!("state: {e}"));
                return;
            }
        };
        let now = self.now();
        let layouts = self.layouts();
        for e in entries {
            let h = win32::hwnd_from_isize(e.hwnd);
            if !win32::is_window(h) || win32::window_pid(h) != e.pid {
                continue;
            }
            let raw = e.hwnd;
            // Files from before the target was stored: restorable, but nothing to guard.
            let guarded = e.target.is_some();
            let target = e
                .target
                .unwrap_or_else(|| win32::window_rect(h).unwrap_or(e.state.rect));
            self.managed.insert(
                raw,
                Managed {
                    state: e.state,
                    pid: e.pid,
                    process: e.process,
                    target,
                    island: canonical_island(&layouts, e.island),
                    guard: None,
                    watching: false,
                    delayed_until: None,
                    settling: false,
                    reassert_due: None,
                    moving: false,
                    pending: None,
                    forced: None,
                },
            );
            if guarded {
                self.begin_settle(raw, now);
            }
        }
        self.persist();
    }

    /// Restores everything; called after the message loop ends.
    pub fn shutdown(&mut self) {
        self.diags.clear();
        self.restore_everything();
        self.sync_timer();
    }

    fn persist(&mut self) {
        let Some(store) = &self.store else { return };
        let entries: Vec<StoredEntry> = self
            .managed
            .iter()
            .map(|(&hwnd, m)| StoredEntry {
                hwnd,
                pid: m.pid,
                process: m.process.clone(),
                state: m.state,
                target: Some(m.target),
                island: m.island.clone(),
            })
            .collect();
        if let Err(e) = save_state(store, &mut self.state_unreadable, &entries) {
            self.note(format!("state: {e}"));
        }
    }

    /// Drops managed windows that closed (or whose hwnd was reused by another process).
    fn prune(&mut self) {
        let dead: Vec<isize> = self
            .managed
            .iter()
            .filter(|&(&raw, m)| {
                let h = win32::hwnd_from_isize(raw);
                !win32::is_window(h) || win32::window_pid(h) != m.pid
            })
            .map(|(&raw, _)| raw)
            .collect();
        if dead.is_empty() {
            return;
        }
        for raw in dead {
            if let Some(m) = self.managed.remove(&raw) {
                watcher::unwatch(win32::hwnd_from_isize(raw));
                if let Some(Pending { reply: Some(r), .. }) = m.pending {
                    r.fail(err(
                        ErrorCode::WindowGone,
                        "the window closed during the operation",
                    ));
                }
            }
        }
        self.persist();
    }

    // -------------------------------------------------------------- tray API

    /// Tray "Apply": same matching as the automatic path (exclusions, path-style names), but
    /// ignores `enabled` and any earlier manual restore.
    pub fn apply_process(&mut self, process: &str) {
        for h in watcher::initial_scan() {
            let Ok(name) = win32::process_name_of(h) else {
                continue;
            };
            if profiles::find(&self.profiles, &name)
                .is_none_or(|p| !p.process.eq_ignore_ascii_case(process))
            {
                continue;
            }
            self.user_restored.remove(&win32::hwnd_to_isize(h));
            self.apply_auto(h);
        }
    }

    /// Tray "Place active window on island".
    pub fn place_foreground(&mut self, island_ref: &str) {
        let fg = foreground();
        let h = if fg.0.is_null() || win32::window_pid(fg) == std::process::id() {
            win32::hwnd_from_isize(self.last_foreground)
        } else {
            fg
        };
        if !win32::is_window(h) {
            self.note("there is no active window to place");
            return;
        }
        self.user_restored.remove(&win32::hwnd_to_isize(h));
        let plan = Plan {
            island: Some(island_ref.to_string()),
            ..Plan::default()
        };
        match self.apply_window(h, &plan, &mut None) {
            Ok(Outcome::Done(r)) if !r.applied => {
                let reason = r.reason.unwrap_or_default();
                self.note(format!("the window was not placed: {reason}"));
            }
            Err(e) => self.note(e.message),
            _ => {}
        }
    }

    /// Tray "Restore all".
    pub fn restore_all_user(&mut self) {
        self.restore_everything();
    }

    /// Tray "Reload profiles". A broken file keeps the profiles already in memory.
    pub fn reload(&mut self) {
        let Some(path) = self.profiles_path.clone() else {
            self.note("profiles: the APPDATA variable is not set");
            return;
        };
        match profiles::load(&path) {
            Ok((file, rev)) => {
                self.profiles = file;
                self.profiles_revision = rev;
                self.menu_dirty = true;
                self.note("profiles reloaded");
            }
            Err(e) => self.note(format!("profiles: {e}")),
        }
    }

    // -------------------------------------------------------------- events

    /// Processes queued window events, IPC requests and the tick; call after every message.
    pub fn drain(&mut self) {
        for event in EVENTS.with_borrow_mut(std::mem::take) {
            match event {
                Event::Window(raw) => self.on_window(win32::hwnd_from_isize(raw)),
                Event::Changed(raw) => self.on_changed(raw),
                Event::MoveSize(raw, start) => {
                    if let Some(m) = self.managed.get_mut(&raw) {
                        m.moving = start;
                    }
                    // A re-assert deferred while the user was dragging resumes here.
                    if !start {
                        self.on_changed(raw);
                    }
                }
                Event::Tick => self.tick(),
            }
        }
        while let Ok((req, tx)) = self.ipc_rx.try_recv() {
            self.handle_ipc(req, tx);
        }
        self.sync_timer();
    }

    /// Show/foreground event: applies the matching profile, skipping managed and user-restored windows.
    fn on_window(&mut self, h: HWND) {
        if !win32::is_window(h) {
            return;
        }
        if foreground() == h && win32::window_pid(h) != std::process::id() {
            self.last_foreground = win32::hwnd_to_isize(h);
        }
        let raw = win32::hwnd_to_isize(h);
        if self.managed.contains_key(&raw) {
            return;
        }
        if !self.user_restored.is_empty() {
            self.user_restored
                .retain(|&r| win32::is_window(win32::hwnd_from_isize(r)));
            if self.user_restored.contains(&raw) {
                return;
            }
        }
        let Ok(name) = win32::process_name_of(h) else {
            return;
        };
        if profiles::find(&self.profiles, &name).is_none_or(|p| !p.enabled) || !auto_candidate(h) {
            return;
        }
        self.apply_auto(h);
    }

    /// Applies the window's profile with no reply to send; failures go to the tray status.
    fn apply_auto(&mut self, h: HWND) {
        let req = PlanReq {
            island: None,
            monitor: None,
            offsets: None,
            delay_ms: None,
            use_profile: true,
            game_resolution: None,
        };
        let res = self
            .plan_for(h, req)
            .and_then(|plan| self.apply_window(h, &plan, &mut None));
        if let Err(e) = res
            && e.code != ErrorCode::Busy
        {
            self.note(e.message);
        }
    }

    /// LOCATIONCHANGE/STATECHANGE of a managed window: a deviation starts (or continues) the
    /// guard's re-assert loop. Events that find the window at its target are our own writes.
    fn on_changed(&mut self, raw: isize) {
        let now = self.now();
        let Some(m) = self.managed.get(&raw) else {
            return;
        };
        let Some(g) = &m.guard else { return };
        if m.moving || m.reassert_due.is_some() || g.fights_back() {
            return;
        }
        let h = win32::hwnd_from_isize(raw);
        if !win32::is_window(h) {
            self.prune();
            return;
        }
        if win32::is_iconic(h) {
            return;
        }
        if let Some(obs @ Observation::Deviated { .. }) = observe(h, m.target) {
            self.feed(raw, now, obs);
        }
    }

    // -------------------------------------------------------------- guard loop

    fn feed(&mut self, raw: isize, now: u64, obs: Observation) {
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        let Some(g) = m.guard.as_mut() else { return };
        let act = g.on_observation(now, obs);
        if let Observation::Deviated { frame_back, .. } = obs
            && let Some(p) = m.pending.as_mut()
        {
            p.first_dev.get_or_insert(now);
            p.frame_back_seen |= frame_back;
        }
        self.handle_action(raw, act, now);
    }

    fn handle_action(&mut self, raw: isize, act: Action, now: u64) {
        match act {
            Action::None => {}
            Action::ReassertAt(at) if at <= now => self.perform_reassert(raw, now),
            Action::ReassertAt(at) => {
                if let Some(m) = self.managed.get_mut(&raw) {
                    m.reassert_due = Some(at);
                    m.settling = true;
                }
            }
            Action::GiveUp { forced } => {
                let Some(m) = self.managed.get_mut(&raw) else {
                    return;
                };
                m.forced = Some(forced);
                m.settling = false;
                m.reassert_due = None;
                let msg = format!(
                    "{}: the game fights back and forces {}x{}",
                    m.process,
                    forced.width(),
                    forced.height()
                );
                self.note(msg);
                self.complete_pending(raw, false);
            }
            Action::Settled => {
                if let Some(m) = self.managed.get_mut(&raw) {
                    m.settling = false;
                }
                self.complete_pending(raw, true);
            }
        }
    }

    /// Writes the target again if the window is still off it; otherwise tells the guard it conformed.
    fn perform_reassert(&mut self, raw: isize, now: u64) {
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        let h = win32::hwnd_from_isize(raw);
        if m.moving || win32::is_iconic(h) {
            // No re-arming here (that spun the tick): the restore / MOVESIZEEND event resumes
            // the guard through `on_changed`, which finds its planned re-assert still pending.
            m.reassert_due = None;
            m.settling = false;
            return;
        }
        let (target, pid) = (m.target, m.pid);
        let obs = observe(h, target);
        if let Some(Observation::Deviated { frame_back, .. }) = obs
            && let Some(p) = m.pending.as_mut()
        {
            p.first_dev.get_or_insert(now);
            p.frame_back_seen |= frame_back;
        }
        match obs {
            None => self.prune(),
            Some(Observation::AtTarget) => {
                let Some(m) = self.managed.get_mut(&raw) else {
                    return;
                };
                m.reassert_due = None;
                let act = m.guard.as_mut().map_or(Action::None, |g| {
                    g.on_observation(now, Observation::AtTarget)
                });
                self.handle_action(raw, act, now);
            }
            Some(Observation::Deviated { .. }) => match write_geometry(h, target, pid, None) {
                Ok(()) => {
                    if let Some(m) = self.managed.get_mut(&raw) {
                        if let Some(g) = m.guard.as_mut() {
                            g.on_reasserted(now);
                        }
                        m.reassert_due = None;
                        m.settling = true;
                    }
                }
                Err(e) => {
                    if let Some(m) = self.managed.get_mut(&raw) {
                        m.guard = None;
                        m.settling = false;
                        m.reassert_due = None;
                    }
                    self.note(e.message.clone());
                    self.fail_pending(raw, e);
                }
            },
        }
    }

    /// One tick of the periodic timer: delayed style passes, guard polling, reply deadlines, diagnoses.
    fn tick(&mut self) {
        let now = self.now();
        let delayed: Vec<isize> = self
            .managed
            .iter()
            .filter(|(_, m)| m.delayed_until.is_some_and(|t| t <= now))
            .map(|(&raw, _)| raw)
            .collect();
        for raw in delayed {
            self.finish_delayed(raw, now);
        }
        let active: Vec<isize> = self
            .managed
            .iter()
            .filter(|(_, m)| {
                m.guard.is_some()
                    && m.delayed_until.is_none()
                    && (m.settling || m.reassert_due.is_some())
            })
            .map(|(&raw, _)| raw)
            .collect();
        for raw in active {
            self.tick_guard(raw, now);
        }
        let late: Vec<isize> = self
            .managed
            .iter()
            .filter(|(_, m)| m.pending.as_ref().is_some_and(|p| p.deadline <= now))
            .map(|(&raw, _)| raw)
            .collect();
        for raw in late {
            self.complete_pending(raw, false);
        }
        while let Some(i) = self.diags.iter().position(|d| d.deadline <= now) {
            self.finish_diag(i);
        }
        self.prune();
    }

    fn tick_guard(&mut self, raw: isize, now: u64) {
        // Events can miss the first fight (style changes may not raise STATECHANGE), so while a
        // cycle is open the window is also polled.
        let Some(m) = self.managed.get(&raw) else {
            return;
        };
        let h = win32::hwnd_from_isize(raw);
        if m.reassert_due.is_none()
            && !m.moving
            && !win32::is_iconic(h)
            && let Some(obs @ Observation::Deviated { .. }) = observe(h, m.target)
        {
            self.feed(raw, now, obs);
        }
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        let act = m.guard.as_mut().map_or(Action::None, |g| g.on_tick(now));
        // A re-assert the guard no longer wants (the game conformed) must not block polling.
        if m.reassert_due.is_some_and(|d| d <= now) && !matches!(act, Action::ReassertAt(_)) {
            m.reassert_due = None;
        }
        self.handle_action(raw, act, now);
    }

    fn sync_timer(&mut self) {
        let need = !self.diags.is_empty()
            || self.managed.values().any(|m| {
                m.delayed_until.is_some()
                    || m.settling
                    || m.reassert_due.is_some()
                    || m.pending.is_some()
            });
        if need && !self.tick_armed {
            match win32::set_timer(self.msg_hwnd, TIMER_TICK, TICK_MS) {
                Ok(()) => self.tick_armed = true,
                Err(e) => self.note(format!("timer: {e}")),
            }
        } else if !need && self.tick_armed {
            win32::kill_timer(self.msg_hwnd, TIMER_TICK);
            self.tick_armed = false;
        }
    }

    // -------------------------------------------------------------- pending apply replies

    fn complete_pending(&mut self, raw: isize, settled: bool) {
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        let Some(p) = m.pending.take() else { return };
        let h = win32::hwnd_from_isize(raw);
        let target = m.target;
        let fin = win32::window_rect(h).unwrap_or(target);
        let frame_back = p.frame_back_seen
            || win32::get_styles(h)
                .is_ok_and(|(s, e)| window_state::borderless_styles(s, e) != (s, e));
        let after_ms = p.first_dev.map(|t| t.saturating_sub(p.started));
        let mut warnings = p.warnings;
        warnings.extend(outcome_warnings(target, fin, frame_back, after_ms));
        if let Some(f) = m.forced {
            warnings.push(warning(
                "fights_back",
                json!({ "forced": { "w": f.width(), "h": f.height() } }),
            ));
        }
        if let Some(reply) = p.reply {
            reply.send(Ok(ApplyResult {
                applied: true,
                reason: None,
                hwnd: raw,
                target,
                r#final: fin,
                settled,
                warnings,
            }));
        }
    }

    fn fail_pending(&mut self, raw: isize, e: ErrorBody) {
        if let Some(Pending { reply: Some(r), .. }) =
            self.managed.get_mut(&raw).and_then(|m| m.pending.take())
        {
            r.fail(e);
        }
    }

    // -------------------------------------------------------------- targets

    /// Request fields override profile fields; a profile's `island` overrides its monitor/offsets.
    fn plan_for(&self, h: HWND, req: PlanReq) -> Result<Plan, ErrorBody> {
        let name = win32::process_name_of(h).map_err(|e| w32(&e, win32::window_pid(h)))?;
        build_plan(&self.profiles, &name, req)
    }

    fn resolve_target(&self, h: HWND, plan: &Plan) -> Result<(Rect, Vec<Warning>), ErrorBody> {
        let pid = win32::window_pid(h);
        let monitors = win32::monitors().map_err(|e| w32(&e, pid))?;
        let mut warnings = Vec::new();
        let mut rect = if let Some(r) = &plan.island {
            islands::resolve(
                &islands::merged_layouts(&self.profiles.layouts),
                r,
                &monitors,
            )
            .map_err(island_err)?
        } else {
            let current = win32::monitor_rect_of(h).map_err(|e| w32(&e, pid))?;
            let monitor = match &plan.monitor {
                None => current,
                Some(sel) => match islands::select_monitor(sel, &monitors) {
                    Ok(m) => m.rect,
                    Err(_) if plan.lenient => current,
                    Err(e) => return Err(island_err(e)),
                },
            };
            window_state::target_rect(monitor, plan.offsets)
        };
        if let Some(size) = plan.game_resolution {
            if size.width <= 0
                || size.height <= 0
                || size.width > MAX_COORD
                || size.height > MAX_COORD
            {
                return Err(err(
                    ErrorCode::InvalidParams,
                    format!("invalid game_resolution: {}x{}", size.width, size.height),
                ));
            }
            if plan.island.is_some() && rect.height() > 0 {
                let island_aspect = f64::from(rect.width()) / f64::from(rect.height());
                let game_aspect = f64::from(size.width) / f64::from(size.height);
                if (island_aspect - game_aspect).abs() / game_aspect > 0.01 {
                    warnings.push(warning(
                        "island_aspect_mismatch",
                        json!({
                            "island": { "w": rect.width(), "h": rect.height() },
                            "game": { "w": size.width, "h": size.height },
                        }),
                    ));
                }
            }
            let (centred, clipped) = islands::center_in(rect, size);
            if clipped {
                warnings.push(warning(
                    "game_resolution_exceeds_island",
                    json!({
                        "island": { "w": rect.width(), "h": rect.height() },
                        "game": { "w": size.width, "h": size.height },
                    }),
                ));
            }
            rect = centred;
        }
        Ok((rect, warnings))
    }

    // -------------------------------------------------------------- apply / restore

    /// Guard decision, capture and first write. `reply` is taken when the answer has to wait
    /// for the settle window (`Outcome::Started`).
    fn apply_window(
        &mut self,
        h: HWND,
        plan: &Plan,
        reply: &mut Option<Reply>,
    ) -> Result<Outcome, ErrorBody> {
        let raw = win32::hwnd_to_isize(h);
        if !win32::is_window(h) {
            return Err(window_not_found(raw));
        }
        let pid = win32::window_pid(h);
        let process = win32::process_name_of(h).map_err(|e| w32(&e, pid))?;
        let excluded = profiles::is_excluded(&process);
        let visible = win32::is_top_level(h) && win32::is_visible(h);
        let managed = self.managed.contains_key(&raw);
        if self.diags.iter().any(|d| d.raw == raw)
            || self
                .managed
                .get(&raw)
                .is_some_and(|m| m.pending.is_some() || m.delayed_until.is_some())
        {
            return Err(err(
                ErrorCode::Busy,
                "an operation on that window is already in progress",
            ));
        }
        let (target, warnings) = if excluded || !visible {
            (Rect::default(), Vec::new())
        } else {
            self.resolve_target(h, plan)?
        };
        let (style, exstyle) = win32::get_styles(h).map_err(|e| w32(&e, pid))?;
        let rect = win32::window_rect(h).map_err(|e| w32(&e, pid))?;
        let now = self.now();
        let island = canonical_island(&self.layouts(), plan.island.clone());
        let skipped = |reason: &str, target: Rect| {
            Ok(Outcome::Done(ApplyResult {
                applied: false,
                reason: Some(reason.into()),
                hwnd: raw,
                target,
                r#final: rect,
                settled: true,
                warnings: Vec::new(),
            }))
        };
        match guard::decide(
            excluded,
            visible,
            managed,
            rect == target,
            window_state::borderless_styles(style, exstyle) != (style, exstyle),
        ) {
            Decision::Skip(SkipReason::Excluded) => return skipped("excluded", target),
            Decision::Skip(SkipReason::NotTopLevelVisible) => {
                return Err(err(
                    ErrorCode::WindowNotFound,
                    "not a visible top-level window",
                ));
            }
            Decision::AlreadyPlaced => {
                if let Some(m) = self.managed.get_mut(&raw) {
                    m.target = target;
                    m.island = island;
                    if let Some(g) = m.guard.as_mut() {
                        g.reset(target, now);
                    }
                }
                self.persist();
                return Ok(Outcome::Done(ApplyResult {
                    applied: false,
                    reason: Some("already_placed".into()),
                    hwnd: raw,
                    target,
                    r#final: rect,
                    settled: true,
                    warnings,
                }));
            }
            Decision::Capture => {}
        }
        let fresh = !managed;
        if fresh {
            self.capture(h, pid, &process, target, island)?;
        } else if let Some(m) = self.managed.get_mut(&raw) {
            m.target = target;
            m.island = island;
            self.persist();
        }
        let delayed = fresh && plan.delay_ms > 0;
        // v0.1 order for delayed apply: position now, style pass after the delay.
        let written = if delayed {
            if rect == target {
                Ok(())
            } else {
                win32::set_pos(h, target, win32::SWP_APPLY).map_err(|e| w32(&e, pid))
            }
        } else {
            write_geometry(h, target, pid, None)
        };
        if let Err(e) = written {
            if fresh {
                let _ = self.restore_window(raw);
            }
            return Err(e);
        }
        let Some(m) = self.managed.get_mut(&raw) else {
            return Err(err(ErrorCode::WindowGone, "the window disappeared"));
        };
        m.pending = Some(Pending {
            reply: reply.take(),
            started: now,
            first_dev: None,
            frame_back_seen: false,
            deadline: now
                .saturating_add(plan.delay_ms)
                .saturating_add(PENDING_GRACE_MS),
            warnings,
        });
        if delayed {
            m.delayed_until = Some(now.saturating_add(plan.delay_ms));
        } else {
            self.begin_settle(raw, now);
        }
        Ok(Outcome::Started)
    }

    /// Records the original state (always, even if nothing needs writing) and starts managing.
    fn capture(
        &mut self,
        h: HWND,
        pid: u32,
        process: &str,
        target: Rect,
        island: Option<String>,
    ) -> Result<(), ErrorBody> {
        let mut maximized = win32::is_zoomed(h);
        if maximized || win32::is_iconic(h) {
            win32::show(h, ShowCmd::Restore);
            // IsZoomed is false while iconic, and SW_RESTORE of a window minimized from
            // maximized brings it back maximized: undo that and remember it was maximized.
            if win32::is_zoomed(h) {
                maximized = true;
                win32::show(h, ShowCmd::Restore);
            }
        }
        let read = (|| -> win32::Result<(u32, u32, Rect)> {
            let (style, exstyle) = win32::get_styles(h)?;
            Ok((style, exstyle, win32::window_rect(h)?))
        })();
        let (style, exstyle, rect) = match read {
            Ok(v) => v,
            Err(e) => {
                if maximized {
                    win32::show(h, ShowCmd::Maximize);
                }
                return Err(w32(&e, pid));
            }
        };
        let raw = win32::hwnd_to_isize(h);
        self.managed.insert(
            raw,
            Managed {
                state: WindowState {
                    hwnd: raw,
                    style,
                    exstyle,
                    rect,
                    maximized,
                },
                pid,
                process: process.to_string(),
                target,
                island,
                guard: None,
                watching: false,
                delayed_until: None,
                settling: false,
                reassert_due: None,
                moving: false,
                pending: None,
                forced: None,
            },
        );
        self.persist();
        Ok(())
    }

    /// Starts a guard cycle at the current time and hooks the window for deviations.
    fn begin_settle(&mut self, raw: isize, now: u64) {
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        m.guard = Some(Guard::new(GuardConfig::default(), m.target, now));
        m.settling = true;
        m.reassert_due = None;
        m.forced = None;
        if let Some(p) = m.pending.as_mut() {
            p.started = now;
            p.first_dev = None;
            p.frame_back_seen = false;
        }
        if !m.watching {
            match watcher::watch(win32::hwnd_from_isize(raw)) {
                Ok(()) => m.watching = true,
                Err(e) => {
                    let msg = format!("{}: deviations are not watched: {e}", m.process);
                    self.note(msg);
                }
            }
        }
    }

    /// Style pass of a delayed apply; the window was already positioned.
    fn finish_delayed(&mut self, raw: isize, now: u64) {
        let h = win32::hwnd_from_isize(raw);
        let Some(m) = self.managed.get_mut(&raw) else {
            return;
        };
        m.delayed_until = None;
        let (target, pid) = (m.target, m.pid);
        if !win32::is_window(h) {
            self.prune();
            return;
        }
        match write_geometry(h, target, pid, None) {
            Ok(()) => self.begin_settle(raw, now),
            Err(e) => {
                self.note(e.message.clone());
                self.fail_pending(raw, e);
                let _ = self.restore_window(raw);
            }
        }
    }

    /// Attempts every step even when one fails, always forgets the window and returns the first
    /// error. `None`: the window was not managed. The bool is "exact": style, ex-style and rect
    /// (or the maximized state) read back equal to the original.
    fn restore_window(&mut self, raw: isize) -> Option<(bool, Result<(), ErrorBody>)> {
        let m = self.managed.remove(&raw)?;
        let h = win32::hwnd_from_isize(raw);
        watcher::unwatch(h);
        if let Some(Pending { reply: Some(r), .. }) = m.pending {
            r.fail(err(ErrorCode::Busy, "operation cancelled by a restore"));
        }
        self.persist();
        if !win32::is_window(h) {
            return Some((false, Ok(())));
        }
        let orig = m.state;
        let mut first: Option<ErrorBody> = None;
        let mut note = |r: win32::Result<()>| {
            if let Err(e) = r {
                first.get_or_insert(w32(&e, m.pid));
            }
        };
        note(win32::set_styles(h, orig.style, orig.exstyle));
        note(win32::set_pos(h, orig.rect, win32::SWP_RESTORE));
        if orig.maximized {
            win32::show(h, ShowCmd::Maximize);
        }
        let exact = win32::get_styles(h).is_ok_and(|s| s == (orig.style, orig.exstyle))
            && if orig.maximized {
                win32::is_zoomed(h)
            } else {
                win32::window_rect(h).is_ok_and(|r| r == orig.rect)
            };
        Some((exact, first.map_or(Ok(()), Err)))
    }

    /// Restores every managed window even if some fail; remembers them so they are not re-applied.
    fn restore_everything(&mut self) -> RestoreAllResult {
        self.prune();
        let raws: Vec<isize> = self.managed.keys().copied().collect();
        let mut out = RestoreAllResult {
            restored: 0,
            failed: Vec::new(),
        };
        for raw in raws {
            match self.restore_window(raw) {
                Some((_, Ok(()))) => out.restored += 1,
                Some((_, Err(e))) => {
                    eprintln!("restore {raw:#x} failed: {}", e.message);
                    out.failed.push(RestoreFailure {
                        hwnd: raw,
                        error: e.message,
                    });
                }
                None => {}
            }
            self.user_restored.insert(raw);
        }
        out
    }

    // -------------------------------------------------------------- diagnose

    fn begin_diagnose(
        &mut self,
        p: &DiagnoseParams,
        reply: &mut Option<Reply>,
    ) -> Result<(), ErrorBody> {
        let h = self.find_window(&p.window)?;
        let raw = win32::hwnd_to_isize(h);
        let pid = win32::window_pid(h);
        let process = win32::process_name_of(h).map_err(|e| w32(&e, pid))?;
        if profiles::is_excluded(&process) {
            return Err(err(
                ErrorCode::ExcludedProcess,
                format!("{process} is on the exclusion list"),
            ));
        }
        if !(win32::is_top_level(h) && win32::is_visible(h)) {
            return Err(err(
                ErrorCode::WindowNotFound,
                "not a visible top-level window",
            ));
        }
        if self.managed.contains_key(&raw) || self.diags.iter().any(|d| d.raw == raw) {
            return Err(err(
                ErrorCode::Busy,
                "the window is already managed or being diagnosed; restore it before diagnosing",
            ));
        }
        let req = PlanReq {
            island: p.island.clone(),
            monitor: p.monitor.clone(),
            offsets: None,
            delay_ms: None,
            use_profile: true,
            game_resolution: None,
        };
        let plan = self.plan_for(h, req)?;
        let (target, _) = self.resolve_target(h, &plan)?;
        let observe_ms = p.observe_ms.clamp(100, ipc::MAX_OBSERVE_MS);
        let Some(owed) = reply.take() else {
            return Err(err(ErrorCode::Internal, "diagnose without a reply channel"));
        };
        let island = canonical_island(&self.layouts(), plan.island.clone());
        if let Err(e) = self.capture(h, pid, &process, target, island) {
            *reply = Some(owed);
            return Err(e);
        }
        // Marks and samples share one origin: the clock starts just before the sampler.
        let t0 = Instant::now();
        let sampler = diagnose::start_sampler(raw, observe_ms);
        let mut marks = Marks {
            t0,
            list: Vec::new(),
        };
        if let Err(e) = write_geometry(h, target, pid, Some(&mut marks)) {
            let _ = self.restore_window(raw);
            *reply = Some(owed);
            return Err(e);
        }
        let deadline = self.now() + observe_ms + DIAG_MARGIN_MS;
        self.diags.push(Diag {
            raw,
            process,
            target,
            deadline,
            sampler,
            marks,
            keep: p.keep,
            reply: owed,
        });
        Ok(())
    }

    fn finish_diag(&mut self, index: usize) {
        let d = self.diags.remove(index);
        let samples = d.sampler.finish();
        let h = win32::hwnd_from_isize(d.raw);
        let gone = !win32::is_window(h);
        let fin = win32::window_rect(h)
            .ok()
            .or_else(|| samples.last().map(|s| s.rect))
            .unwrap_or(d.target);
        let awareness = win32::dpi_awareness_of(h);
        let awareness_name = serde_json::to_value(awareness)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        let analysis = diagnose::analyse(
            d.target,
            &d.marks.list,
            &samples,
            &awareness_name,
            win32::monitor_dpi_at(d.target),
            &d.process,
        );
        let restored = !d.keep && !gone && matches!(self.restore_window(d.raw), Some((_, Ok(()))));
        if d.keep || gone {
            self.prune();
        }
        d.reply.send(Ok(DiagnoseResult {
            hwnd: d.raw,
            process: d.process,
            target: d.target,
            verdict: analysis.verdict,
            timeline: build_timeline(&d.marks.list, &samples),
            r#final: fin,
            forced_size: analysis.forced_size,
            revert_latency_ms: analysis.revert_latency_ms,
            dpi_awareness: awareness,
            recommendations: analysis.recommendations,
            restored,
        }));
    }

    // -------------------------------------------------------------- IPC

    fn find_window(&self, r: &WindowRef) -> Result<HWND, ErrorBody> {
        if let Some(raw) = r.hwnd {
            let h = win32::hwnd_from_isize(raw);
            return if win32::is_window(h) {
                Ok(h)
            } else {
                Err(window_not_found(raw))
            };
        }
        if r.process.is_none() && r.title.is_none() {
            return Err(err(
                ErrorCode::InvalidParams,
                "window needs hwnd, or process and/or title",
            ));
        }
        let process = r.process.as_deref().map(base_name);
        let title = r.title.as_deref().map(str::to_lowercase);
        let mut hits: Vec<(HWND, String, String)> = Vec::new();
        for h in watcher::initial_scan() {
            let name = win32::process_name_of(h).unwrap_or_default();
            if process.is_some_and(|p| !name.eq_ignore_ascii_case(p)) {
                continue;
            }
            let t = win32::window_title(h);
            if title
                .as_ref()
                .is_some_and(|q| !t.to_lowercase().contains(q))
            {
                continue;
            }
            hits.push((h, name, t));
        }
        if hits.len() > 1 && hits.iter().any(|(_, _, t)| !t.is_empty()) {
            hits.retain(|(_, _, t)| !t.is_empty());
        }
        match hits.as_slice() {
            [] => Err(err(ErrorCode::WindowNotFound, "no window matches")),
            [(h, _, _)] => Ok(*h),
            many => Err(err(
                ErrorCode::AmbiguousWindow,
                format!("{} windows match; give an hwnd or a more specific title", many.len()),
            )
            .with_data(json!({
                "candidates": many
                    .iter()
                    .map(|(h, p, t)| json!({ "hwnd": win32::hwnd_to_isize(*h), "process": p, "title": t }))
                    .collect::<Vec<_>>()
            }))),
        }
    }

    fn handle_ipc(&mut self, req: Request, tx: oneshot::Sender<Response>) {
        self.prune();
        let Request { id, method, params } = req;
        let reply = Reply { id, tx };
        match method.as_str() {
            "hello" => match ipc::parse_params::<HelloParams>(params) {
                Ok(_) => reply.send(Ok(self.hello())),
                Err(e) => reply.fail(e),
            },
            "list_windows" => {
                reply.send(ipc::parse_params(params).and_then(|p| self.list_windows(&p)))
            }
            "apply" => match ipc::parse_params::<ApplyParams>(params) {
                Ok(p) => {
                    let req = PlanReq {
                        island: p.island,
                        monitor: p.monitor,
                        offsets: p.offsets,
                        delay_ms: p.delay_ms,
                        use_profile: p.use_profile,
                        game_resolution: None,
                    };
                    self.apply_request(&p.window, req, reply);
                }
                Err(e) => reply.fail(e),
            },
            "place" => match ipc::parse_params::<PlaceParams>(params) {
                Ok(p) => {
                    let req = PlanReq {
                        island: Some(p.island),
                        monitor: None,
                        offsets: None,
                        delay_ms: None,
                        use_profile: false,
                        game_resolution: p.game_resolution,
                    };
                    self.apply_request(&p.window, req, reply);
                }
                Err(e) => reply.fail(e),
            },
            "restore" => {
                reply.send(ipc::parse_params(params).and_then(|p| self.restore_request(&p)))
            }
            "restore_all" => match ipc::parse_params::<RestoreAllParams>(params) {
                Ok(_) => reply.send(Ok(self.restore_everything())),
                Err(e) => reply.fail(e),
            },
            "list_islands" => match ipc::parse_params::<ListIslandsParams>(params) {
                Ok(_) => reply.send(self.list_islands()),
                Err(e) => reply.fail(e),
            },
            "get_profiles" => match ipc::parse_params::<GetProfilesParams>(params) {
                Ok(_) => reply.send(self.get_profiles()),
                Err(e) => reply.fail(e),
            },
            "set_profiles" => {
                reply.send(ipc::parse_params(params).and_then(|p| self.set_profiles(p)))
            }
            "diagnose" => match ipc::parse_params::<DiagnoseParams>(params) {
                Ok(p) => {
                    let mut slot = Some(reply);
                    if let Err(e) = self.begin_diagnose(&p, &mut slot)
                        && let Some(r) = slot
                    {
                        r.fail(e);
                    }
                }
                Err(e) => reply.fail(e),
            },
            other => reply.fail(err(
                ErrorCode::UnknownMethod,
                format!("unknown method: {other}"),
            )),
        }
    }

    fn hello(&self) -> HelloResult {
        HelloResult {
            protocol: ipc::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            profiles_revision: self.profiles_revision.clone(),
        }
    }

    fn apply_request(&mut self, window: &WindowRef, req: PlanReq, reply: Reply) {
        let mut slot = Some(reply);
        let res = self.find_window(window).and_then(|h| {
            let plan = self.plan_for(h, req)?;
            self.user_restored.remove(&win32::hwnd_to_isize(h));
            self.apply_window(h, &plan, &mut slot)
        });
        // `Started` has already taken the reply; everything else is answered here.
        if let Some(reply) = slot {
            match res {
                Ok(Outcome::Done(r)) => reply.send(Ok(r)),
                Ok(Outcome::Started) => reply.fail(err(ErrorCode::Internal, "reply lost")),
                Err(e) => reply.fail(e),
            }
        }
    }

    fn restore_request(&mut self, p: &RestoreParams) -> Result<RestoreResult, ErrorBody> {
        // Pruned first: a managed entry whose hwnd now belongs to another process (reused
        // handle) must never get the dead window's state; a live window is looked up as usual.
        self.prune();
        let raw = match p.window.hwnd.filter(|r| self.managed.contains_key(r)) {
            Some(r) => r,
            None => win32::hwnd_to_isize(self.find_window(&p.window)?),
        };
        match self.restore_window(raw) {
            None => Ok(RestoreResult {
                restored: false,
                exact: false,
                hwnd: raw,
            }),
            Some((exact, result)) => {
                self.user_restored.insert(raw);
                result?;
                Ok(RestoreResult {
                    restored: true,
                    exact,
                    hwnd: raw,
                })
            }
        }
    }

    fn list_windows(&self, p: &ListWindowsParams) -> Result<ListWindowsResult, ErrorBody> {
        let monitors = win32::monitors().map_err(|e| w32(&e, 0))?;
        let handles = win32::top_level_windows().map_err(|e| w32(&e, 0))?;
        let mut windows = Vec::new();
        for h in handles {
            if !win32::is_top_level(h) || (!p.include_hidden && !win32::is_visible(h)) {
                continue;
            }
            let raw = win32::hwnd_to_isize(h);
            let managed = self.managed.get(&raw);
            if p.only_managed && managed.is_none() {
                continue;
            }
            let process = win32::process_name_of(h).unwrap_or_default();
            if p.process
                .as_deref()
                .is_some_and(|f| !process.eq_ignore_ascii_case(base_name(f)))
            {
                continue;
            }
            let (Ok((style, exstyle)), Ok(rect)) = (win32::get_styles(h), win32::window_rect(h))
            else {
                continue;
            };
            let (w, hh) = win32::client_size(h).unwrap_or((0, 0));
            let monitor = win32::monitor_rect_of(h)
                .ok()
                .and_then(|r| monitors.iter().find(|m| m.rect == r));
            let pid = win32::window_pid(h);
            windows.push(WindowInfo {
                hwnd: raw,
                pid,
                process,
                title: win32::window_title(h),
                class: win32::window_class(h),
                visible: win32::is_visible(h),
                style: ipc::hex(style),
                exstyle: ipc::hex(exstyle),
                rect,
                client: ClientSize { w, h: hh },
                monitor: WindowMonitor {
                    index: monitor.map_or(0, |m| m.index),
                    device: monitor.map(|m| m.device.clone()).unwrap_or_default(),
                },
                dpi: win32::dpi_of(h),
                dpi_awareness: win32::dpi_awareness_of(h),
                elevated: win32::is_elevated(pid),
                maximized: win32::is_zoomed(h),
                minimized: win32::is_iconic(h),
                managed: managed.is_some(),
                island: managed.and_then(|m| m.island.clone()),
            });
        }
        Ok(ListWindowsResult { windows })
    }

    /// Layouts whose monitor is not connected are left out: they cannot be resolved to rects.
    fn list_islands(&self) -> Result<ListIslandsResult, ErrorBody> {
        let monitors = win32::monitors().map_err(|e| w32(&e, 0))?;
        let mut layouts = Vec::new();
        let all = islands::merged_layouts(&self.profiles.layouts);
        for l in &all {
            let Ok(mon) = islands::select_monitor(&l.monitor, &monitors) else {
                continue;
            };
            let islands = l
                .islands
                .iter()
                .map(|i| {
                    let reference = format!("{}/{}", l.id, i.id);
                    let mut occupants: Vec<isize> = self
                        .managed
                        .iter()
                        .filter(|(_, m)| {
                            m.island
                                .as_deref()
                                .and_then(|r| islands::canonical_ref(&all, r))
                                .is_some_and(|c| c == reference)
                        })
                        .map(|(&raw, _)| raw)
                        .collect();
                    occupants.sort_unstable();
                    IslandInfo {
                        id: i.id.clone(),
                        rect: islands::island_rect_on(mon.rect, &i.rect),
                        occupants,
                    }
                })
                .collect();
            layouts.push(LayoutInfo {
                id: l.id.clone(),
                monitor: l.monitor.clone(),
                islands,
            });
        }
        Ok(ListIslandsResult { monitors, layouts })
    }

    fn profiles_path(&self) -> Result<PathBuf, ErrorBody> {
        self.profiles_path
            .clone()
            .ok_or_else(|| err(ErrorCode::ProfileError, "the APPDATA variable is not set"))
    }

    /// Reads the file from disk (so `revision` is its real hash) and adopts it.
    fn get_profiles(&mut self) -> Result<GetProfilesResult, ErrorBody> {
        let path = self.profiles_path()?;
        let (file, revision) = profiles::load(&path).map_err(profile_err)?;
        let result = GetProfilesResult {
            path: path.display().to_string(),
            revision: revision.clone(),
            version: file.version,
            profiles: file.profiles.clone(),
            layouts: file.layouts.clone(),
            warnings: profiles::validate(&file),
        };
        if revision != self.profiles_revision {
            self.menu_dirty = true;
        }
        self.profiles = file;
        self.profiles_revision = revision;
        Ok(result)
    }

    fn set_profiles(&mut self, p: SetProfilesParams) -> Result<SetProfilesResult, ErrorBody> {
        let (file, result) = Self::apply_set_profiles(&self.profiles_path()?, p)?;
        self.profiles = file;
        self.profiles_revision = result.revision.clone();
        self.menu_dirty = true;
        Ok(result)
    }

    /// The file side of `set_profiles`: load → revision check → upsert → validate → save.
    fn apply_set_profiles(
        path: &Path,
        p: SetProfilesParams,
    ) -> Result<(ProfileFile, SetProfilesResult), ErrorBody> {
        // An invalid file on disk is reported, never overwritten: the user can fix a hand edit.
        let (mut file, revision) = profiles::load(path).map_err(profile_err)?;
        if p.expected_revision
            .as_ref()
            .is_some_and(|exp| *exp != revision)
        {
            return Err(profile_err(ProfileError::RevisionConflict));
        }
        let upserted: HashSet<String> = p
            .profiles
            .iter()
            .flatten()
            .map(|x| base_name(&x.process).to_ascii_lowercase())
            .collect();
        let upserted_layouts: HashSet<String> =
            p.layouts.iter().flatten().map(|l| l.id.clone()).collect();
        profiles::upsert(
            &mut file,
            p.profiles.unwrap_or_default(),
            p.layouts.unwrap_or_default(),
            p.remove.unwrap_or_default(),
            p.remove_layouts,
            p.replace_extras,
        );
        let warnings = profiles::validate(&file);
        // Invalid combinations in what this call wrote are rejected; older findings stay warnings.
        let rejected: Vec<&Warning> = warnings
            .iter()
            .filter(|w| {
                profiles::is_blocking(&w.code)
                    && match w.data.get("process").and_then(Value::as_str) {
                        Some(n) => upserted.contains(&base_name(n).to_ascii_lowercase()),
                        None => w
                            .data
                            .get("layout")
                            .and_then(Value::as_str)
                            .is_some_and(|l| upserted_layouts.contains(l)),
                    }
            })
            .collect();
        if !rejected.is_empty() {
            return Err(
                err(ErrorCode::ProfileError, "profiles rejected by validation")
                    .with_data(json!({ "warnings": rejected })),
            );
        }
        profiles::save_atomic(path, &file).map_err(profile_err)?;
        let bytes = std::fs::read(path).map_err(|e| profile_err(e.into()))?;
        let revision = profiles::revision(&bytes);
        Ok((file, SetProfilesResult { revision, warnings }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{MonitorChoice, Profile};

    const fn r(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn warnings_size_position_style() {
        let target = r(2560, 0, 5120, 1440);
        assert!(outcome_warnings(target, target, false, None).is_empty());
        let w = outcome_warnings(target, r(0, 0, 1280, 720), false, Some(60));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].code, "size_forced");
        assert_eq!(w[0].data, json!({ "w": 1280, "h": 720, "after_ms": 60 }));
        let w = outcome_warnings(target, r(100, 0, 2660, 1440), true, None);
        assert_eq!(
            w.iter().map(|x| x.code.as_str()).collect::<Vec<_>>(),
            ["position_forced", "style_reverted"]
        );
    }

    #[test]
    fn timeline_dedups_samples_and_orders_marks() {
        let s = |t_ms, left| Sample {
            t_ms,
            style: 0x1000_0000,
            exstyle: 0,
            rect: r(left, 0, left + 10, 10),
        };
        let samples = [s(0.0, 0), s(1.0, 0), s(2.0, 5), s(3.0, 5), s(4.0, 0)];
        let marks = [WriteMark {
            t_ms: 0.5,
            op: "set_pos".into(),
        }];
        let t = build_timeline(&marks, &samples);
        let kinds: Vec<_> = t.iter().map(|e| (e.t_ms, e.source)).collect();
        assert_eq!(
            kinds,
            [
                (0.0, TimelineSource::Window),
                (0.5, TimelineSource::Us),
                (2.0, TimelineSource::Window),
                (4.0, TimelineSource::Window),
            ]
        );
        assert_eq!(t[1].op.as_deref(), Some("set_pos"));
        assert_eq!(t[1].style, "0x10000000");
    }

    #[test]
    fn base_name_strips_directories() {
        assert_eq!(base_name("C:\\Games\\a.exe"), "a.exe");
        assert_eq!(base_name("a.exe"), "a.exe");
    }

    fn profile(process: &str) -> Profile {
        Profile {
            process: process.into(),
            enabled: true,
            monitor: MonitorChoice::Current,
            offsets: Offsets::default(),
            delay_ms: 0,
            clip_cursor: false,
            island: None,
            game_resolution: None,
            extra: Default::default(),
        }
    }

    fn file(profiles: Vec<Profile>) -> ProfileFile {
        ProfileFile {
            version: profiles::SUPPORTED_VERSION,
            layouts: vec![],
            profiles,
            extra: Default::default(),
        }
    }

    fn req() -> PlanReq {
        PlanReq {
            island: None,
            monitor: None,
            offsets: None,
            delay_ms: None,
            use_profile: true,
            game_resolution: None,
        }
    }

    #[test]
    fn plan_without_profile_or_params_uses_current_monitor() {
        let plan = build_plan(&file(vec![]), "PWAAT.exe", req()).unwrap();
        assert_eq!(plan.island, None);
        assert_eq!(plan.monitor, None);
        assert_eq!(plan.offsets, Offsets::default());
        assert!(plan.lenient);
        assert_eq!(plan.delay_ms, 0);
    }

    #[test]
    fn plan_profile_island_overrides_its_monitor_and_offsets() {
        let mut p = profile("pwaat.exe");
        p.island = Some("l/i".into());
        p.monitor = MonitorChoice::Index(2);
        p.offsets.left = 10;
        let plan = build_plan(&file(vec![p]), "C:\\Games\\PWAAT.exe", req()).unwrap();
        assert_eq!(plan.island.as_deref(), Some("l/i"));
        assert_eq!(plan.monitor, None);
        assert_eq!(plan.offsets, Offsets::default());
    }

    #[test]
    fn plan_profile_monitor_and_offsets_apply_without_island() {
        let mut p = profile("PWAAT.exe");
        p.monitor = MonitorChoice::Index(1);
        p.offsets.top = 5;
        p.delay_ms = 250;
        let plan = build_plan(&file(vec![p]), "PWAAT.exe", req()).unwrap();
        assert_eq!(plan.monitor, Some(MonitorSel::Index(1)));
        assert_eq!(plan.offsets.top, 5);
        assert_eq!(plan.delay_ms, 250);
    }

    #[test]
    fn plan_request_island_beats_profile_monitor() {
        let mut p = profile("PWAAT.exe");
        p.monitor = MonitorChoice::Index(1);
        let mut rq = req();
        rq.island = Some("a/b".into());
        let plan = build_plan(&file(vec![p]), "PWAAT.exe", rq).unwrap();
        assert_eq!(plan.island.as_deref(), Some("a/b"));
        assert_eq!(plan.monitor, None);
    }

    #[test]
    fn plan_request_monitor_beats_profile_island() {
        let mut p = profile("PWAAT.exe");
        p.island = Some("l/i".into());
        let mut rq = req();
        rq.monitor = Some(MonitorSel::Primary);
        let plan = build_plan(&file(vec![p]), "PWAAT.exe", rq).unwrap();
        assert_eq!(plan.island, None);
        assert_eq!(plan.monitor, Some(MonitorSel::Primary));
        assert!(!plan.lenient);
    }

    #[test]
    fn plan_use_profile_false_ignores_profile() {
        let mut p = profile("PWAAT.exe");
        p.island = Some("l/i".into());
        p.delay_ms = 99;
        let mut rq = req();
        rq.use_profile = false;
        let plan = build_plan(&file(vec![p]), "PWAAT.exe", rq).unwrap();
        assert_eq!(plan.island, None);
        assert_eq!(plan.delay_ms, 0);
    }

    #[test]
    fn plan_rejects_out_of_range_request_values() {
        let mut rq = req();
        rq.delay_ms = Some(u64::MAX);
        assert_eq!(
            build_plan(&file(vec![]), "a.exe", rq).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        let mut rq = req();
        rq.offsets = Some(Offsets {
            left: i32::MAX,
            ..Offsets::default()
        });
        assert_eq!(
            build_plan(&file(vec![]), "a.exe", rq).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        let mut rq = req();
        rq.delay_ms = Some(MAX_DELAY_MS);
        assert!(build_plan(&file(vec![]), "a.exe", rq).is_ok());
    }

    #[test]
    fn plan_clamps_profile_delay() {
        let mut p = profile("a.exe");
        p.delay_ms = u64::MAX;
        let plan = build_plan(&file(vec![p]), "a.exe", req()).unwrap();
        assert_eq!(plan.delay_ms, MAX_DELAY_MS);
    }

    #[test]
    fn win32_code_unwraps_facility_7_only() {
        assert_eq!(win32_code(0x8007_0005_u32 as i32), 5);
        assert_eq!(win32_code(0x8000_4005_u32 as i32), 0x8000_4005);
    }

    #[test]
    fn reply_to_a_vanished_client_is_ignored() {
        let (tx, rx) = oneshot::channel();
        drop(rx);
        Reply { id: Id::Num(1), tx }.send(Ok(json!({})));
    }

    #[test]
    fn island_refs_are_stored_canonical() {
        let presets = islands::merged_layouts(&[]);
        assert_eq!(
            canonical_island(&presets, Some("mitades-16-9/derecha".into())), // legacy alias test
            Some("halves-16-9/right".into())
        );
        assert_eq!(
            canonical_island(&presets, Some("halves-16-9/left".into())),
            Some("halves-16-9/left".into())
        );
        // A ref that no longer resolves is kept as stored, so the window stays restorable.
        assert_eq!(
            canonical_island(&presets, Some("gone/x".into())),
            Some("gone/x".into())
        );
        assert_eq!(canonical_island(&presets, None), None);
    }

    #[test]
    fn auto_candidate_style_skips_menus_tooltips_and_untitled_popups() {
        const WS_POPUP: u32 = 0x8000_0000;
        const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
        const WS_EX_TOPMOST: u32 = 0x8;
        // Normal framed top-level window, with or without a title.
        assert!(auto_candidate_style(WS_OVERLAPPEDWINDOW, 0, true));
        assert!(auto_candidate_style(WS_OVERLAPPEDWINDOW, 0, false));
        // A game that is already borderless but has a title.
        assert!(auto_candidate_style(WS_POPUP, 0, true));
        // Menu (#32768-like) and tooltip: popup tool windows without a title.
        assert!(!auto_candidate_style(
            WS_POPUP,
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            false
        ));
        assert!(!auto_candidate_style(WS_POPUP, WS_EX_TOOLWINDOW, true));
        // Untitled popup (splash, drop-down).
        assert!(!auto_candidate_style(WS_POPUP, 0, false));
    }

    #[test]
    fn win32_error_message_is_english_only() {
        let e = Win32Error::Call {
            call: "SetWindowPos",
            source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(2)),
        };
        let body = w32(&e, 0);
        assert_eq!(body.message, "SetWindowPos failed (os error 2)");
        assert_eq!(
            body.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!(2))
        );
    }

    #[test]
    fn profile_io_error_message_is_english_only() {
        let e = ProfileError::Io(std::io::Error::from_raw_os_error(5));
        assert_eq!(profile_err(e).message, "I/O: os error 5");
        // Errors that do not come from the OS keep their (English) Rust text.
        let e = ProfileError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ));
        assert_eq!(
            profile_err(e).message,
            "I/O: stream did not contain valid UTF-8"
        );
    }

    fn temp_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bs-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(hwnd: isize) -> StoredEntry {
        StoredEntry {
            hwnd,
            pid: 1,
            process: "a.exe".into(),
            state: WindowState {
                hwnd,
                style: 0,
                exstyle: 0,
                rect: r(0, 0, 10, 10),
                maximized: false,
            },
            target: None,
            island: None,
        }
    }

    #[test]
    fn unreadable_state_is_moved_aside_before_it_is_overwritten() {
        let dir = temp_state_dir("aside");
        let path = dir.join("state.json");
        let bad = dir.join("state.json.bad");
        std::fs::write(&bad, "older bad copy").unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let store = StateStore::open(path.clone());
        let mut unreadable = Some(path.clone());
        save_state(&store, &mut unreadable, &[entry(7)]).unwrap();
        assert_eq!(unreadable, None);
        // The newest bad copy replaces the older one, and the new state is written.
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{ not json");
        assert_eq!(store.load().unwrap(), [entry(7)]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_state_that_cannot_be_moved_is_never_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_state_dir("locked");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ unreadable").unwrap();
        let store = StateStore::open(path.clone());
        let mut unreadable = Some(path.clone());
        {
            // Share mode 0: no other open, rename or delete succeeds while this handle lives.
            let _lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap();
            assert!(save_state(&store, &mut unreadable, &[entry(7)]).is_err());
            assert_eq!(unreadable.as_deref(), Some(path.as_path()));
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ unreadable");
        assert!(!dir.join("state.json.bad").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readable_state_is_saved_without_a_bad_copy() {
        let dir = temp_state_dir("plain");
        let store = StateStore::open(dir.join("state.json"));
        let mut unreadable = None;
        save_state(&store, &mut unreadable, &[entry(3)]).unwrap();
        assert_eq!(store.load().unwrap(), [entry(3)]);
        assert!(!dir.join("state.json.bad").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Runs one request through `handle_ipc` and returns the reply.
    fn ipc_reply(e: &mut Engine, method: &str, params: Value) -> Response {
        let (tx, mut rx) = oneshot::channel();
        e.handle_ipc(
            Request {
                id: Id::Num(1),
                method: method.into(),
                params,
            },
            tx,
        );
        rx.try_recv().expect("reply sent synchronously")
    }

    #[test]
    fn paramless_methods_reject_non_object_params() {
        let mut e = bare_engine();
        for method in ["hello", "restore_all", "list_islands", "get_profiles"] {
            for params in [json!([1, [2]]), json!([]), json!(5), json!("x")] {
                let resp = ipc_reply(&mut e, method, params.clone());
                let body = resp
                    .error
                    .unwrap_or_else(|| panic!("{method} {params} accepted"));
                assert_eq!(body.code, ErrorCode::InvalidParams, "{method} {params}");
            }
        }
    }

    #[test]
    fn hello_accepts_null_and_object_params() {
        let mut e = bare_engine();
        for params in [Value::Null, json!({}), json!({"extra": [1]})] {
            assert!(ipc_reply(&mut e, "hello", params).error.is_none());
        }
    }

    /// An engine with no store, profiles or timer, for logic that only needs `managed`.
    fn bare_engine() -> Engine {
        let (_tx, ipc_rx) = mpsc::channel();
        Engine {
            msg_hwnd: HWND::default(),
            ipc_rx,
            t0: Instant::now(),
            profiles: empty_profiles(),
            profiles_path: None,
            profiles_revision: String::new(),
            store: None,
            state_unreadable: None,
            managed: HashMap::new(),
            diags: Vec::new(),
            user_restored: HashSet::new(),
            last_foreground: 0,
            tick_armed: false,
            status: None,
            menu_dirty: false,
        }
    }

    #[test]
    fn restore_never_applies_a_dead_windows_state_to_a_reused_hwnd() {
        unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            win32::def_window_proc(h, m, w, l)
        }
        // Message-only (HWND_MESSAGE): never shown, owned by this test process.
        let h = win32::create_message_window(proc).unwrap();
        let raw = win32::hwnd_to_isize(h);
        let before = win32::get_styles(h).unwrap();
        let mut engine = bare_engine();
        // Recorded for another process: the hwnd now belongs to a different window.
        let pid = win32::window_pid(h) + 1;
        engine.managed.insert(
            raw,
            Managed {
                state: WindowState {
                    hwnd: raw,
                    style: 0x00CF_0000,
                    exstyle: 0,
                    rect: r(0, 0, 640, 480),
                    maximized: false,
                },
                pid,
                process: "gone.exe".into(),
                target: r(0, 0, 640, 480),
                island: None,
                guard: None,
                watching: false,
                delayed_until: None,
                settling: false,
                reassert_due: None,
                moving: false,
                pending: None,
                forced: None,
            },
        );
        let res = engine
            .restore_request(&RestoreParams {
                window: WindowRef {
                    hwnd: Some(raw),
                    ..WindowRef::default()
                },
            })
            .unwrap();
        assert!(!res.restored);
        assert!(engine.managed.is_empty());
        assert_eq!(win32::get_styles(h).unwrap(), before);
    }

    const SET_PROFILES_FILE: &str = r#"{"version":2,
      "layouts":[
        {"id":"l","monitor":0,"_l":1,"islands":[
          {"id":"i","_i":1,"rect":{"x":0,"y":0,"w":0.5,"h":1}},
          {"id":"j","_j":1,"rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]},
        {"id":"u","monitor":0,"_u":1,"islands":[
          {"id":"i","_ui":1,"rect":{"x":0,"y":0,"w":1,"h":1}}]}],
      "profiles":[{"process":"a.exe","_p":1},{"process":"b.exe","_b":1}]}"#;

    /// IPC params text → `parse_params` → `apply_set_profiles` on a `SET_PROFILES_FILE` copy;
    /// returns the call's result and the file bytes afterwards.
    fn set_profiles_call(
        tag: &str,
        params: &str,
    ) -> (Result<SetProfilesResult, ErrorBody>, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("bs-engine-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.json");
        std::fs::write(&path, SET_PROFILES_FILE).unwrap();
        let p: SetProfilesParams =
            crate::ipc::parse_params(serde_json::from_str(params).unwrap()).unwrap();
        let res = Engine::apply_set_profiles(&path, p).map(|(_, r)| r);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        (res, bytes)
    }

    /// IPC params text → `parse_params` → `apply_set_profiles` → the saved file as JSON.
    fn set_profiles_saved(tag: &str, params: &str) -> Value {
        let (res, bytes) = set_profiles_call(tag, params);
        res.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn set_profiles_stale_expected_revision_is_a_conflict_and_leaves_the_file() {
        let (res, bytes) = set_profiles_call(
            "stale",
            r#"{"expected_revision":"stale","profiles":[{"process":"c.exe"}]}"#,
        );
        assert_eq!(res.unwrap_err().code, ErrorCode::RevisionConflict);
        assert_eq!(bytes, SET_PROFILES_FILE.as_bytes());
    }

    #[test]
    fn set_profiles_blocking_warning_is_rejected_and_leaves_the_file() {
        let (res, bytes) = set_profiles_call(
            "reject",
            r#"{"layouts":[{"id":"l","monitor":0,"islands":[
                {"id":"i","rect":{"x":0,"y":0,"w":0.5,"h":1}},
                {"id":"i","rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}]}"#,
        );
        let e = res.unwrap_err();
        assert_eq!(e.code, ErrorCode::ProfileError);
        let data = e.data.unwrap();
        let codes: Vec<&str> = data["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w["code"].as_str())
            .collect();
        assert_eq!(codes, ["duplicate_island"]);
        assert_eq!(bytes, SET_PROFILES_FILE.as_bytes());
    }

    #[test]
    fn set_profiles_returns_the_revision_of_the_saved_file() {
        let before = profiles::revision(SET_PROFILES_FILE.as_bytes());
        let (res, bytes) = set_profiles_call(
            "revision",
            &format!(r#"{{"expected_revision":"{before}","profiles":[{{"process":"c.exe"}}]}}"#),
        );
        let revision = res.unwrap().revision;
        assert_ne!(revision, before);
        assert_eq!(revision, profiles::revision(&bytes));
    }

    #[test]
    fn set_profiles_replace_extras_stores_sent_items_as_sent() {
        let v = set_profiles_saved(
            "replace",
            r#"{"replace_extras":true,
              "layouts":[{"id":"l","monitor":0,"islands":[
                {"id":"i","rect":{"x":0,"y":0,"w":0.5,"h":1}},
                {"id":"j","rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}],
              "profiles":[{"process":"a.exe"}]}"#,
        );
        assert!(v["layouts"][0].get("_l").is_none());
        assert!(v["layouts"][0]["islands"][0].get("_i").is_none());
        assert!(v["layouts"][0]["islands"][1].get("_j").is_none());
        assert!(v["profiles"][0].get("_p").is_none());
        assert_eq!(v["layouts"][1]["_u"], 1);
        assert_eq!(v["layouts"][1]["islands"][0]["_ui"], 1);
        assert_eq!(v["profiles"][1]["_b"], 1);
    }

    #[test]
    fn set_profiles_without_replace_extras_carries_extras_over() {
        let v = set_profiles_saved(
            "carry",
            r#"{"layouts":[{"id":"l","monitor":0,"islands":[
                {"id":"i","rect":{"x":0,"y":0,"w":0.5,"h":1}},
                {"id":"j","rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}],
              "profiles":[{"process":"a.exe"}]}"#,
        );
        assert_eq!(v["layouts"][0]["_l"], 1);
        assert_eq!(v["layouts"][0]["islands"][0]["_i"], 1);
        assert_eq!(v["layouts"][0]["islands"][1]["_j"], 1);
        assert_eq!(v["profiles"][0]["_p"], 1);
        assert_eq!(v["layouts"][1]["_u"], 1);
        assert_eq!(v["profiles"][1]["_b"], 1);
    }
}
