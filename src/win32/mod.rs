//! Thin wrappers over the `windows` crate for the app's Win32 calls. Where a wrapper exists, call it
//! instead of the raw API. Modules outside this one that hold their own `unsafe` sites are listed under
//! Unsafe in `AGENTS.md`.

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, LPARAM, LRESULT, RECT,
    SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MONITORINFOEXW, MonitorFromRect, MonitorFromWindow,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetDpiForMonitor, GetDpiForWindow,
    GetWindowDpiAwarenessContext, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, EnumWindows, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GetAncestor,
    GetClassNameW, GetClientRect, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, HWND_MESSAGE, HWND_TOP, IsIconic, IsWindow, IsWindowVisible,
    IsZoomed, KillTimer, MONITORINFOF_PRIMARY, PostMessageW, RegisterClassW, SET_WINDOW_POS_FLAGS,
    SW_MAXIMIZE, SW_RESTORE, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
};
use windows::core::{BOOL, PWSTR, w};

pub use windows::Win32::Foundation::HWND;

use crate::window_state::Rect;

/// Window procedure of the message-only window.
pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

#[derive(Debug, thiserror::Error)]
pub enum Win32Error {
    #[error("{call} failed: {source}")]
    Call {
        call: &'static str,
        #[source]
        source: windows::core::Error,
    },
}

pub type Result<T, E = Win32Error> = std::result::Result<T, E>;

/// FRAMECHANGED | SHOWWINDOW | NOOWNERZORDER | NOSENDCHANGING
pub const SWP_APPLY: u32 = 0x0660;
/// FRAMECHANGED | NOZORDER | NOACTIVATE
pub const SWP_RESTORE: u32 = 0x0034;

pub enum ShowCmd {
    Restore,
    Maximize,
}

fn call_err(call: &'static str, source: windows::core::Error) -> Win32Error {
    Win32Error::Call { call, source }
}

pub fn hwnd_to_isize(h: HWND) -> isize {
    h.0 as isize
}

pub fn hwnd_from_isize(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

/// Reads one window long; a zero result is only an error if the thread's last error is set.
fn get_long(
    h: HWND,
    index: windows::Win32::UI::WindowsAndMessaging::WINDOW_LONG_PTR_INDEX,
    call: &'static str,
) -> Result<u32> {
    // SAFETY: plain FFI calls; `h` may be invalid, which the API reports via last error.
    let (value, err) = unsafe {
        SetLastError(WIN32_ERROR(0));
        let v = GetWindowLongPtrW(h, index);
        (v, GetLastError())
    };
    if value == 0 && err.0 != 0 {
        return Err(call_err(call, windows::core::Error::from(err.to_hresult())));
    }
    Ok(value as u32)
}

pub fn get_styles(h: HWND) -> Result<(u32, u32)> {
    Ok((
        get_long(h, GWL_STYLE, "GetWindowLongPtrW(GWL_STYLE)")?,
        get_long(h, GWL_EXSTYLE, "GetWindowLongPtrW(GWL_EXSTYLE)")?,
    ))
}

fn set_long(
    h: HWND,
    index: windows::Win32::UI::WindowsAndMessaging::WINDOW_LONG_PTR_INDEX,
    value: u32,
    call: &'static str,
) -> Result<()> {
    // SAFETY: plain FFI calls; failure is signalled by a zero return plus last error.
    let (prev, err) = unsafe {
        SetLastError(WIN32_ERROR(0));
        let p = SetWindowLongPtrW(h, index, value as isize);
        (p, GetLastError())
    };
    if prev == 0 && err.0 != 0 {
        return Err(call_err(call, windows::core::Error::from(err.to_hresult())));
    }
    Ok(())
}

pub fn set_styles(h: HWND, style: u32, exstyle: u32) -> Result<()> {
    set_long(h, GWL_STYLE, style, "SetWindowLongPtrW(GWL_STYLE)")?;
    set_long(h, GWL_EXSTYLE, exstyle, "SetWindowLongPtrW(GWL_EXSTYLE)")
}

fn from_native(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

pub fn window_rect(h: HWND) -> Result<Rect> {
    let mut r = RECT::default();
    // SAFETY: `r` is a valid out pointer for the duration of the call.
    unsafe { GetWindowRect(h, &mut r) }.map_err(|e| call_err("GetWindowRect", e))?;
    Ok(from_native(r))
}

pub fn is_window(h: HWND) -> bool {
    // SAFETY: IsWindow accepts any handle value.
    unsafe { IsWindow(Some(h)) }.as_bool()
}

pub fn is_visible(h: HWND) -> bool {
    // SAFETY: IsWindowVisible accepts any handle value.
    unsafe { IsWindowVisible(h) }.as_bool()
}

pub fn is_zoomed(h: HWND) -> bool {
    // SAFETY: IsZoomed accepts any handle value.
    unsafe { IsZoomed(h) }.as_bool()
}

pub fn is_iconic(h: HWND) -> bool {
    // SAFETY: IsIconic accepts any handle value.
    unsafe { IsIconic(h) }.as_bool()
}

pub fn is_top_level(h: HWND) -> bool {
    // SAFETY: GetAncestor accepts any handle value.
    unsafe { GetAncestor(h, GA_ROOT) == h }
}

pub fn show(h: HWND, cmd: ShowCmd) {
    let c = match cmd {
        ShowCmd::Restore => SW_RESTORE,
        ShowCmd::Maximize => SW_MAXIMIZE,
    };
    // SAFETY: ShowWindow accepts any handle value; the return is the previous visibility, not an error.
    let _ = unsafe { ShowWindow(h, c) };
}

pub fn set_pos(h: HWND, r: Rect, flags: u32) -> Result<()> {
    // SAFETY: plain FFI call with by-value arguments.
    unsafe {
        SetWindowPos(
            h,
            Some(HWND_TOP),
            r.left,
            r.top,
            r.width(),
            r.height(),
            SET_WINDOW_POS_FLAGS(flags),
        )
    }
    .map_err(|e| call_err("SetWindowPos", e))
}

fn monitor_rect(m: HMONITOR) -> Result<Rect> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid MONITORINFO with cbSize set.
    if !unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
        return Err(call_err(
            "GetMonitorInfoW",
            windows::core::Error::from_thread(),
        ));
    }
    Ok(from_native(info.rcMonitor))
}

pub fn monitor_rect_of(h: HWND) -> Result<Rect> {
    // SAFETY: DEFAULTTONEAREST always returns a valid monitor.
    let m = unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) };
    monitor_rect(m)
}

unsafe extern "system" fn collect_monitor(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed by `monitors`, alive during enumeration.
    let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    list.push(m);
    BOOL(1)
}

/// Monitors in `EnumDisplayMonitors` order with GDI device name, primary flag and effective DPI.
pub fn monitors() -> Result<Vec<crate::islands::MonitorInfo>> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the pointer outlives the synchronous enumeration and matches the callback's cast.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut _ as isize),
        )
    };
    if !ok.as_bool() {
        return Err(call_err(
            "EnumDisplayMonitors",
            windows::core::Error::from_thread(),
        ));
    }
    handles
        .into_iter()
        .enumerate()
        .map(|(index, m)| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            // SAFETY: MONITORINFOEXW starts with a MONITORINFO and cbSize announces the larger struct.
            let ok =
                unsafe { GetMonitorInfoW(m, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
            if !ok.as_bool() {
                return Err(call_err(
                    "GetMonitorInfoW",
                    windows::core::Error::from_thread(),
                ));
            }
            let name_len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            let dpi = monitor_dpi(m);
            Ok(crate::islands::MonitorInfo {
                index,
                device: String::from_utf16_lossy(&info.szDevice[..name_len]),
                rect: from_native(info.monitorInfo.rcMonitor),
                dpi,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            })
        })
        .collect()
}

/// Effective DPI of monitor `m`; 96 (100 %) if it cannot be read.
fn monitor_dpi(m: HMONITOR) -> u32 {
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    // SAFETY: both out pointers are valid for the call.
    match unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } {
        Ok(()) if dpi_x != 0 => dpi_x,
        _ => 96,
    }
}

/// Effective DPI of the monitor that holds most of `r` (the nearest one if none does). Unlike
/// [`dpi_of`], this is the real scale even for a DPI-unaware window, whose own DPI reads 96.
pub fn monitor_dpi_at(r: Rect) -> u32 {
    let native = RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };
    // SAFETY: `native` is a valid RECT for the call; DEFAULTTONEAREST always returns a monitor.
    monitor_dpi(unsafe { MonitorFromRect(&native, MONITOR_DEFAULTTONEAREST) })
}

/// Process id that owns window `h` (0 if the window is gone).
pub fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

/// Reads a UTF-16 string through `read` (title/class getters); empty on failure.
fn read_text(read: impl FnOnce(&mut [u16]) -> i32) -> String {
    let mut units = vec![0u16; 512];
    let n = read(&mut units).clamp(0, units.len() as i32) as usize;
    String::from_utf16_lossy(&units[..n])
}

pub fn window_title(h: HWND) -> String {
    // SAFETY: the buffer is writable for its whole length during the call.
    read_text(|buf| unsafe { GetWindowTextW(h, buf) })
}

pub fn window_class(h: HWND) -> String {
    // SAFETY: the buffer is writable for its whole length during the call.
    read_text(|buf| unsafe { GetClassNameW(h, buf) })
}

/// Client area size in physical pixels.
pub fn client_size(h: HWND) -> Result<(i32, i32)> {
    let mut r = RECT::default();
    // SAFETY: `r` is a valid out pointer.
    unsafe { GetClientRect(h, &mut r) }.map_err(|e| call_err("GetClientRect", e))?;
    Ok((r.right - r.left, r.bottom - r.top))
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DpiAwareness {
    Unaware,
    System,
    PerMonitor,
    PerMonitorV2,
    Unknown,
}

pub fn dpi_awareness_of(h: HWND) -> DpiAwareness {
    // SAFETY: plain FFI calls with by-value arguments; a null context yields INVALID/Unknown.
    unsafe {
        let ctx = GetWindowDpiAwarenessContext(h);
        if ctx.0.is_null() {
            return DpiAwareness::Unknown;
        }
        if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool() {
            return DpiAwareness::PerMonitorV2;
        }
        match GetAwarenessFromDpiAwarenessContext(ctx) {
            a if a == DPI_AWARENESS_UNAWARE => DpiAwareness::Unaware,
            a if a == DPI_AWARENESS_SYSTEM_AWARE => DpiAwareness::System,
            a if a == DPI_AWARENESS_PER_MONITOR_AWARE => DpiAwareness::PerMonitor,
            _ => DpiAwareness::Unknown,
        }
    }
}

/// DPI of the window (0 if the window is invalid).
pub fn dpi_of(h: HWND) -> u32 {
    // SAFETY: plain FFI call with a by-value argument.
    unsafe { GetDpiForWindow(h) }
}

/// Whether process `pid` runs elevated; `None` if its token cannot be queried.
pub fn is_elevated(pid: u32) -> Option<bool> {
    // SAFETY: plain FFI call; the guard closes the handle.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; the guard below closes it.
    unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }.ok()?;
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the announced size.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    Some(elevation.TokenIsElevated != 0)
}

/// Closes the wrapped kernel handle on drop.
pub struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful Win32 call and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// File name (no directory) of the executable that owns window `h`.
pub fn process_name_of(h: HWND) -> Result<String> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    // SAFETY: plain FFI call; the guard closes the handle.
    let process = OwnedHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|e| call_err("OpenProcess", e))?,
    );
    let mut units = vec![0u16; 1024];
    let mut len = units.len() as u32;
    // SAFETY: `units` has `len` writable u16 slots and outlives the call.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(units.as_mut_ptr()),
            &mut len,
        )
    }
    .map_err(|e| call_err("QueryFullProcessImageNameW", e))?;
    let full = String::from_utf16_lossy(&units[..len as usize]);
    Ok(std::path::Path::new(&full)
        .file_name()
        .map_or_else(|| full.clone(), |n| n.to_string_lossy().into_owned()))
}

unsafe extern "system" fn collect_window(h: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<HWND>` passed by `top_level_windows`, alive during enumeration.
    let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
    list.push(h);
    BOOL(1)
}

pub fn top_level_windows() -> Result<Vec<HWND>> {
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the pointer outlives the synchronous enumeration and matches the callback's cast.
    unsafe { EnumWindows(Some(collect_window), LPARAM(&mut list as *mut _ as isize)) }
        .map_err(|e| call_err("EnumWindows", e))?;
    Ok(list)
}

/// Registers the message-only window class and creates the window on the calling thread.
pub fn create_message_window(proc: WndProc) -> Result<HWND> {
    let class = w!("BorderlessShatter.MessageWindow");
    // SAFETY: no preconditions; null name = the current executable.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| call_err("GetModuleHandleW", e))?;
    let instance = HINSTANCE(module.0);
    let wc = WNDCLASSW {
        lpfnWndProc: Some(proc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialised and the class name is a static literal.
    if unsafe { RegisterClassW(&wc) } == 0 {
        return Err(call_err(
            "RegisterClassW",
            windows::core::Error::from_thread(),
        ));
    }
    // SAFETY: the class was registered just above; HWND_MESSAGE makes a message-only window.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| call_err("CreateWindowExW", e))
}

pub fn def_window_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: forwards the arguments of the window procedure unchanged.
    unsafe { DefWindowProcW(h, msg, wparam, lparam) }
}

/// Queues `msg` on the window's own thread; unlike a thread message it is never lost in modal loops.
pub fn post_message(h: HWND, msg: u32, wparam: usize) -> Result<()> {
    post_message_lparam(h, msg, wparam, 0)
}

/// Like `post_message` but also carries an `lparam`.
pub fn post_message_lparam(h: HWND, msg: u32, wparam: usize, lparam: isize) -> Result<()> {
    // SAFETY: plain FFI call with by-value arguments.
    unsafe { PostMessageW(Some(h), msg, WPARAM(wparam), LPARAM(lparam)) }
        .map_err(|e| call_err("PostMessageW", e))
}

/// One-shot-by-convention timer: the owner kills it when `WM_TIMER` arrives.
pub fn set_timer(h: HWND, id: usize, delay_ms: u32) -> Result<()> {
    // SAFETY: plain FFI call; no timer callback is installed.
    if unsafe { SetTimer(Some(h), id, delay_ms, None) } == 0 {
        return Err(call_err("SetTimer", windows::core::Error::from_thread()));
    }
    Ok(())
}

pub fn kill_timer(h: HWND, id: usize) {
    // SAFETY: plain FFI call; killing an unknown timer only fails harmlessly.
    let _ = unsafe { KillTimer(Some(h), id) };
}

/// Takes the per-user single-instance mutex of instance `suffix` (already validated, see
/// [`crate::ipc::pipe::instance_suffix`]). `Ok(None)` means another instance already holds it;
/// otherwise keep the returned guard alive for the whole process.
pub fn acquire_single_instance(suffix: Option<&str>) -> Result<Option<OwnedHandle>> {
    let name: Vec<u16> = match suffix {
        Some(s) => format!("Local\\BorderlessShatter.SingleInstance.{s}"),
        None => "Local\\BorderlessShatter.SingleInstance".to_owned(),
    }
    .encode_utf16()
    .chain([0])
    .collect();
    // SAFETY: `name` is NUL-terminated and outlives the call; default security attributes.
    let mutex = unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) }
        .map_err(|e| call_err("CreateMutexW", e))?;
    let guard = OwnedHandle(mutex);
    // SAFETY: read straight after CreateMutexW, before any other call can overwrite it.
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    Ok((!exists).then_some(guard))
}
