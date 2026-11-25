//! WinEvent hooks that notify the message-only window about new/foregrounded windows.

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::HWND as RawHwnd;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

use crate::win32::{self, HWND, Win32Error};

/// WM_APP + 1; wparam = hwnd as usize.
pub const WM_SHATTER_WINDOW: u32 = 0x8000 + 1;

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
    _event: u32,
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
        let _ = win32::post_message(
            win32::hwnd_from_isize(target),
            WM_SHATTER_WINDOW,
            win32::hwnd_to_isize(hwnd) as usize,
        );
    }
}

/// Must be called on the thread that runs the message loop (out-of-context hooks need it).
/// `target` is the message window that will receive the events.
pub fn start(target: HWND) -> win32::Result<Watcher> {
    TARGET.store(win32::hwnd_to_isize(target), Ordering::Relaxed);
    let mut watcher = Watcher {
        hooks: Vec::with_capacity(2),
    };
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
