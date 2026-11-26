//! On-disk record of managed windows (`%LOCALAPPDATA%\BorderlessShatter\state.json`), so a crash
//! of the resident process does not lose the original state needed to restore them.
//!
//! With `BORDERLESS_SHATTER_INSTANCE=<s>` the file is `state.<s>.json`, since it lists the windows
//! that one resident manages; `profiles.json` and `settings.json` stay shared by every instance.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::window_state::{Rect, WindowState};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not locate %LOCALAPPDATA%")]
    NoLocalAppData,
    #[error("I/O error on {}: {}", .path.display(), crate::io_text(.source))]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the state file {path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Instance(#[from] crate::ipc::pipe::InvalidInstance),
}

/// One managed window: `hwnd` + `pid` identify it across restarts, `state` is the original state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEntry {
    pub hwnd: isize,
    pub pid: u32,
    pub process: String,
    pub state: WindowState,
    /// Rect the window was forced to; absent in files written before v0.2 re-adoption.
    #[serde(default)]
    pub target: Option<Rect>,
    #[serde(default)]
    pub island: Option<String>,
}

/// File layout; the wrapper leaves room for future fields without breaking old files.
#[derive(Serialize, Deserialize)]
struct File {
    entries: Vec<StoredEntry>,
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn default_path() -> Result<PathBuf, StoreError> {
        let base = std::env::var_os("LOCALAPPDATA").ok_or(StoreError::NoLocalAppData)?;
        Ok(path_in(
            Path::new(&base),
            crate::ipc::pipe::instance_suffix()?.as_deref(),
        ))
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open(path: PathBuf) -> Self {
        Self { path }
    }

    /// A missing file means nothing is stored.
    pub fn load(&self) -> Result<Vec<StoredEntry>, StoreError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_str::<File>(&text)
            .map(|f| f.entries)
            .map_err(|source| StoreError::Parse {
                path: self.path.clone(),
                source,
            })
    }

    /// Replaces the file through a synced temp file, so readers never see a torn file.
    pub fn save(&self, entries: &[StoredEntry]) -> Result<(), StoreError> {
        let io = |source: std::io::Error| StoreError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let json = serde_json::to_string_pretty(&File {
            entries: entries.to_vec(),
        })
        .map_err(|source| StoreError::Parse {
            path: self.path.clone(),
            source,
        })?;
        crate::write_atomic(&self.path, json.as_bytes()).map_err(io)
    }
}

/// `<base>\BorderlessShatter\state[.<suffix>].json`.
fn path_in(base: &Path, suffix: Option<&str>) -> PathBuf {
    let name = suffix.map_or_else(|| "state.json".to_owned(), |s| format!("state.{s}.json"));
    base.join("BorderlessShatter").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!("bs-state-{tag}-{}-{nanos}", std::process::id()))
            .join("state.json")
    }

    fn entry(hwnd: isize) -> StoredEntry {
        StoredEntry {
            hwnd,
            pid: 4242,
            process: "PWAAT.exe".into(),
            state: WindowState {
                hwnd,
                style: 0x14CA_0000,
                exstyle: 0x100,
                rect: Rect {
                    left: 231,
                    top: 223,
                    right: 1516,
                    bottom: 971,
                },
                maximized: false,
            },
            target: None,
            island: None,
        }
    }

    #[test]
    fn state_file_follows_the_instance_suffix() {
        let base = Path::new(r"C:\Local");
        assert_eq!(
            path_in(base, None),
            Path::new(r"C:\Local\BorderlessShatter\state.json")
        );
        assert_eq!(
            path_in(base, Some("fix-store")),
            Path::new(r"C:\Local\BorderlessShatter\state.fix-store.json")
        );
    }

    #[test]
    fn store_reports_its_own_path() {
        let path = temp_path("path");
        assert_eq!(StateStore::open(path.clone()).path(), path);
    }

    #[test]
    fn io_error_text_carries_only_the_os_code() {
        let e = StoreError::Io {
            path: PathBuf::from(r"C:\s.json"),
            source: std::io::Error::from_raw_os_error(5),
        };
        assert_eq!(e.to_string(), r"I/O error on C:\s.json: os error 5");
    }

    #[test]
    fn missing_file_is_empty() {
        let store = StateStore::open(temp_path("missing"));
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn round_trip_overwrites_and_leaves_no_temp() {
        let path = temp_path("rt");
        let store = StateStore::open(path.clone());
        store.save(&[entry(1), entry(2)]).unwrap();
        assert_eq!(store.load().unwrap(), vec![entry(1), entry(2)]);
        store.save(&[entry(3)]).unwrap();
        assert_eq!(store.load().unwrap(), vec![entry(3)]);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn old_file_without_target_and_island_loads() {
        let path = temp_path("old");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut old = serde_json::to_value(File {
            entries: vec![entry(5)],
        })
        .unwrap();
        let e = old["entries"][0].as_object_mut().unwrap();
        e.remove("target");
        e.remove("island");
        std::fs::write(&path, old.to_string()).unwrap();
        assert_eq!(
            StateStore::open(path.clone()).load().unwrap(),
            vec![entry(5)]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn target_and_island_round_trip() {
        let path = temp_path("ti");
        let store = StateStore::open(path.clone());
        let mut e = entry(6);
        e.target = Some(Rect {
            left: 2560,
            top: 0,
            right: 5120,
            bottom: 1440,
        });
        e.island = Some("halves-16-9/right".into());
        store.save(&[e.clone()]).unwrap();
        assert_eq!(store.load().unwrap(), vec![e]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_is_a_parse_error() {
        let path = temp_path("bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{nope").unwrap();
        assert!(matches!(
            StateStore::open(path.clone()).load(),
            Err(StoreError::Parse { .. })
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
