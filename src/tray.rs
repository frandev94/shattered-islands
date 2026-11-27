//! Resident process: tray icon, message loop and pipe server around the [`Engine`].

use std::collections::HashMap;
use std::sync::{Arc, mpsc};

use tokio::sync::oneshot;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, TranslateMessage,
};

use crate::autostart::{self, Autostart};
use crate::engine::{self, Engine, IpcItem, WM_SHATTER_IPC};
use crate::i18n::{t, tf};
use crate::ipc::Request;
use crate::ipc::pipe::{self, Handler};
use crate::islands::Layout;
use crate::profiles::ProfileFile;
use crate::{watcher, win32};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    Apply(String),
    /// "layout/island" to place the active window in.
    Place(String),
    RestoreAll,
    Reload,
    /// Launch `borderless-shatter gui` (the panel enforces its own single instance).
    OpenPanel,
    /// Turn "Start at sign-in" on (or off, when it is on).
    ToggleAutostart,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    commands: HashMap<MenuId, TrayCommand>,
    /// The menu's autostart check, refreshed from the registry before the menu opens.
    autostart_item: CheckMenuItem,
}

/// The right-button press arrives as its own event, before the release that opens the menu.
fn is_right_press(event: &TrayIconEvent) -> bool {
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Right,
            button_state: MouseButtonState::Down,
            ..
        }
    )
}

/// Flips the registry entry: on becomes off; off, another copy or disabled by Windows becomes on.
fn toggle_autostart() -> Result<(), win32::Win32Error> {
    if autostart::status()? == Autostart::On {
        autostart::disable()
    } else {
        autostart::enable()
    }
}

const TOOLTIP: &str = "Borderless Shatter";

/// Two-colour 32x32 square with a diagonal "crack".
fn make_icon() -> anyhow::Result<Icon> {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let crack = (x as i32 - y as i32).abs() <= 1;
            let px: [u8; 4] = if crack {
                [255, 255, 255, 255]
            } else if x < N / 2 {
                [30, 120, 220, 255]
            } else {
                [20, 40, 80, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Ok(Icon::from_rgba(rgba, N, N)?)
}

type BuiltMenu = (Menu, HashMap<MenuId, TrayCommand>, CheckMenuItem);

fn build_menu(profiles: &ProfileFile, layouts: &[Layout]) -> anyhow::Result<BuiltMenu> {
    let menu = Menu::new();
    let mut commands = HashMap::new();
    let panel = MenuItem::new(t("tray.open_panel"), true, None);
    menu.append(&panel)?;
    commands.insert(panel.id().clone(), TrayCommand::OpenPanel);
    menu.append(&PredefinedMenuItem::separator())?;
    for p in &profiles.profiles {
        let label = tf("tray.apply", &[("process", &p.process)]);
        let item = MenuItem::new(label, true, None);
        menu.append(&item)?;
        commands.insert(item.id().clone(), TrayCommand::Apply(p.process.clone()));
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let place = Submenu::new(t("tray.place_submenu"), true);
    for layout in layouts {
        for island in &layout.islands {
            let reference = format!("{}/{}", layout.id, island.id);
            let item = MenuItem::new(&reference, true, None);
            place.append(&item)?;
            commands.insert(item.id().clone(), TrayCommand::Place(reference));
        }
    }
    menu.append(&place)?;
    menu.append(&PredefinedMenuItem::separator())?;
    for (label, cmd) in [
        (t("tray.restore_all"), TrayCommand::RestoreAll),
        (t("tray.reload"), TrayCommand::Reload),
    ] {
        let item = MenuItem::new(label, true, None);
        menu.append(&item)?;
        commands.insert(item.id().clone(), cmd);
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let start = CheckMenuItem::new(
        t("tray.autostart"),
        true,
        autostart::status().is_ok_and(|s| s == Autostart::On),
        None,
    );
    menu.append(&start)?;
    commands.insert(start.id().clone(), TrayCommand::ToggleAutostart);
    menu.append(&PredefinedMenuItem::separator())?;
    let quit = MenuItem::new(t("tray.quit"), true, None);
    menu.append(&quit)?;
    commands.insert(quit.id().clone(), TrayCommand::Quit);
    Ok((menu, commands, start))
}

impl Tray {
    /// Must be called on the thread that runs the Win32 message loop: the tray
    /// icon's hidden window and its events are bound to the creating thread.
    pub fn new(profiles: &ProfileFile, layouts: &[Layout]) -> anyhow::Result<Tray> {
        let (menu, commands, autostart_item) = build_menu(profiles, layouts)?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(TOOLTIP)
            .with_icon(make_icon()?)
            .build()?;
        Ok(Tray {
            icon,
            commands,
            autostart_item,
        })
    }

    pub fn set_menu(&mut self, profiles: &ProfileFile, layouts: &[Layout]) -> anyhow::Result<()> {
        let (menu, commands, autostart_item) = build_menu(profiles, layouts)?;
        self.icon.set_menu(Some(Box::new(menu)));
        self.commands = commands;
        self.autostart_item = autostart_item;
        Ok(())
    }

    pub fn set_status(&mut self, status: &str) {
        // A failed tooltip update is cosmetic; nothing useful to do about it.
        let mut text = format!("{TOOLTIP} — {status}");
        if text.encode_utf16().count() > 127 {
            // Tooltips hold 128 UTF-16 units including the NUL; keep room for '…'.
            let mut units = 0;
            let cut = text
                .char_indices()
                .find(|(_, c)| {
                    units += c.len_utf16();
                    units > 126
                })
                .map_or(text.len(), |(i, _)| i);
            text.truncate(cut);
            text.push('…');
        }
        let _ = self.icon.set_tooltip(Some(text));
    }

    pub fn poll(&self) -> Option<TrayCommand> {
        loop {
            if let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(cmd) = self.commands.get(&event.id) {
                    return Some(cmd.clone());
                }
                continue;
            }
            let event = TrayIconEvent::receiver().try_recv().ok()?;
            if is_right_press(&event) {
                // The panel may have changed the registry since the menu was built.
                let on = autostart::status().is_ok_and(|s| s == Autostart::On);
                self.autostart_item.set_checked(on);
            }
            if let Some(cmd) = command_for_event(&event) {
                return Some(cmd);
            }
        }
    }
}

/// Maps a tray-icon event to the command it triggers; only a left double-click opens the panel.
fn command_for_event(event: &TrayIconEvent) -> Option<TrayCommand> {
    match event {
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => Some(TrayCommand::OpenPanel),
        _ => None,
    }
}

fn drain_tray(tray: &mut Tray, engine: &mut Engine) {
    while let Some(cmd) = tray.poll() {
        match cmd {
            TrayCommand::Apply(process) => engine.apply_process(&process),
            TrayCommand::Place(island) => engine.place_foreground(&island),
            TrayCommand::RestoreAll => engine.restore_all_user(),
            TrayCommand::Reload => engine.reload(),
            TrayCommand::OpenPanel => {
                if let Err(e) = crate::gui::spawn_detached(&["gui"]) {
                    eprintln!("could not open the panel: {e}");
                }
            }
            TrayCommand::ToggleAutostart => {
                if let Err(e) = toggle_autostart() {
                    eprintln!("autostart: {e}");
                    tray.set_status(&tf("tray.status.autostart_error", &[("error", &e)]));
                }
            }
            // SAFETY: plain Win32 call, always valid on a thread with a message queue.
            TrayCommand::Quit => unsafe { PostQuitMessage(0) },
        }
    }
}

/// Forwards pipe requests to the message-loop thread and wakes it up.
fn ipc_handler(queue: mpsc::Sender<IpcItem>, msg_hwnd: isize) -> Handler {
    Arc::new(move |req: Request| {
        let (tx, rx) = oneshot::channel();
        if queue.send((req, tx)).is_ok() {
            let _ = win32::post_message(win32::hwnd_from_isize(msg_hwnd), WM_SHATTER_IPC, 0);
        }
        // A failed send dropped `tx`, so the receiver reports the closure to the pipe task.
        rx
    })
}

/// Runs the resident process until `WM_QUIT`; restores every managed window before returning.
/// The tray language is fixed here: a language change applies on the next start.
pub fn run() -> anyhow::Result<()> {
    // A set-but-invalid instance suffix stops here (fatal, exit 1): no mutex, tray or pipe.
    let suffix = crate::ipc::pipe::instance_suffix()?;
    // Held until the process exits; a second instance must not create a second tray icon.
    let Some(_single_instance) = win32::acquire_single_instance(suffix.as_deref())? else {
        return Ok(());
    };
    crate::i18n::init();
    let msg_hwnd = win32::create_message_window(engine::wnd_proc)?;
    let (queue, inbox) = mpsc::channel();
    let mut engine = Engine::new(msg_hwnd, inbox);
    let mut tray = Tray::new(engine.profiles(), &engine.layouts())?;
    let _watcher = watcher::start(msg_hwnd)?;
    engine.startup();
    // The sink runs on the pipe thread: it only queues the text and wakes the loop.
    let (err_tx, err_rx) = mpsc::channel::<String>();
    let err_hwnd = win32::hwnd_to_isize(msg_hwnd);
    let sink: pipe::ErrorSink = Arc::new(move |m: String| {
        if err_tx.send(m).is_ok() {
            let _ = win32::post_message(win32::hwnd_from_isize(err_hwnd), WM_SHATTER_IPC, 0);
        }
    });
    let server = match pipe::spawn_server_with_status(
        ipc_handler(queue, win32::hwnd_to_isize(msg_hwnd)),
        Some(sink),
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("ipc: {e}");
            tray.set_status(&tf("tray.status.no_pipe", &[("error", &e)]));
            None
        }
    };

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
        engine.drain();
        while let Ok(m) = err_rx.try_recv() {
            eprintln!("ipc: pipe stopped: {m}");
            tray.set_status(&tf("tray.status.pipe_stopped", &[("error", &m)]));
        }
        drain_tray(&mut tray, &mut engine);
        if engine.take_menu_dirty()
            && let Err(e) = tray.set_menu(engine.profiles(), &engine.layouts())
        {
            eprintln!("menu: {e}");
            tray.set_status(&tf("tray.status.menu_error", &[("error", &e)]));
        }
        if let Some(status) = engine.take_status() {
            tray.set_status(&status);
        }
    }
    if let Some(server) = server {
        server.stop();
    }
    engine.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Locale, scoped_locale};
    use tray_icon::dpi::{PhysicalPosition, PhysicalSize};
    use tray_icon::menu::MenuItemKind;
    use tray_icon::{MouseButton, MouseButtonState, Rect, TrayIconId};

    fn labels(menu: &Menu) -> Vec<String> {
        menu.items()
            .iter()
            .filter_map(|i| match i {
                MenuItemKind::MenuItem(m) => Some(m.text()),
                MenuItemKind::Check(c) => Some(c.text()),
                MenuItemKind::Submenu(s) => Some(s.text()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn menu_labels_come_from_the_catalog() {
        let profiles: ProfileFile = serde_json::from_value(serde_json::json!({
            "version": 2, "profiles": [{"process": "game.exe"}]
        }))
        .unwrap();
        let _g = scoped_locale(Locale::En);
        let (menu, commands, _) = build_menu(&profiles, &crate::islands::presets()).unwrap();
        assert_eq!(
            labels(&menu),
            [
                "Open panel",
                "Apply: game.exe",
                "Place active window in island",
                "Restore all",
                "Reload profiles",
                "Start at sign-in",
                "Quit"
            ]
        );
        assert!(commands.values().any(|c| *c == TrayCommand::Quit));
        assert!(
            commands
                .values()
                .any(|c| *c == TrayCommand::ToggleAutostart)
        );
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        let (menu, _, _) = build_menu(&profiles, &[]).unwrap();
        let es = labels(&menu);
        assert_eq!(es[0], crate::i18n::t("tray.open_panel"));
        assert_ne!(es[0], "Open panel");
        assert!(es.iter().all(|l| !l.starts_with("tray.")), "{es:?}");
    }

    fn rect() -> Rect {
        Rect {
            size: PhysicalSize::new(0, 0),
            position: PhysicalPosition::new(0.0, 0.0),
        }
    }

    fn double_click(button: MouseButton) -> TrayIconEvent {
        TrayIconEvent::DoubleClick {
            id: TrayIconId::new("test"),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: rect(),
            button,
        }
    }

    #[test]
    fn only_a_right_button_press_refreshes_the_check() {
        let click = |button, button_state| TrayIconEvent::Click {
            id: TrayIconId::new("test"),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: rect(),
            button,
            button_state,
        };
        assert!(is_right_press(&click(
            MouseButton::Right,
            MouseButtonState::Down
        )));
        assert!(!is_right_press(&click(
            MouseButton::Right,
            MouseButtonState::Up
        )));
        assert!(!is_right_press(&click(
            MouseButton::Left,
            MouseButtonState::Down
        )));
    }

    #[test]
    fn left_double_click_opens_panel() {
        let ev = double_click(MouseButton::Left);
        assert_eq!(command_for_event(&ev), Some(TrayCommand::OpenPanel));
    }

    #[test]
    fn right_and_middle_double_click_do_nothing() {
        assert_eq!(command_for_event(&double_click(MouseButton::Right)), None);
        assert_eq!(command_for_event(&double_click(MouseButton::Middle)), None);
    }

    #[test]
    fn single_left_click_does_nothing() {
        let ev = TrayIconEvent::Click {
            id: TrayIconId::new("test"),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: rect(),
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
        };
        assert_eq!(command_for_event(&ev), None);
    }
}
