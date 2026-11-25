use crate::window_state::Offsets;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorChoice {
    #[default]
    Current,
    Index(usize),
}

impl MonitorChoice {
    pub fn index(&self) -> Option<usize> {
        match self {
            MonitorChoice::Current => None,
            MonitorChoice::Index(i) => Some(*i),
        }
    }
}

impl Serialize for MonitorChoice {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MonitorChoice::Current => s.serialize_str("current"),
            MonitorChoice::Index(i) => s.serialize_u64(*i as u64),
        }
    }
}

impl<'de> Deserialize<'de> for MonitorChoice {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = MonitorChoice;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"current\" or a non-negative integer")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.eq_ignore_ascii_case("current") {
                    Ok(MonitorChoice::Current)
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(v), &self))
                }
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                usize::try_from(v)
                    .map(MonitorChoice::Index)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Unsigned(v), &self))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(v), &self))
                    .and_then(|u| self.visit_u64(u))
            }
        }
        d.deserialize_any(V)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub process: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub monitor: MonitorChoice,
    #[serde(default)]
    pub offsets: Offsets,
    #[serde(default)]
    pub delay_ms: u64,
    /// Accepted but ignored in v0.1.
    #[serde(default)]
    pub clip_cursor: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileFile {
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON inválido: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("versión {0} no soportada")]
    UnsupportedVersion(u32),
    #[error("la variable APPDATA no está definida")]
    NoAppData,
}

pub fn default_path() -> Result<PathBuf, ProfileError> {
    let base = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .ok_or(ProfileError::NoAppData)?;
    Ok(PathBuf::from(base)
        .join("BorderlessShatter")
        .join("profiles.json"))
}

pub fn parse(s: &str) -> Result<ProfileFile, ProfileError> {
    let file: ProfileFile = serde_json::from_str(s)?;
    if file.version > SUPPORTED_VERSION {
        return Err(ProfileError::UnsupportedVersion(file.version));
    }
    Ok(file)
}

pub fn load_or_create(path: &Path) -> Result<ProfileFile, ProfileError> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = ProfileFile {
            version: SUPPORTED_VERSION,
            profiles: Vec::new(),
        };
        std::fs::write(path, serde_json::to_string_pretty(&file)?)?;
        return Ok(file);
    }
    parse(&std::fs::read_to_string(path)?)
}

const EXCLUDED: &[&str] = &[
    "explorer.exe",
    "dwm.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "textinputhost.exe",
    "applicationframehost.exe",
    "systemsettings.exe",
    "lockapp.exe",
    "taskmgr.exe",
    "borderless-shatter.exe",
];

fn file_name(s: &str) -> &str {
    s.rsplit(['\\', '/']).next().unwrap_or(s)
}

pub fn is_excluded(process_name: &str) -> bool {
    let n = file_name(process_name);
    EXCLUDED.iter().any(|e| e.eq_ignore_ascii_case(n))
}

pub fn find<'a>(file: &'a ProfileFile, process_name: &str) -> Option<&'a Profile> {
    let n = file_name(process_name);
    if is_excluded(n) {
        return None;
    }
    file.profiles
        .iter()
        .find(|p| file_name(&p.process).eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{"version":1,"profiles":[{"process":"game.exe","enabled":false,
      "monitor":"current","offsets":{"left":1,"top":2,"right":3,"bottom":4},
      "delay_ms":3000,"clip_cursor":true}]}"#;

    #[test]
    fn full_example() {
        let f = parse(FULL).unwrap();
        let p = &f.profiles[0];
        assert!(!p.enabled);
        assert_eq!(
            p.offsets,
            Offsets {
                left: 1,
                top: 2,
                right: 3,
                bottom: 4
            }
        );
        assert_eq!(p.delay_ms, 3000);
        assert!(p.clip_cursor);
    }

    #[test]
    fn defaults() {
        let f = parse(r#"{"version":1,"profiles":[{"process":"a.exe"}]}"#).unwrap();
        let p = &f.profiles[0];
        assert!(p.enabled);
        assert_eq!(p.monitor, MonitorChoice::Current);
        assert_eq!(p.offsets, Offsets::default());
        assert_eq!(p.delay_ms, 0);
    }

    #[test]
    fn unknown_fields_ignored() {
        assert!(parse(r#"{"version":1,"x":1,"profiles":[{"process":"a.exe","y":2}]}"#).is_ok());
    }

    #[test]
    fn monitor_forms() {
        let m = |v: &str| {
            parse(&format!(
                r#"{{"version":1,"profiles":[{{"process":"a","monitor":{v}}}]}}"#
            ))
        };
        assert_eq!(m("1").unwrap().profiles[0].monitor, MonitorChoice::Index(1));
        assert_eq!(
            m("\"CURRENT\"").unwrap().profiles[0].monitor,
            MonitorChoice::Current
        );
        assert!(m("-1").is_err());
    }

    #[test]
    fn version_and_broken() {
        assert!(matches!(
            parse(r#"{"version":2}"#),
            Err(ProfileError::UnsupportedVersion(2))
        ));
        assert!(matches!(parse("{"), Err(ProfileError::Parse(_))));
    }

    #[test]
    fn find_matching() {
        let f = parse(
            r#"{"version":1,"profiles":[{"process":"game.exe"},{"process":"explorer.exe"}]}"#,
        )
        .unwrap();
        assert!(find(&f, "C:\\Games\\GAME.EXE").is_some());
        assert!(find(&f, "explorer.exe").is_none());
    }

    #[test]
    fn load_or_create_paths() {
        let dir = std::env::temp_dir().join(format!("bs-profiles-test-{}", std::process::id()));
        let path = dir.join("sub").join("profiles.json");
        let f = load_or_create(&path).unwrap();
        assert!(f.profiles.is_empty() && path.exists());
        std::fs::write(&path, "garbage").unwrap();
        assert!(load_or_create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
