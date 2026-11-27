//! Start at sign-in: one per-user value under `HKCU\...\Run` (no admin rights). The registry is the
//! only source of truth: nothing is copied to `settings.json`, and the panel and the tray both read
//! it each time they show it. Task Manager's "Disabled" switch lives in `StartupApproved\Run`; the
//! pure parts (names, quoting, blob parsing, classification) import no `windows` types.

use std::path::Path;

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY, REG_EXPAND_SZ, REG_NONE,
    REG_OPEN_CREATE_OPTIONS, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::HSTRING;

use crate::win32::{Result, Win32Error};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const BASE_NAME: &str = "Borderless Shatter";

/// What the registry says about starting this exe at sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Autostart {
    Off,
    On,
    /// The Run value exists but starts another copy of the program (its path, unquoted).
    OnOtherPath(String),
    /// Task Manager's Startup tab turned the entry off.
    DisabledByWindows,
}

/// Run value name; an instance suffix keeps isolated instances off the real entry.
fn value_name(suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{BASE_NAME} ({s})"),
        None => BASE_NAME.to_owned(),
    }
}

fn command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// `StartupApproved` blob: the first byte is odd when the user disabled the entry.
fn approved_disabled(blob: &[u8]) -> bool {
    blob.first().is_some_and(|b| b & 1 == 1)
}

/// The executable path inside a stored command (quoted or bare).
fn stored_exe(stored: &str) -> &str {
    let s = stored.trim();
    match s.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => s,
    }
}

fn classify(stored: Option<&str>, disabled: bool, exe: &Path) -> Autostart {
    let Some(stored) = stored else {
        return Autostart::Off;
    };
    if disabled {
        return Autostart::DisabledByWindows;
    }
    // Extra arguments after our own path still start this exe, so they count as On.
    if stored_exe(stored).to_lowercase() == exe.display().to_string().to_lowercase() {
        Autostart::On
    } else {
        Autostart::OnOtherPath(stored_exe(stored).to_owned())
    }
}

fn current_name() -> String {
    // An invalid suffix already stopped every entry point; treating it as unset is unreachable.
    let suffix = crate::ipc::pipe::instance_suffix().ok().flatten();
    value_name(suffix.as_deref())
}

fn call_err(call: &'static str, source: windows::core::Error) -> Win32Error {
    Win32Error::Call { call, source }
}

/// Owned registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed exactly once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Opens (or, with `create`, makes) a subkey of HKCU. `None` = the key does not exist.
/// This is the only door to the registry: `cargo test` never goes through it.
fn open(subkey: &str, create: bool, access: REG_SAM_FLAGS) -> Result<Option<Key>> {
    if cfg!(test) {
        let denied = windows::core::Error::from(ERROR_ACCESS_DENIED.to_hresult());
        return Err(call_err("registry (unreachable under cfg(test))", denied));
    }
    let path = HSTRING::from(subkey);
    let mut key = HKEY::default();
    // SAFETY: `path` is a NUL-terminated wide string and `key` a valid out pointer.
    let r = unsafe {
        if create {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &path,
                None,
                None,
                REG_OPEN_CREATE_OPTIONS(0),
                access,
                None,
                &mut key,
                None,
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, &path, None, access, &mut key)
        }
    };
    if r == ERROR_FILE_NOT_FOUND && !create {
        return Ok(None);
    }
    let call = if create {
        "RegCreateKeyExW"
    } else {
        "RegOpenKeyExW"
    };
    r.ok().map_err(|e| call_err(call, e))?;
    Ok(Some(Key(key)))
}

/// The value types we accept: the Run command is text, the StartupApproved mark is binary.
#[derive(Clone, Copy)]
enum ValueKind {
    Text,
    Binary,
}

impl ValueKind {
    fn accepts(self, ty: REG_VALUE_TYPE) -> bool {
        match self {
            // Not expanded: we only compare it with the path we wrote as REG_SZ.
            Self::Text => ty == REG_SZ || ty == REG_EXPAND_SZ,
            Self::Binary => ty == REG_BINARY,
        }
    }
}

/// Raw bytes of a value; `None` when the key or value is absent, or has a type of the wrong kind.
fn read_value(subkey: &str, name: &str, kind: ValueKind) -> Result<Option<Vec<u8>>> {
    let Some(key) = open(subkey, false, KEY_QUERY_VALUE)? else {
        return Ok(None);
    };
    let name = HSTRING::from(name);
    let mut len = 0u32;
    // SAFETY: size query only: no data pointer, `len` is a valid out pointer.
    let r = unsafe { RegQueryValueExW(key.0, &name, None, None, None, Some(&mut len)) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    r.ok().map_err(|e| call_err("RegQueryValueExW", e))?;
    let mut buf = vec![0u8; len as usize];
    let mut ty = REG_NONE;
    // SAFETY: `buf` holds `len` bytes, which the call may overwrite and shrink in `len`;
    // `ty` is a valid out pointer.
    let r = unsafe {
        RegQueryValueExW(
            key.0,
            &name,
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
    };
    r.ok().map_err(|e| call_err("RegQueryValueExW", e))?;
    if !kind.accepts(ty) {
        return Ok(None);
    }
    buf.truncate(len as usize);
    Ok(Some(buf))
}

fn delete_value(subkey: &str, name: &str) -> Result<()> {
    let Some(key) = open(subkey, false, KEY_SET_VALUE)? else {
        return Ok(());
    };
    // SAFETY: `key` is open with KEY_SET_VALUE and the name is a NUL-terminated wide string.
    let r = unsafe { RegDeleteValueW(key.0, &HSTRING::from(name)) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    r.ok().map_err(|e| call_err("RegDeleteValueW", e))
}

fn write_string(subkey: &str, name: &str, data: &str) -> Result<()> {
    let Some(key) = open(subkey, true, KEY_SET_VALUE)? else {
        let missing = windows::core::Error::from(ERROR_FILE_NOT_FOUND.to_hresult());
        return Err(call_err("RegCreateKeyExW", missing));
    };
    let bytes: Vec<u8> = data
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: `key` is open with KEY_SET_VALUE; `bytes` is a NUL-terminated UTF-16 string.
    let r = unsafe { RegSetValueExW(key.0, &HSTRING::from(name), None, REG_SZ, Some(&bytes)) };
    r.ok().map_err(|e| call_err("RegSetValueExW", e))
}

fn utf16_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_end_matches('\0')
        .to_owned()
}

/// Reads the registry: does it start this exe at sign-in, and did Windows disable it?
/// A value of the wrong type under our name is foreign to us: Windows cannot run it as a command
/// and ignores it as an approval mark, so it counts as absent (`enable` then replaces it).
pub fn status() -> Result<Autostart> {
    let name = current_name();
    let stored = read_value(RUN_KEY, &name, ValueKind::Text)?.map(|b| utf16_string(&b));
    let disabled =
        read_value(APPROVED_KEY, &name, ValueKind::Binary)?.is_some_and(|b| approved_disabled(&b));
    let exe = std::env::current_exe().unwrap_or_default();
    Ok(classify(stored.as_deref(), disabled, &exe))
}

/// Starts this exe at sign-in. Also clears Task Manager's "Disabled" mark: the choice made here wins.
pub fn enable() -> Result<()> {
    let name = current_name();
    let exe = std::env::current_exe().map_err(|e| call_err("current_exe", e.into()))?;
    write_string(RUN_KEY, &name, &command_line(&exe))?;
    delete_value(APPROVED_KEY, &name)
}

pub fn disable() -> Result<()> {
    let name = current_name();
    delete_value(RUN_KEY, &name)?;
    delete_value(APPROVED_KEY, &name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use windows::Win32::System::Registry::{REG_DWORD, REG_MULTI_SZ, REG_QWORD};

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Games\Tools\borderless-shatter.exe")
    }

    #[test]
    fn value_name_carries_the_instance_suffix() {
        assert_eq!(value_name(None), "Borderless Shatter");
        assert_eq!(value_name(Some("e2e-1")), "Borderless Shatter (e2e-1)");
    }

    #[test]
    fn command_line_quotes_the_path() {
        assert_eq!(
            command_line(Path::new(r"C:\Program Files\BS\bs.exe")),
            r#""C:\Program Files\BS\bs.exe""#
        );
    }

    #[test]
    fn approved_blob_odd_first_byte_means_disabled() {
        assert!(!approved_disabled(&[]));
        assert!(!approved_disabled(&[0x02, 0, 0, 0]));
        assert!(!approved_disabled(&[0x06, 0, 0, 0]));
        assert!(approved_disabled(&[0x03, 0, 0, 0]));
        assert!(approved_disabled(&[0x01]));
    }

    #[test]
    fn missing_value_is_off_even_if_marked_disabled() {
        assert_eq!(classify(None, true, &exe()), Autostart::Off);
    }

    #[test]
    fn same_path_is_on_ignoring_case_and_padding() {
        let stored = r#" "c:\games\TOOLS\Borderless-Shatter.exe" "#;
        assert_eq!(classify(Some(stored), false, &exe()), Autostart::On);
    }

    #[test]
    fn another_path_reports_that_path_unquoted() {
        let stored = r#""D:\old\bs.exe""#;
        assert_eq!(
            classify(Some(stored), false, &exe()),
            Autostart::OnOtherPath(r"D:\old\bs.exe".to_owned())
        );
    }

    #[test]
    fn extra_arguments_still_start_this_exe() {
        let stored = format!("{} gui", command_line(&exe()));
        assert_eq!(classify(Some(&stored), false, &exe()), Autostart::On);
    }

    #[test]
    fn run_value_must_be_text_and_approval_must_be_binary() {
        for ty in [REG_SZ, REG_EXPAND_SZ] {
            assert!(ValueKind::Text.accepts(ty));
            assert!(!ValueKind::Binary.accepts(ty));
        }
        assert!(ValueKind::Binary.accepts(REG_BINARY));
        for ty in [REG_BINARY, REG_DWORD, REG_MULTI_SZ, REG_QWORD, REG_NONE] {
            assert!(!ValueKind::Text.accepts(ty), "{ty:?}");
        }
        for ty in [REG_DWORD, REG_MULTI_SZ, REG_QWORD, REG_NONE] {
            assert!(!ValueKind::Binary.accepts(ty), "{ty:?}");
        }
    }

    #[test]
    fn windows_disabled_wins_over_a_matching_path() {
        let stored = command_line(&exe());
        assert_eq!(
            classify(Some(&stored), true, &exe()),
            Autostart::DisabledByWindows
        );
    }

    #[test]
    fn utf16_value_drops_the_terminator() {
        let bytes: Vec<u8> = "a b\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(utf16_string(&bytes), "a b");
    }

    #[test]
    fn tests_never_reach_the_real_registry() {
        assert!(status().is_err());
        assert!(enable().is_err());
        assert!(disable().is_err());
    }
}
