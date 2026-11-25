use std::collections::HashMap;

use anyhow::Result;

use crate::profiles::Profile;
use crate::win32::{self, HWND, ShowCmd};
use crate::window_state::{self, Rect, WindowState};

/// Original state of a managed window plus the rect it is forced to, fixed at capture time.
struct Entry {
    state: WindowState,
    target: Rect,
}

/// Owns the original state of every window we have made borderless.
pub struct Manager {
    managed: HashMap<isize, Entry>,
}

impl Manager {
    pub fn new() -> Self {
        Self {
            managed: HashMap::new(),
        }
    }

    pub fn is_managed(&self, h: HWND) -> bool {
        self.managed.contains_key(&win32::hwnd_to_isize(h))
    }

    pub fn apply(&mut self, h: HWND, p: &Profile) -> Result<bool> {
        if !self.capture(h, p)? {
            return Ok(false);
        }
        self.finish_delayed(h, p)?;
        Ok(true)
    }

    pub fn begin_delayed(&mut self, h: HWND, p: &Profile) -> Result<bool> {
        if !self.capture(h, p)? {
            return Ok(false);
        }
        let target = self.managed[&win32::hwnd_to_isize(h)].target;
        if let Err(e) = win32::set_pos(h, target, win32::SWP_APPLY) {
            let _ = self.restore(h);
            return Err(e.into());
        }
        Ok(true)
    }

    /// Strips the frame and repositions to the rect fixed by `capture`; the profile is not
    /// re-read so the geometry cannot drift between the two phases. On failure the window is restored.
    pub fn finish_delayed(&mut self, h: HWND, _p: &Profile) -> Result<()> {
        let Some(target) = self.managed.get(&win32::hwnd_to_isize(h)).map(|e| e.target) else {
            return Ok(());
        };
        if !win32::is_window(h) {
            self.managed.remove(&win32::hwnd_to_isize(h));
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let (style, exstyle) = win32::get_styles(h)?;
            let (style, exstyle) = window_state::borderless_styles(style, exstyle);
            win32::set_styles(h, style, exstyle)?;
            win32::set_pos(h, target, win32::SWP_APPLY)?;
            // The app may relayout itself in response to the style change; re-assert once.
            if win32::window_rect(h)? != target {
                win32::set_pos(h, target, win32::SWP_APPLY)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = self.restore(h);
            return Err(e);
        }
        Ok(())
    }

    /// Attempts every step even when one fails, always forgets the window, returns the first error.
    pub fn restore(&mut self, h: HWND) -> Result<()> {
        let Some(Entry { state: orig, .. }) = self.managed.remove(&win32::hwnd_to_isize(h)) else {
            return Ok(());
        };
        if !win32::is_window(h) {
            return Ok(());
        }
        let mut first: Option<anyhow::Error> = None;
        let mut note = |r: win32::Result<()>| {
            if let Err(e) = r {
                first.get_or_insert(e.into());
            }
        };
        note(win32::set_styles(h, orig.style, orig.exstyle));
        note(win32::set_pos(h, orig.rect, win32::SWP_RESTORE));
        if orig.maximized {
            win32::show(h, ShowCmd::Maximize);
        }
        first.map_or(Ok(()), Err)
    }

    pub fn restore_all(&mut self) {
        let handles: Vec<isize> = self.managed.keys().copied().collect();
        for raw in handles {
            if let Err(e) = self.restore(win32::hwnd_from_isize(raw)) {
                eprintln!("restore {raw:#x} failed: {e}");
            }
        }
    }

    /// Guard + save original state and target rect. Returns false when the window must be left alone.
    fn capture(&mut self, h: HWND, p: &Profile) -> Result<bool> {
        if self.is_managed(h) || !win32::is_window(h) {
            return Ok(false);
        }
        let (style, _) = win32::get_styles(h)?;
        if !window_state::has_targetable_style(style) {
            return Ok(false);
        }
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
        let entry = (|| -> Result<Entry> {
            let (style, exstyle) = win32::get_styles(h)?;
            let rect = win32::window_rect(h)?;
            let monitor = window_state::pick_monitor(
                &win32::monitor_rects()?,
                win32::monitor_rect_of(h)?,
                p.monitor.index(),
            );
            Ok(Entry {
                state: WindowState {
                    hwnd: win32::hwnd_to_isize(h),
                    style,
                    exstyle,
                    rect,
                    maximized,
                },
                target: window_state::target_rect(monitor, p.offsets),
            })
        })()
        .inspect_err(|_| {
            if maximized {
                win32::show(h, ShowCmd::Maximize);
            }
        })?;
        self.managed.insert(win32::hwnd_to_isize(h), entry);
        Ok(true)
    }
}
