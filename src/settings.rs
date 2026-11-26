//! `%APPDATA%\BorderlessShatter\settings.json` v1: `{ "version": 1, "language": "system"|"en"|"es" }`
//! (plan addendum W2, A1). Read by the resident, the panel and the CLI; written only by the panel.
//! Shared by every `BORDERLESS_SHATTER_INSTANCE`, like `profiles.json` (only `state.json` is not).
//! A missing, unreadable, invalid, oversized or newer file gives the defaults and is never
//! overwritten: [`save`] only replaces a missing or valid v1 file, atomically.

use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::i18n::Locale;

pub const VERSION: u64 = 1;
/// Larger files are not read at all.
pub const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `None` = follow the system (`"system"`).
    pub language: Option<Locale>,
}

/// What [`load`] found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Missing,
    Valid,
    /// Unreadable, too large, not JSON, or the wrong shape.
    Invalid,
    /// Written by a newer version (`version` > 1).
    Future(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loaded {
    pub settings: Settings,
    pub state: FileState,
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("settings file has version {0}, newer than this app; not overwritten")]
    FutureVersion(u64),
    #[error("settings file is unreadable or invalid; not overwritten")]
    Invalid,
    #[error("I/O: {}", crate::io_text(.0))]
    Io(#[from] std::io::Error),
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(base)
            .join("BorderlessShatter")
            .join("settings.json"),
    )
}

pub fn load(path: &Path) -> Loaded {
    match read(path) {
        Ok((_, settings)) => Loaded {
            settings,
            state: FileState::Valid,
        },
        Err(state) => Loaded {
            settings: Settings::default(),
            state,
        },
    }
}

/// The file's object (unknown fields kept) and its settings, or why it is not usable.
fn read(path: &Path) -> Result<(Map<String, Value>, Settings), FileState> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(FileState::Missing),
        Err(_) => return Err(FileState::Invalid),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FileState::Invalid)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(FileState::Invalid);
    }
    // Notepad and PowerShell may add a UTF-8 BOM to a hand-edited file.
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let Ok(Value::Object(map)) = serde_json::from_slice(body) else {
        return Err(FileState::Invalid);
    };
    let version = map
        .get("version")
        .and_then(Value::as_u64)
        .filter(|&v| v >= 1)
        .ok_or(FileState::Invalid)?;
    if version > VERSION {
        return Err(FileState::Future(version));
    }
    let language = match map.get("language") {
        None => None,
        Some(Value::String(s)) if s == "system" => None,
        Some(Value::String(s)) => Some(Locale::from_code(s).ok_or(FileState::Invalid)?),
        Some(_) => return Err(FileState::Invalid),
    };
    Ok((map, Settings { language }))
}

/// Atomic, durable replace (synced temp file + rename). Refuses to touch a file [`load`] would
/// not accept; unknown fields of a valid file are kept.
pub fn save(path: &Path, settings: &Settings) -> Result<(), SaveError> {
    let mut map = match read(path) {
        Ok((map, _)) => map,
        Err(FileState::Missing) => Map::new(),
        Err(FileState::Future(v)) => return Err(SaveError::FutureVersion(v)),
        Err(_) => return Err(SaveError::Invalid),
    };
    map.insert("version".into(), VERSION.into());
    let language = settings.language.map_or("system", Locale::code);
    map.insert("language".into(), language.into());
    let bytes = serde_json::to_vec_pretty(&Value::Object(map)).map_err(std::io::Error::from)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::write_atomic(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh empty directory per test (pid + name), removed by the test.
    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bs-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_file(name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let d = temp_dir(name);
        let f = d.join("settings.json");
        std::fs::write(&f, bytes).unwrap();
        (d, f)
    }

    const DEFAULTS: Settings = Settings { language: None };

    #[test]
    fn missing_file_gives_defaults() {
        let d = temp_dir("missing");
        let l = load(&d.join("settings.json"));
        assert_eq!(
            l,
            Loaded {
                settings: DEFAULTS,
                state: FileState::Missing
            }
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn valid_files() {
        for (body, want) in [
            (r#"{"version":1,"language":"es"}"#, Some(Locale::Es)),
            (r#"{"version":1,"language":"en"}"#, Some(Locale::En)),
            (r#"{"version":1,"language":"system"}"#, None),
            (r#"{"version":1}"#, None),
            (
                r#"{"version":1,"language":"es","future_field":[1,2]}"#,
                Some(Locale::Es),
            ),
            (
                "\u{feff}{\"version\":1,\"language\":\"en\"}",
                Some(Locale::En),
            ),
        ] {
            let (d, f) = with_file("valid", body.as_bytes());
            assert_eq!(
                load(&f),
                Loaded {
                    settings: Settings { language: want },
                    state: FileState::Valid
                },
                "{body}"
            );
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    const HOSTILE: &[&[u8]] = &[
        b"",
        b"{",
        b"not json",
        b"null",
        b"[]",
        b"\"x\"",
        b"{}",
        br#"{"version":"1","language":"es"}"#,
        br#"{"version":0,"language":"es"}"#,
        br#"{"version":-1,"language":"es"}"#,
        br#"{"version":1.5,"language":"es"}"#,
        br#"{"version":1,"language":5}"#,
        br#"{"version":1,"language":"fr"}"#,
        br#"{"version":1,"language":null}"#,
        b"\xff\xfe{\"version\":1}",
    ];

    #[test]
    fn hostile_files_give_defaults_and_are_never_overwritten() {
        for body in HOSTILE {
            let (d, f) = with_file("hostile", body);
            let l = load(&f);
            assert_eq!(
                l,
                Loaded {
                    settings: DEFAULTS,
                    state: FileState::Invalid
                },
                "{body:?}"
            );
            let r = save(
                &f,
                &Settings {
                    language: Some(Locale::Es),
                },
            );
            assert!(matches!(r, Err(SaveError::Invalid)), "{body:?}: {r:?}");
            assert_eq!(std::fs::read(&f).unwrap(), *body);
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn oversized_file_is_not_read_and_not_overwritten() {
        let mut body = br#"{"version":1,"language":"es","pad":""#.to_vec();
        body.extend(std::iter::repeat_n(b'a', MAX_BYTES as usize));
        body.extend(br#""}"#);
        let (d, f) = with_file("huge", &body);
        assert_eq!(
            load(&f),
            Loaded {
                settings: DEFAULTS,
                state: FileState::Invalid
            }
        );
        assert!(matches!(
            save(&f, &Settings::default()),
            Err(SaveError::Invalid)
        ));
        assert_eq!(std::fs::read(&f).unwrap(), body);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn future_version_gives_defaults_and_is_never_overwritten() {
        let body = br#"{"version":2,"language":"es","new":true}"#;
        let (d, f) = with_file("future", body);
        assert_eq!(
            load(&f),
            Loaded {
                settings: DEFAULTS,
                state: FileState::Future(2)
            }
        );
        let r = save(
            &f,
            &Settings {
                language: Some(Locale::En),
            },
        );
        assert!(matches!(r, Err(SaveError::FutureVersion(2))), "{r:?}");
        assert_eq!(std::fs::read(&f).unwrap(), body);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn directory_in_place_of_file_is_invalid() {
        let d = temp_dir("isdir");
        let f = d.join("settings.json");
        std::fs::create_dir(&f).unwrap();
        assert_eq!(load(&f).state, FileState::Invalid);
        assert!(save(&f, &Settings::default()).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn save_then_load_roundtrip_creates_dirs_and_leaves_no_temp() {
        let d = temp_dir("roundtrip");
        let f = d.join("sub").join("settings.json");
        for want in [Some(Locale::Es), None, Some(Locale::En)] {
            save(&f, &Settings { language: want }).unwrap();
            assert_eq!(
                load(&f),
                Loaded {
                    settings: Settings { language: want },
                    state: FileState::Valid
                }
            );
        }
        let v: Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"version": 1, "language": "en"}));
        let names: Vec<_> = std::fs::read_dir(d.join("sub"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn save_keeps_unknown_fields() {
        let (d, f) = with_file(
            "unknown",
            br#"{"version":1,"language":"es","extra":{"a":1}}"#,
        );
        save(&f, &Settings { language: None }).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"version": 1, "language": "system", "extra": {"a": 1}})
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn io_error_text_carries_only_the_os_code() {
        let e = SaveError::Io(std::io::Error::from_raw_os_error(5));
        assert_eq!(e.to_string(), "I/O: os error 5");
    }
}
