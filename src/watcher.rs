//! WinEvent hooks that notify the message-only window about new, foregrounded and renamed windows.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{E_INVALIDARG, HWND as RawHwnd};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_STATECHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MOVESIZEEND,
    EVENT_SYSTEM_MOVESIZESTART, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

use crate::win32::{self, HWND, Win32Error};

/// WM_APP + 1; wparam = hwnd as usize.
pub const WM_SHATTER_WINDOW: u32 = 0x8000 + 1;
/// WM_APP + 3; wparam = watched hwnd. Its location or state changed.
pub const WM_SHATTER_CHANGED: u32 = 0x8000 + 3;
/// WM_APP + 4; wparam = watched hwnd, lparam 1 = move/size started, 0 = ended.
pub const WM_SHATTER_MOVESIZE: u32 = 0x8000 + 4;
/// WM_APP + 6; wparam = hwnd as usize. A top-level window's title changed.
pub const WM_SHATTER_RENAMED: u32 = 0x8000 + 6;

/// Message window that receives `WM_SHATTER_WINDOW`; 0 until `start()` runs.
static TARGET: AtomicIsize = AtomicIsize::new(0);

pub struct Watcher {
    hooks: Vec<HWINEVENTHOOK>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for h in self.hooks.drain(..) {
            // SAFETY: `h` was returned by SetWinEventHook and is unhooked exactly once.
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}

extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: RawHwnd,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    let target = TARGET.load(Ordering::Relaxed);
    if target == 0 {
        return;
    }
    if win32::is_top_level(hwnd) && win32::is_visible(hwnd) {
        let msg = if event == EVENT_OBJECT_NAMECHANGE {
            WM_SHATTER_RENAMED
        } else {
            WM_SHATTER_WINDOW
        };
        let _ = win32::post_message(
            win32::hwnd_from_isize(target),
            msg,
            win32::hwnd_to_isize(hwnd) as usize,
        );
    }
}

/// Must be called on the thread that runs the message loop (out-of-context hooks need it).
/// `target` is the message window that will receive the events.
pub fn start(target: HWND) -> win32::Result<Watcher> {
    TARGET.store(win32::hwnd_to_isize(target), Ordering::Relaxed);
    let mut watcher = Watcher {
        hooks: Vec::with_capacity(3),
    };
    for event in [
        EVENT_OBJECT_SHOW,
        EVENT_SYSTEM_FOREGROUND,
        EVENT_OBJECT_NAMECHANGE,
    ] {
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
            // `watcher` drops here and unhooks whatever was installed.
            return Err(Win32Error::Call {
                call: "SetWinEventHook",
                source: windows::core::Error::from_thread(),
            });
        }
        watcher.hooks.push(hook);
    }
    Ok(watcher)
}

pub fn initial_scan() -> Vec<HWND> {
    win32::top_level_windows()
        .map(|v| {
            v.into_iter()
                .filter(|&h| win32::is_top_level(h) && win32::is_visible(h))
                .collect()
        })
        .unwrap_or_default()
}

/// Hooks installed for one process id plus the watched windows of that process.
struct PidHooks {
    /// `HWINEVENTHOOK` pointers stored as integers so the map is `Send`.
    hooks: Vec<isize>,
    hwnds: Vec<isize>,
}

static WATCHED: Mutex<BTreeMap<u32, PidHooks>> = Mutex::new(BTreeMap::new());

/// Locks the registry; a poisoned lock is still usable (the callback must never panic).
fn watched() -> std::sync::MutexGuard<'static, BTreeMap<u32, PidHooks>> {
    WATCHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

extern "system" fn on_watch_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: RawHwnd,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    let target = TARGET.load(Ordering::Relaxed);
    if target == 0 {
        return;
    }
    let id = win32::hwnd_to_isize(hwnd);
    let is_watched = watched()
        .get(&win32::window_pid(hwnd))
        .is_some_and(|p| p.hwnds.contains(&id));
    if !is_watched {
        return;
    }
    let (msg, lparam) = match event {
        EVENT_SYSTEM_MOVESIZESTART => (WM_SHATTER_MOVESIZE, 1),
        EVENT_SYSTEM_MOVESIZEEND => (WM_SHATTER_MOVESIZE, 0),
        _ => (WM_SHATTER_CHANGED, 0),
    };
    let _ = win32::post_message_lparam(win32::hwnd_from_isize(target), msg, id as usize, lparam);
}

fn unhook_all(hooks: &[isize]) {
    for &h in hooks {
        // SAFETY: `h` was returned by SetWinEventHook and is unhooked exactly once.
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut core::ffi::c_void));
        }
    }
}

/// Starts delivering `WM_SHATTER_CHANGED`/`WM_SHATTER_MOVESIZE` for window `h` to the message
/// window given to `start()`. Hooks are shared per process id. Call on the message-loop thread.
pub fn watch(h: HWND) -> win32::Result<()> {
    let pid = win32::window_pid(h);
    if pid == 0 {
        return Err(Win32Error::Call {
            call: "GetWindowThreadProcessId",
            source: windows::core::Error::from(E_INVALIDARG),
        });
    }
    let id = win32::hwnd_to_isize(h);
    let mut map = watched();
    if let Some(entry) = map.get_mut(&pid) {
        if !entry.hwnds.contains(&id) {
            entry.hwnds.push(id);
        }
        return Ok(());
    }
    let mut hooks = Vec::with_capacity(3);
    for (min, max) in [
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        (EVENT_OBJECT_STATECHANGE, EVENT_OBJECT_STATECHANGE),
        (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
    ] {
        // SAFETY: `on_watch_event` has the WINEVENTPROC signature and lives for the whole
        // process; no module handle is needed for WINEVENT_OUTOFCONTEXT.
        let hook = unsafe {
            SetWinEventHook(
                min,
                max,
                None,
                Some(on_watch_event),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.0.is_null() {
            let source = windows::core::Error::from_thread();
            unhook_all(&hooks);
            return Err(Win32Error::Call {
                call: "SetWinEventHook",
                source,
            });
        }
        hooks.push(hook.0 as isize);
    }
    map.insert(
        pid,
        PidHooks {
            hooks,
            hwnds: vec![id],
        },
    );
    Ok(())
}

/// Stops watching `h`; removes the process hooks when no watched window of that pid remains.
/// Call on the message-loop thread (the thread that installed the hooks).
pub fn unwatch(h: HWND) {
    let id = win32::hwnd_to_isize(h);
    let mut map = watched();
    // The window may already be gone (pid 0), so find the owner by hwnd instead.
    let Some(pid) = map
        .iter()
        .find(|(_, p)| p.hwnds.contains(&id))
        .map(|(&pid, _)| pid)
    else {
        return;
    };
    if let Some(entry) = map.get_mut(&pid) {
        entry.hwnds.retain(|&x| x != id);
        if entry.hwnds.is_empty()
            && let Some(entry) = map.remove(&pid)
        {
            unhook_all(&entry.hooks);
        }
    }
}
