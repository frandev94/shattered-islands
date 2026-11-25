use std::collections::HashMap;

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::profiles::ProfileFile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    Apply(String),
    RestoreAll,
    Reload,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    commands: HashMap<MenuId, TrayCommand>,
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

fn build_menu(profiles: &ProfileFile) -> anyhow::Result<(Menu, HashMap<MenuId, TrayCommand>)> {
    let menu = Menu::new();
    let mut commands = HashMap::new();
    let mut add = |label: String, cmd: TrayCommand| -> anyhow::Result<()> {
        let item = MenuItem::new(label, true, None);
        menu.append(&item)?;
        commands.insert(item.id().clone(), cmd);
        Ok(())
    };
    for p in &profiles.profiles {
        add(
            format!("Aplicar: {}", p.process),
            TrayCommand::Apply(p.process.clone()),
        )?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    add("Restaurar todas".into(), TrayCommand::RestoreAll)?;
    add("Recargar perfiles".into(), TrayCommand::Reload)?;
    menu.append(&PredefinedMenuItem::separator())?;
    add("Salir".into(), TrayCommand::Quit)?;
    Ok((menu, commands))
}

impl Tray {
    /// Must be called on the thread that runs the Win32 message loop: the tray
    /// icon's hidden window and its events are bound to the creating thread.
    pub fn new(profiles: &ProfileFile) -> anyhow::Result<Tray> {
        let (menu, commands) = build_menu(profiles)?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(TOOLTIP)
            .with_icon(make_icon()?)
            .build()?;
        Ok(Tray { icon, commands })
    }

    pub fn set_profiles(&mut self, profiles: &ProfileFile) -> anyhow::Result<()> {
        let (menu, commands) = build_menu(profiles)?;
        self.icon.set_menu(Some(Box::new(menu)));
        self.commands = commands;
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
        let event = MenuEvent::receiver().try_recv().ok()?;
        self.commands.get(&event.id).cloned()
    }
}
