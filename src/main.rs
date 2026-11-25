#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod borderless;
mod profiles;
mod tray;
mod watcher;
mod win32;
mod window_state;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, TranslateMessage, WM_TIMER,
};

use borderless::Manager;
use profiles::{Profile, ProfileFile};
use tray::{Tray, TrayCommand};
use win32::HWND;

/// What the message window's procedure hands to the main loop.
enum Event {
    Window(isize),
    Timer(usize),
}

thread_local! {
    /// Filled by `wnd_proc`, drained by the main loop after each `DispatchMessageW`.
    ///
    /// The procedure can run nested inside tray-icon's modal menu loop, so it only enqueues:
    /// the real work (foreign-window Win32 calls, tray updates) happens at one known place with
    /// exclusive access to `App`, never re-entrantly from inside a menu.
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let event = match msg {
        watcher::WM_SHATTER_WINDOW => Event::Window(wparam.0 as isize),
        WM_TIMER => {
            // One-shot: kill now so the timer cannot fire again while the event is queued.
            win32::kill_timer(h, wparam.0);
            Event::Timer(wparam.0)
        }
        _ => return win32::def_window_proc(h, msg, wparam, lparam),
    };
    EVENTS.with_borrow_mut(|queue| queue.push(event));
    LRESULT(0)
}

struct App {
    manager: Manager,
    tray: Tray,
    profiles: ProfileFile,
    /// Message-only window that owns the timers.
    msg_hwnd: HWND,
    /// Windows positioned and waiting for their style pass, keyed by timer id: (hwnd, profile).
    pending: HashMap<usize, (isize, Profile)>,
    next_timer: usize,
    /// Windows the user restored from the tray; automatic handling skips them.
    user_restored: HashSet<isize>,
}

fn empty_profiles() -> ProfileFile {
    ProfileFile {
        version: profiles::SUPPORTED_VERSION,
        profiles: vec![],
    }
}

fn load_profiles() -> (ProfileFile, Option<String>) {
    match profiles::default_path().and_then(|path| profiles::load_or_create(&path)) {
        Ok(f) => (f, None),
        Err(e) => {
            eprintln!("profiles: {e}");
            (empty_profiles(), Some(format!("perfiles: {e}")))
        }
    }
}

impl App {
    fn report(&mut self, what: &str, e: &dyn std::fmt::Display) {
        eprintln!("{what}: {e}");
        self.tray.set_status(&format!("{what}: {e}"));
    }

    /// Applies `p` to `h`; with a delay, positions now and arms a timer for the style pass.
    fn apply_profile(&mut self, h: HWND, p: &Profile) {
        let raw = win32::hwnd_to_isize(h);
        if self.manager.is_managed(h) {
            return;
        }
        if p.delay_ms == 0 {
            if let Err(e) = self.manager.apply(h, p) {
                self.report(&p.process, &e);
            }
            return;
        }
        match self.manager.begin_delayed(h, p) {
            Ok(true) => {
                self.next_timer += 1;
                let id = self.next_timer;
                // SetTimer clamps anything above its maximum itself.
                let delay = u32::try_from(p.delay_ms).unwrap_or(u32::MAX);
                match win32::set_timer(self.msg_hwnd, id, delay) {
                    Ok(()) => {
                        self.pending.insert(id, (raw, p.clone()));
                    }
                    Err(e) => {
                        let _ = self.manager.restore(h);
                        self.report(&p.process, &e);
                    }
                }
            }
            Ok(false) => {}
            Err(e) => self.report(&p.process, &e),
        }
    }

    fn on_window(&mut self, h: HWND) {
        if !win32::is_window(h) || self.manager.is_managed(h) {
            return;
        }
        if !self.user_restored.is_empty() {
            self.user_restored
                .retain(|&r| win32::is_window(win32::hwnd_from_isize(r)));
            if self.user_restored.contains(&win32::hwnd_to_isize(h)) {
                return;
            }
        }
        let Ok(name) = win32::process_name_of(h) else {
            return;
        };
        let Some(p) = profiles::find(&self.profiles, &name)
            .filter(|p| p.enabled)
            .cloned()
        else {
            return;
        };
        self.apply_profile(h, &p);
    }

    fn on_timer(&mut self, id: usize) {
        let Some((raw, p)) = self.pending.remove(&id) else {
            return; // cancelled by a restore
        };
        if let Err(e) = self.manager.finish_delayed(win32::hwnd_from_isize(raw), &p) {
            self.report(&p.process, &e);
        }
    }

    /// Tray "Aplicar": same matching as the automatic path (exclusions, path-style names),
    /// but ignores `enabled` and any earlier manual restore.
    fn apply_process(&mut self, process: &str) {
        for h in watcher::initial_scan() {
            let Ok(name) = win32::process_name_of(h) else {
                continue;
            };
            let Some(p) = profiles::find(&self.profiles, &name)
                .filter(|p| p.process.eq_ignore_ascii_case(process))
                .cloned()
            else {
                continue;
            };
            self.user_restored.remove(&win32::hwnd_to_isize(h));
            self.apply_profile(h, &p);
        }
    }

    /// Cancels every pending style pass; their windows are about to be restored.
    fn cancel_timers(&mut self) {
        let msg_hwnd = self.msg_hwnd;
        for (id, _) in self.pending.drain() {
            win32::kill_timer(msg_hwnd, id);
        }
    }

    fn reload(&mut self) {
        let (file, err) = load_profiles();
        self.profiles = file;
        if let Err(e) = self.tray.set_profiles(&self.profiles) {
            self.report("menú", &e);
        } else if let Some(msg) = err {
            self.tray.set_status(&msg);
        } else {
            self.tray.set_status("perfiles recargados");
        }
    }

    fn drain_events(&mut self) {
        for event in EVENTS.with_borrow_mut(std::mem::take) {
            match event {
                Event::Window(raw) => self.on_window(win32::hwnd_from_isize(raw)),
                Event::Timer(id) => self.on_timer(id),
            }
        }
    }

    fn drain_tray(&mut self) {
        while let Some(cmd) = self.tray.poll() {
            match cmd {
                TrayCommand::Apply(process) => self.apply_process(&process),
                TrayCommand::RestoreAll => {
                    self.cancel_timers();
                    // Remember what is about to be restored so it is not re-applied on the next focus.
                    let managed: Vec<isize> = win32::top_level_windows()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|&h| self.manager.is_managed(h))
                        .map(win32::hwnd_to_isize)
                        .collect();
                    self.manager.restore_all();
                    self.user_restored.extend(managed);
                }
                TrayCommand::Reload => self.reload(),
                TrayCommand::Quit => {
                    self.cancel_timers();
                    self.manager.restore_all();
                    // SAFETY: plain Win32 call, always valid on a thread with a message queue.
                    unsafe { PostQuitMessage(0) };
                }
            }
        }
    }
}

fn run() -> Result<()> {
    // Held until the process exits; a second instance must not create a second tray icon.
    let Some(_single_instance) = win32::acquire_single_instance()? else {
        return Ok(());
    };
    let (file, err) = load_profiles();
    let mut tray = Tray::new(&file)?;
    if let Some(msg) = &err {
        tray.set_status(msg);
    }
    let msg_hwnd = win32::create_message_window(wnd_proc)?;
    let _watcher = watcher::start(msg_hwnd)?;

    let mut app = App {
        manager: Manager::new(),
        tray,
        profiles: file,
        msg_hwnd,
        pending: HashMap::new(),
        next_timer: 0,
        user_restored: HashSet::new(),
    };
    for h in watcher::initial_scan() {
        app.on_window(h);
    }

    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid MSG; a null HWND receives all messages for this thread.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // 0 = WM_QUIT (also posted straight to the thread), -1 = error
        }
        // SAFETY: `msg` was filled in by GetMessageW above.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        app.drain_events();
        app.drain_tray();
    }
    app.cancel_timers();
    app.manager.restore_all();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}
