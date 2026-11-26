//! Borderless Shatter engine: window state, islands, re-assert guard, profiles, IPC and the
//! resident engine that owns every managed window. `main.rs` only dispatches subcommands.

pub mod cli;
pub mod diagnose;
pub mod engine;
pub mod guard;
pub mod gui;
pub mod i18n;
#[warn(missing_docs)]
pub mod ipc;
#[warn(missing_docs)]
pub mod islands;
pub mod mcp;
#[warn(missing_docs)]
pub mod profiles;
pub mod settings;
pub mod state_store;
pub mod tray;
pub mod watcher;
pub mod win32;
#[warn(missing_docs)]
pub mod window_state;

/// `os error N` for an OS error, else the error's own (English, non-OS) text: messages stay
/// English-only whatever the OS language.
pub(crate) fn io_text(e: &std::io::Error) -> String {
    e.raw_os_error()
        .map_or_else(|| e.to_string(), |n| format!("os error {n}"))
}

/// Replaces `path` with `bytes` through `<path>.tmp`, synced to disk before the rename, so a
/// reader or a power cut sees the old file or the new one, never a torn or empty one.
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let write = || {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f); // closed before the rename: Windows refuses to move an open file
        std::fs::rename(&tmp, path)
    };
    let r = write();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}
