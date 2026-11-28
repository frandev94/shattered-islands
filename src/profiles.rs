//! The `profiles.json` model: per-game profiles, user layouts, validation and atomic persistence.
//!
//! Offsets and sizes are physical pixels; island fractions are `0..1` with at most 6 decimals.
//! Unknown JSON fields of the file and of each profile, layout and island are kept (`extra`); see
//! [`upsert`] for how a replace carries them over. A file newer than [`SUPPORTED_VERSION`] is rejected;
//! a key repeated in any object or an array where an object is expected is [`ProfileError::Parse`].
//! [`Warning::code`] values and `ErrorBody` codes are the stable contract, not message texts.
//!
//! ```
//! use borderless_shatter::profiles::{find, parse, validate};
//!
//! let file = parse(r#"{"version":2,"profiles":[{"process":"Game.exe","island":"halves-16-9/left"},
//!     {"process":"game.exe","title":"Launcher","enabled":false}]}"#)
//!     .unwrap();
//! assert!(validate(&file).is_empty());
//! // Lookup compares the file name case-insensitively and ignores the directory.
//! assert!(find(&file.profiles, r"C:\games\game.EXE", "Game").unwrap().enabled);
//! // A title rule beats the process-only profile; a disabled winner leaves the window alone.
//! assert!(!find(&file.profiles, "game.exe", "launcher").unwrap().enabled);
//! assert!(find(&file.profiles, "explorer.exe", "").is_none());
//! ```

use crate::window_state::Offsets;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

/// Highest `profiles.json` schema version this build reads; files are always written as this.
pub const SUPPORTED_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// Which monitor a profile targets. JSON: `"current"` or a 0-based integer index.
pub enum MonitorChoice {
    /// The monitor the window is currently on. The default.
    #[default]
    Current,
    /// 0-based monitor index.
    Index(usize),
}

impl MonitorChoice {
    /// The explicit index, or `None` for [`MonitorChoice::Current`].
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

impl JsonSchema for MonitorChoice {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MonitorChoice".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                { "type": "integer", "minimum": 0 },
                { "type": "string", "enum": ["current"] }
            ]
        })
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// How one game's window is managed.
pub struct Profile {
    /// Process executable name; matched on the file name, case-insensitively.
    pub process: String,
    /// Exact window title, compared trimmed and case-insensitively. Excludes `title_regex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Window title regular expression (`regex` crate syntax), searched anywhere in the title:
    /// `^…$` for the whole title, `(?i)` to ignore case. Excludes `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
    /// Disabled profiles are kept but not applied. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Target monitor, unless `island` is set.
    #[serde(default)]
    pub monitor: MonitorChoice,
    /// Per-edge adjustment in physical pixels, unless `island` is set.
    #[serde(default)]
    pub offsets: Offsets,
    /// Milliseconds to wait after the window appears before applying.
    #[serde(default)]
    pub delay_ms: u64,
    /// Accepted but ignored in v0.1.
    #[serde(default)]
    pub clip_cursor: bool,
    /// "layout/island"; overrides `monitor` and `offsets`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub island: Option<String>,
    /// Declared game resolution in physical pixels; the window is centred in the island.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_resolution: Option<crate::islands::Size>,
    /// Fields this version does not know, written back unchanged.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// Whole `profiles.json` content.
pub struct ProfileFile {
    /// Schema version (1 or 2).
    pub version: u32,
    /// User layouts; merged over the built-in presets.
    #[serde(default)]
    pub layouts: Vec<crate::islands::Layout>,
    /// Per-game profiles.
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// Fields this version does not know, written back unchanged.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Non-fatal validation finding; `code` is stable, `data` carries context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Warning {
    /// Stable machine-readable identifier, e.g. `bad_island_ref`.
    pub code: String,
    /// Context for the code (process, island, ...).
    pub data: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
/// Failure loading, parsing or saving `profiles.json`.
pub enum ProfileError {
    /// Reading or writing the file failed (including non-UTF-8 content).
    #[error("I/O: {}", crate::io_text(.0))]
    Io(#[from] std::io::Error),
    /// The content is not valid profile JSON.
    #[error("invalid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    /// The file's version is newer than [`SUPPORTED_VERSION`].
    #[error("version {0} not supported")]
    UnsupportedVersion(u32),
    /// `%APPDATA%` is unset or empty, so [`default_path`] cannot be built.
    #[error("the APPDATA variable is not set")]
    NoAppData,
    /// The file changed on disk since the caller read it (optimistic-concurrency check).
    #[error("the file revision has changed")]
    RevisionConflict,
    /// The file is UTF-16 (byte order mark `FF FE` or `FE FF`); only UTF-8 is read.
    #[error("the file is UTF-16 encoded; save it as UTF-8")]
    Utf16,
}

/// `%APPDATA%\BorderlessShatter\profiles.json`, shared by every `BORDERLESS_SHATTER_INSTANCE`.
pub fn default_path() -> Result<PathBuf, ProfileError> {
    let base = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .ok_or(ProfileError::NoAppData)?;
    Ok(PathBuf::from(base)
        .join("BorderlessShatter")
        .join("profiles.json"))
}

/// Parses profile JSON, rejecting versions above [`SUPPORTED_VERSION`]. A key repeated in any object
/// or an array where an object is expected is [`ProfileError::Parse`]; a newer version wins over both.
///
/// ```
/// use borderless_shatter::profiles::{parse, ProfileError};
///
/// assert!(matches!(parse(r#"{"version":99}"#), Err(ProfileError::UnsupportedVersion(99))));
/// assert!(matches!(parse(r#"{"version":2,"_a":1,"_a":2}"#), Err(ProfileError::Parse(_))));
/// ```
pub fn parse(s: &str) -> Result<ProfileFile, ProfileError> {
    let file: ProfileFile = serde_json::from_str(s)?;
    if file.version > SUPPORTED_VERSION {
        return Err(ProfileError::UnsupportedVersion(file.version));
    }
    // Same rule as IPC: serde keeps the last repeated unknown key and reads structs by position.
    crate::ipc::check_unique_keys(s.as_bytes())?;
    let value: serde_json::Value = serde_json::from_str(s)?;
    crate::ipc::check_arrays::<ProfileFile>(&value)
        .map_err(<serde_json::Error as serde::de::Error>::custom)?;
    Ok(file)
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

/// True for shell and system processes (and this app) that must never be managed.
/// Accepts a bare name or a path.
pub fn is_excluded(process_name: &str) -> bool {
    let n = file_name(process_name);
    EXCLUDED.iter().any(|e| e.eq_ignore_ascii_case(n))
}

/// Trimmed and lowercased: how exact titles are compared, in matching and in [`ProfileKey`].
fn norm_title(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Compiled-size cap of a `title_regex` (the `regex` default is 10 MiB): a pattern over it fails
/// to compile, so it is reported and never matches. 1 MiB still takes `^[\w ]{3,20}$` (256 KiB
/// does not) and bounds the one-time compile to a few milliseconds.
const REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// Patterns kept compiled per thread; the cache starts over when it is full.
const REGEX_CACHE_MAX: usize = 64;

thread_local! {
    static REGEXES: std::cell::RefCell<std::collections::HashMap<String, Option<regex::Regex>>> =
        Default::default();
}

/// `pattern` compiled under [`REGEX_SIZE_LIMIT`], once per thread; `None` if it is empty or does
/// not compile.
fn compiled(pattern: &str) -> Option<regex::Regex> {
    if pattern.is_empty() {
        return None;
    }
    REGEXES.with_borrow_mut(|cache| {
        if let Some(re) = cache.get(pattern) {
            return re.clone();
        }
        if cache.len() >= REGEX_CACHE_MAX {
            cache.clear();
        }
        let re = regex::RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .ok();
        cache.insert(pattern.to_owned(), re.clone());
        re
    })
}

/// Whether `pattern` is a usable `title_regex`: not empty, compiles, and within the 1 MiB
/// compiled-size limit.
///
/// ```
/// assert!(borderless_shatter::profiles::title_regex_ok("^Game \\d+$"));
/// assert!(!borderless_shatter::profiles::title_regex_ok("(oops"));
/// ```
pub fn title_regex_ok(pattern: &str) -> bool {
    compiled(pattern).is_some()
}

/// Precedence of `p` for a window titled `title`: 0 exact title, 1 regex, 2 process only; `None`
/// when its rule does not match or is invalid (empty, broken regex, both rules).
fn rank(p: &Profile, title: &str) -> Option<u8> {
    match (&p.title, &p.title_regex) {
        (None, None) => Some(2),
        (Some(t), None) => {
            (!t.trim().is_empty() && norm_title(t) == norm_title(title)).then_some(0)
        }
        (None, Some(r)) => compiled(r)
            .is_some_and(|re| re.is_match(title))
            .then_some(1),
        (Some(_), Some(_)) => None,
    }
}

/// The best profile for a window of `process_name` (name or path, case-insensitive) titled
/// `title`: among the process's profiles whose rule matches, an exact `title` beats a
/// `title_regex`, which beats a process-only profile; ties go to file order. Disabled profiles
/// compete too (a disabled winner means "leave the window alone"). `None` if none matches or the
/// process is excluded.
pub fn find<'a>(profiles: &'a [Profile], process_name: &str, title: &str) -> Option<&'a Profile> {
    let n = file_name(process_name);
    if is_excluded(n) {
        return None;
    }
    profiles
        .iter()
        .filter(|p| file_name(&p.process).eq_ignore_ascii_case(n))
        .filter_map(|p| rank(p, title).map(|r| (r, p)))
        .min_by_key(|&(r, _)| r)
        .map(|(_, p)| p)
}

/// Identity of a profile: upsert, remove and duplicate checks go by it. Built normalized by
/// [`ProfileKey::new`] / [`ProfileKey::of`] (the process file name lowercased, `title` trimmed and
/// lowercased, `title_regex` as is), so `==` is the identity; build it with those, not a literal.
/// JSON: a process name (the process-only key) or `{process, title?, title_regex?}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "KeyRepr", into = "KeyRepr")]
pub struct ProfileKey {
    /// Process file name, lowercased.
    pub process: String,
    /// Exact title rule, trimmed and lowercased.
    pub title: Option<String>,
    /// Regex title rule, as written.
    pub title_regex: Option<String>,
}

impl ProfileKey {
    /// The normalized key of a profile with these fields.
    pub fn new(process: &str, title: Option<&str>, title_regex: Option<&str>) -> Self {
        ProfileKey {
            process: file_name(process).to_ascii_lowercase(),
            title: title.map(norm_title),
            title_regex: title_regex.map(str::to_owned),
        }
    }

    /// The key of `p`.
    pub fn of(p: &Profile) -> Self {
        Self::new(&p.process, p.title.as_deref(), p.title_regex.as_deref())
    }
}

impl From<&str> for ProfileKey {
    /// The process-only key of `process`.
    fn from(process: &str) -> Self {
        Self::new(process, None, None)
    }
}

/// Wire form of [`ProfileKey`].
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum KeyRepr {
    /// A process name: its process-only profile.
    Process(String),
    /// One profile by process and title rule.
    Rule(RuleRepr),
}

/// One profile by process and title rule. A misspelt field is an error: ignoring it would turn
/// the key into the process-only one, and a `remove` would delete that profile.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RuleRepr {
    /// Process executable name.
    process: String,
    /// Exact title rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Regex title rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title_regex: Option<String>,
}

impl From<KeyRepr> for ProfileKey {
    fn from(r: KeyRepr) -> Self {
        match r {
            KeyRepr::Process(p) => Self::new(&p, None, None),
            KeyRepr::Rule(RuleRepr {
                process,
                title,
                title_regex,
            }) => Self::new(&process, title.as_deref(), title_regex.as_deref()),
        }
    }
}

impl From<ProfileKey> for KeyRepr {
    fn from(k: ProfileKey) -> Self {
        match k {
            ProfileKey {
                process,
                title: None,
                title_regex: None,
            } => KeyRepr::Process(process),
            ProfileKey {
                process,
                title,
                title_regex,
            } => KeyRepr::Rule(RuleRepr {
                process,
                title,
                title_regex,
            }),
        }
    }
}

impl JsonSchema for ProfileKey {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProfileKey".into()
    }

    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        KeyRepr::json_schema(g)
    }
}

/// Lowercase hex SHA-256 of the raw file bytes.
///
/// ```
/// assert_eq!(
///     borderless_shatter::profiles::revision(b""),
///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
/// );
/// ```
pub fn revision(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn warn(code: &str, data: serde_json::Value) -> Warning {
    Warning {
        code: code.into(),
        data,
    }
}

/// Non-fatal findings for `file`: profile references, game resolutions and layout ids, rects and
/// duplicates. Codes are stable; see [`is_blocking`] for those that reject a write.
///
/// ```
/// use borderless_shatter::profiles::{parse, validate};
///
/// let file = parse(r#"{"version":2,"profiles":[{"process":"a.exe","island":"nope/x"}]}"#).unwrap();
/// assert_eq!(validate(&file)[0].code, "island_not_found");
/// ```
pub fn validate(file: &ProfileFile) -> Vec<Warning> {
    let layouts = crate::islands::merged_layouts(&file.layouts);
    let mut out = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for p in &file.profiles {
        // `process`, plus the title rule when the profile has one.
        let data = |more: serde_json::Value| {
            let mut d = serde_json::json!({ "process": p.process });
            if let Some(t) = &p.title {
                d["title"] = t.as_str().into();
            }
            if let Some(r) = &p.title_regex {
                d["title_regex"] = r.as_str().into();
            }
            if let (Some(d), serde_json::Value::Object(more)) = (d.as_object_mut(), more) {
                d.extend(more);
            }
            d
        };
        let none = || serde_json::json!({});
        if !keys.insert(ProfileKey::of(p)) {
            out.push(warn("duplicate_profile", data(none())));
        }
        if p.title.as_ref().is_some_and(|t| t.trim().is_empty()) {
            out.push(warn("bad_title", data(none())));
        }
        if p.title_regex.as_ref().is_some_and(|r| !title_regex_ok(r)) {
            out.push(warn("bad_title_regex", data(none())));
        }
        if p.title.is_some() && p.title_regex.is_some() {
            out.push(warn("title_rule_conflict", data(none())));
        }
        if let Some(r) = &p.island {
            if p.monitor != MonitorChoice::Current || p.offsets != Offsets::default() {
                out.push(warn(
                    "island_overrides_monitor",
                    data(serde_json::json!({ "island": r })),
                ));
            }
            match crate::islands::find_island(&layouts, r) {
                Ok(_) => {}
                Err(crate::islands::IslandError::BadRef(_)) => out.push(warn(
                    "bad_island_ref",
                    data(serde_json::json!({ "island": r })),
                )),
                Err(_) => out.push(warn(
                    "island_not_found",
                    data(serde_json::json!({ "island": r })),
                )),
            }
        }
        if let Some(s) = p.game_resolution
            && (s.width <= 0 || s.height <= 0)
        {
            out.push(warn(
                "bad_game_resolution",
                data(serde_json::json!({ "width": s.width, "height": s.height })),
            ));
        }
    }
    validate_layouts(&file.layouts, &mut out);
    out
}

/// Layout ids and island ids must be usable in a `layout/island` reference.
fn bad_id(s: &str) -> bool {
    s.is_empty() || s.contains('/')
}

/// Upper bound for pixel-unit islands; far above any real monitor.
const MAX_PX: f64 = 100_000.0;

fn rect_ok(r: &crate::islands::IslandRect) -> bool {
    let all = [r.x, r.y, r.w, r.h];
    if !all.iter().all(|v| v.is_finite()) || r.w <= 0.0 || r.h <= 0.0 || r.x < 0.0 || r.y < 0.0 {
        return false;
    }
    match r.unit {
        crate::islands::Unit::Fraction => r.x + r.w <= 1.0 && r.y + r.h <= 1.0,
        crate::islands::Unit::Px => r.x + r.w <= MAX_PX && r.y + r.h <= MAX_PX,
    }
}

fn validate_layouts(layouts: &[crate::islands::Layout], out: &mut Vec<Warning>) {
    let mut seen = std::collections::HashSet::new();
    for l in layouts {
        if bad_id(&l.id) {
            out.push(warn("bad_layout_id", serde_json::json!({ "layout": l.id })));
        } else if !seen.insert(l.id.as_str()) {
            out.push(warn(
                "duplicate_layout",
                serde_json::json!({ "layout": l.id }),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for i in &l.islands {
            let d = serde_json::json!({ "layout": l.id, "island": i.id });
            if bad_id(&i.id) {
                out.push(warn("bad_island_id", d));
                continue;
            }
            if !ids.insert(i.id.as_str()) {
                out.push(warn("duplicate_island", d.clone()));
            }
            if !rect_ok(&i.rect) {
                out.push(warn("bad_island_rect", d));
            }
        }
    }
}

/// Whether a [`Warning::code`] is blocking.
///
/// Codes that make `set_profiles` reject the write (layout codes always; profile codes only for
/// profile keys the call wrote). `island_not_found` and `duplicate_profile` are informational.
pub fn is_blocking(code: &str) -> bool {
    matches!(
        code,
        "island_overrides_monitor"
            | "bad_island_ref"
            | "bad_game_resolution"
            | "bad_title"
            | "bad_title_regex"
            | "title_rule_conflict"
            | "bad_layout_id"
            | "duplicate_layout"
            | "bad_island_id"
            | "duplicate_island"
            | "bad_island_rect"
    )
}

/// Upsert by [`ProfileKey`] (the first profile with the same key is replaced) / layout `id`, then
/// drop every profile whose key is in `remove` and the layouts in `remove_layouts`. A process-only
/// key leaves the title-rule profiles of that process alone.
/// With `replace_extras` false:
/// - A replaced profile or layout that has no unknown keys (`extra`) keeps the replaced item's keys.
/// - Each island of a replaced layout does the same, matched by `id` inside the replaced layout.
/// - An item that brings its own keys replaces the old set whole.
///
/// With `replace_extras` true, every sent item is stored exactly as sent.
/// Items not sent keep their keys.
///
/// ```
/// use borderless_shatter::profiles::{parse, upsert};
///
/// let mut f = parse(r#"{"version":2,"profiles":[{"process":"a.exe"},{"process":"a.exe","title":"T"}]}"#)
///     .unwrap();
/// upsert(&mut f, vec![], vec![], vec!["A.EXE".into()], vec![], false);
/// assert_eq!(f.profiles.len(), 1);
/// assert_eq!(f.profiles[0].title.as_deref(), Some("T"));
/// ```
pub fn upsert(
    file: &mut ProfileFile,
    profiles: Vec<Profile>,
    layouts: Vec<crate::islands::Layout>,
    remove: Vec<ProfileKey>,
    remove_layouts: Vec<String>,
    replace_extras: bool,
) {
    for p in profiles {
        let key = ProfileKey::of(&p);
        match file.profiles.iter_mut().find(|x| ProfileKey::of(x) == key) {
            Some(slot) => {
                let old = std::mem::replace(slot, p);
                if slot.extra.is_empty() && !replace_extras {
                    slot.extra = old.extra;
                }
            }
            None => file.profiles.push(p),
        }
    }
    for mut l in layouts {
        match file.layouts.iter_mut().find(|x| x.id == l.id) {
            Some(slot) => {
                if !replace_extras {
                    if l.extra.is_empty() {
                        l.extra = std::mem::take(&mut slot.extra);
                    }
                    for i in l.islands.iter_mut().filter(|i| i.extra.is_empty()) {
                        if let Some(old) = slot.islands.iter_mut().find(|o| o.id == i.id) {
                            i.extra = std::mem::take(&mut old.extra);
                        }
                    }
                }
                *slot = l;
            }
            None => file.layouts.push(l),
        }
    }
    if !remove.is_empty() {
        file.profiles
            .retain(|p| !remove.contains(&ProfileKey::of(p)));
    }
    file.layouts.retain(|l| !remove_layouts.contains(&l.id));
}

/// Writes version 2 durably (synced temp file + rename). The replaced file is kept as
/// `<file>.prev` (best effort), and a v1 file is also copied to `profiles.v1.bak` (once).
///
/// ```
/// use borderless_shatter::profiles::{load, save_atomic};
///
/// let dir = std::env::temp_dir().join(format!("bs-doctest-profiles-{}", std::process::id()));
/// let path = dir.join("profiles.json");
/// let (mut file, _) = load(&path).unwrap(); // a missing file is created empty
/// file.profiles.clear();
/// save_atomic(&path, &file).unwrap();
/// assert_eq!(load(&path).unwrap().0.version, 2);
/// std::fs::remove_dir_all(&dir).unwrap();
/// ```
pub fn save_atomic(path: &Path, file: &ProfileFile) -> Result<(), ProfileError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Ok(old) = std::fs::read(path) {
        let bak = path.with_file_name("profiles.v1.bak");
        if !bak.exists()
            && text_of(&old)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| v.get("version").and_then(|n| n.as_u64()))
                == Some(1)
        {
            crate::write_atomic(&bak, &old)?;
        }
        let mut prev = path.as_os_str().to_owned();
        prev.push(".prev");
        // A failed copy only loses the older revision; the new one is still written atomically.
        let _ = crate::write_atomic(Path::new(&prev), &old);
    }
    let out = ProfileFile {
        version: SUPPORTED_VERSION,
        ..file.clone()
    };
    crate::write_atomic(path, serde_json::to_string_pretty(&out)?.as_bytes())?;
    Ok(())
}

/// The UTF-8 text of `bytes` without its byte order mark, if any; UTF-16 is refused.
fn text_of(bytes: &[u8]) -> Result<&str, ProfileError> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(ProfileError::Utf16);
    }
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    std::str::from_utf8(body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e).into())
}

/// Loads the file and its revision; a missing file is created empty (v2). Invalid files (see
/// [`parse`]: a key repeated in any object or an array where an object is expected is
/// [`ProfileError::Parse`]) are never touched. A UTF-8 byte order mark is accepted; UTF-16 gives
/// [`ProfileError::Utf16`].
pub fn load(path: &Path) -> Result<(ProfileFile, String), ProfileError> {
    if !path.exists() {
        let file = ProfileFile {
            version: SUPPORTED_VERSION,
            layouts: Vec::new(),
            profiles: Vec::new(),
            extra: serde_json::Map::new(),
        };
        save_atomic(path, &file)?;
    }
    let bytes = std::fs::read(path)?;
    Ok((parse(text_of(&bytes)?)?, revision(&bytes)))
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
            parse(r#"{"version":3}"#),
            Err(ProfileError::UnsupportedVersion(3))
        ));
        assert!(matches!(parse("{"), Err(ProfileError::Parse(_))));
    }

    #[test]
    fn find_matching() {
        let f = parse(
            r#"{"version":1,"profiles":[{"process":"game.exe"},{"process":"explorer.exe"}]}"#,
        )
        .unwrap();
        assert!(find(&f.profiles, "C:\\Games\\GAME.EXE", "").is_some());
        assert!(find(&f.profiles, "explorer.exe", "").is_none());
    }

    fn profiles_of(json: &str) -> Vec<Profile> {
        parse(&format!(r#"{{"version":2,"profiles":{json}}}"#))
            .unwrap()
            .profiles
    }

    #[test]
    fn find_exact_title_ignores_case_and_outer_spaces() {
        let ps = profiles_of(r#"[{"process":"g.exe","title":"  Main Window "}]"#);
        assert!(find(&ps, "g.exe", "main window").is_some());
        assert!(find(&ps, "g.exe", " MAIN WINDOW\t").is_some());
        assert!(find(&ps, "g.exe", "Main Window 2").is_none());
        assert!(find(&ps, "other.exe", "Main Window").is_none());
    }

    #[test]
    fn find_regex_is_unanchored() {
        let ps = profiles_of(r#"[{"process":"g.exe","title_regex":"Lev\\d"}]"#);
        assert!(find(&ps, "g.exe", "Game - Lev3 - 60 FPS").is_some());
        assert!(find(&ps, "g.exe", "game - lev3").is_none());
        let anchored = profiles_of(r#"[{"process":"g.exe","title_regex":"(?i)^lev\\d$"}]"#);
        assert!(find(&anchored, "g.exe", "LEV3").is_some());
        assert!(find(&anchored, "g.exe", "x LEV3").is_none());
    }

    #[test]
    fn find_invalid_or_empty_rule_never_matches() {
        for rule in [
            r#""title_regex":"(""#,
            r#""title_regex":"""#,
            r#""title":"  ""#,
            r#""title":"x","title_regex":"x""#,
        ] {
            let ps = profiles_of(&format!(r#"[{{"process":"g.exe",{rule}}}]"#));
            for title in ["", "  ", "x", "("] {
                assert!(find(&ps, "g.exe", title).is_none(), "{rule} {title:?}");
            }
        }
    }

    #[test]
    fn find_excluded_process_ignores_title_rules() {
        let ps = profiles_of(
            r#"[{"process":"explorer.exe","title":"Desktop"},{"process":"explorer.exe","title_regex":"."}]"#,
        );
        assert!(find(&ps, "C:\\Windows\\explorer.exe", "Desktop").is_none());
    }

    #[test]
    fn find_precedence_exact_then_regex_then_process_then_file_order() {
        let ps = profiles_of(
            r#"[{"process":"g.exe","delay_ms":1},
                {"process":"g.exe","title_regex":"^Ga","delay_ms":2},
                {"process":"g.exe","title_regex":"me$","delay_ms":3},
                {"process":"g.exe","title":"game","delay_ms":4},
                {"process":"g.exe","title":"GAME","delay_ms":5}]"#,
        );
        let delay = |title: &str| find(&ps, "g.exe", title).map(|p| p.delay_ms);
        assert_eq!(delay("Game"), Some(4));
        assert_eq!(delay("Gallery"), Some(2));
        assert_eq!(delay("Frame"), Some(3));
        assert_eq!(delay("Launcher"), Some(1));
    }

    #[test]
    fn find_disabled_best_match_shadows_the_process_profile() {
        let ps = profiles_of(
            r#"[{"process":"g.exe"},{"process":"g.exe","title":"Launcher","enabled":false}]"#,
        );
        assert!(!find(&ps, "g.exe", "launcher").unwrap().enabled);
        assert!(find(&ps, "g.exe", "Game").unwrap().enabled);
    }

    #[test]
    fn title_regex_ok_rejects_empty_and_broken() {
        assert!(title_regex_ok("^Game \\d+$"));
        assert!(!title_regex_ok(""));
        assert!(!title_regex_ok("(unclosed"));
    }

    #[test]
    fn a_title_regex_too_big_to_compile_cheaply_is_rejected_and_never_matches() {
        // Fits the regex crate's default size limit, not ours.
        let huge = r"\w{50}";
        assert!(!title_regex_ok(huge));
        let ps = profiles_of(r#"[{"process":"g.exe","title_regex":"\\w{50}"}]"#);
        assert!(find(&ps, "g.exe", &"a".repeat(60)).is_none());
        // Ordinary patterns still compile.
        assert!(title_regex_ok(r"(?i)^.*game - level \d+ .*$"));
        assert!(title_regex_ok(r"^[\w ]{3,20}$"));
    }

    #[test]
    fn profile_key_object_rejects_unknown_fields() {
        // A misspelt rule must not fall back to the process-only key (a remove would delete it).
        for bad in [
            r#"{"process":"a.exe","titel":"Menu"}"#,
            r#"{"process":"a.exe","Title":"Menu"}"#,
            r#"{"process":"a.exe","regex":"^M"}"#,
        ] {
            assert!(serde_json::from_str::<ProfileKey>(bad).is_err(), "{bad}");
        }
        let schema = schemars::SchemaGenerator::default().into_root_schema_for::<ProfileKey>();
        let text = schema.as_value().to_string();
        assert!(text.contains(r#""additionalProperties":false"#), "{text}");
    }

    #[test]
    fn profile_without_rule_serializes_as_before() {
        let text = r#"{"process":"a.exe","enabled":true,"monitor":"current","offsets":{"left":0,"top":0,"right":0,"bottom":0},"delay_ms":0,"clip_cursor":false}"#;
        let p: Profile = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&p).unwrap(), text);
        let ruled = profiles_of(
            r#"[{"process":"a.exe","title":"T"},{"process":"b.exe","title_regex":"R"}]"#,
        );
        assert_eq!(ruled[0].title.as_deref(), Some("T"));
        assert_eq!(ruled[1].title_regex.as_deref(), Some("R"));
        assert!(ruled[0].extra.is_empty() && ruled[1].extra.is_empty());
    }

    #[test]
    fn profile_key_normalizes_process_and_title_but_not_regex() {
        let ps = profiles_of(
            r#"[{"process":"C:\\G\\Game.EXE","title":" Main "},{"process":"g.exe","title_regex":"A"}]"#,
        );
        assert_eq!(
            ProfileKey::of(&ps[0]),
            ProfileKey::new("game.exe", Some("main"), None)
        );
        assert_ne!(
            ProfileKey::of(&ps[1]),
            ProfileKey::new("g.exe", None, Some("a"))
        );
        let keys: Vec<ProfileKey> = serde_json::from_str(
            r#"["GAME.exe",{"process":"game.exe","title":"MAIN"},{"process":"g.exe","title_regex":"A","title":null}]"#,
        )
        .unwrap();
        assert_eq!(
            keys,
            [
                ProfileKey::new("game.exe", None, None),
                ProfileKey::of(&ps[0]),
                ProfileKey::of(&ps[1]),
            ]
        );
        assert!(serde_json::from_str::<ProfileKey>("1").is_err());
        assert!(serde_json::from_str::<ProfileKey>(r#"{"title":"x"}"#).is_err());
        assert_eq!(serde_json::to_string(&keys[0]).unwrap(), r#""game.exe""#);
    }

    fn codes_and_data(json: &str) -> Vec<(String, serde_json::Value)> {
        let f = parse(&format!(r#"{{"version":2,"profiles":{json}}}"#)).unwrap();
        validate(&f).into_iter().map(|w| (w.code, w.data)).collect()
    }

    #[test]
    fn validate_title_rules() {
        use serde_json::json;
        assert_eq!(
            codes_and_data(
                r#"[{"process":"a.exe","title":" "},
                    {"process":"b.exe","title_regex":"("},
                    {"process":"c.exe","title_regex":""},
                    {"process":"d.exe","title":"x","title_regex":"y"}]"#
            ),
            [
                (
                    "bad_title".into(),
                    json!({"process": "a.exe", "title": " "})
                ),
                (
                    "bad_title_regex".into(),
                    json!({"process": "b.exe", "title_regex": "("})
                ),
                (
                    "bad_title_regex".into(),
                    json!({"process": "c.exe", "title_regex": ""})
                ),
                (
                    "title_rule_conflict".into(),
                    json!({"process": "d.exe", "title": "x", "title_regex": "y"})
                ),
            ]
        );
        for code in ["bad_title", "bad_title_regex", "title_rule_conflict"] {
            assert!(is_blocking(code), "{code}");
        }
    }

    #[test]
    fn validate_flags_duplicate_keys_as_informational() {
        use serde_json::json;
        assert_eq!(
            codes_and_data(
                r#"[{"process":"a.exe"},{"process":"a.exe","title":"T"},{"process":"A.EXE"},
                    {"process":"a.exe","title":" t "},{"process":"a.exe","title_regex":"T"},
                    {"process":"a.exe","title_regex":"t"}]"#
            ),
            [
                ("duplicate_profile".into(), json!({"process": "A.EXE"})),
                (
                    "duplicate_profile".into(),
                    json!({"process": "a.exe", "title": " t "})
                ),
            ]
        );
        assert!(!is_blocking("duplicate_profile"));
    }

    #[test]
    fn profile_warnings_carry_the_rule() {
        use serde_json::json;
        assert_eq!(
            codes_and_data(r#"[{"process":"a.exe","title":"T","island":"nope/x"}]"#),
            [(
                "island_not_found".into(),
                json!({"process": "a.exe", "title": "T", "island": "nope/x"})
            )]
        );
    }

    #[test]
    fn upsert_by_key_keeps_other_rules_of_the_process() {
        let mut f = parse(
            r#"{"version":2,"profiles":[{"process":"a.exe","delay_ms":1},{"process":"a.exe","title":"T","delay_ms":2}]}"#,
        )
        .unwrap();
        let sent = profiles_of(
            r#"[{"process":"A.exe","title":" t ","delay_ms":3},{"process":"a.exe","title_regex":"R","delay_ms":4}]"#,
        );
        upsert(&mut f, sent, vec![], vec![], vec![], false);
        let got: Vec<_> = f
            .profiles
            .iter()
            .map(|p| (p.title.as_deref(), p.title_regex.as_deref(), p.delay_ms))
            .collect();
        assert_eq!(
            got,
            [
                (None, None, 1),
                (Some(" t "), None, 3),
                (None, Some("R"), 4)
            ]
        );
    }

    #[test]
    fn remove_string_deletes_only_the_process_only_profile() {
        let mut f = parse(
            r#"{"version":2,"profiles":[{"process":"a.exe"},{"process":"a.exe","title":"T"},
              {"process":"a.exe","title_regex":"R"},{"process":"A.EXE"},{"process":"b.exe"}]}"#,
        )
        .unwrap();
        let remove: Vec<ProfileKey> = serde_json::from_str(r#"["C:\\x\\a.exe"]"#).unwrap();
        upsert(&mut f, vec![], vec![], remove, vec![], false);
        let left: Vec<_> = f
            .profiles
            .iter()
            .map(|p| {
                (
                    p.process.as_str(),
                    p.title.is_some(),
                    p.title_regex.is_some(),
                )
            })
            .collect();
        assert_eq!(
            left,
            [
                ("a.exe", true, false),
                ("a.exe", false, true),
                ("b.exe", false, false)
            ]
        );
    }

    #[test]
    fn remove_object_deletes_that_profile() {
        let mut f = parse(
            r#"{"version":2,"profiles":[{"process":"a.exe"},{"process":"a.exe","title":"T"},
              {"process":"a.exe","title_regex":"R"},{"process":"a.exe","title_regex":"r"}]}"#,
        )
        .unwrap();
        let remove: Vec<ProfileKey> = serde_json::from_str(
            r#"[{"process":"A.EXE","title":"t"},{"process":"a.exe","title_regex":"r"}]"#,
        )
        .unwrap();
        upsert(&mut f, vec![], vec![], remove, vec![], false);
        let left: Vec<_> = f
            .profiles
            .iter()
            .map(|p| (p.title.as_deref(), p.title_regex.as_deref()))
            .collect();
        assert_eq!(left, [(None, None), (None, Some("R"))]);
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bs-prof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn v1_parses_as_v2_without_layouts() {
        let f = parse(FULL).unwrap();
        assert!(f.layouts.is_empty());
        assert!(f.profiles[0].island.is_none() && f.profiles[0].game_resolution.is_none());
    }

    #[test]
    fn v2_with_layouts_island_resolution() {
        let f = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":"primary","islands":[
              {"id":"a","rect":{"x":0,"y":0,"w":0.5,"h":1}}]}],
              "profiles":[{"process":"g.exe","island":"l/a","game_resolution":{"width":1280,"height":720}}]}"#,
        )
        .unwrap();
        assert_eq!(f.layouts[0].islands.len(), 1);
        assert_eq!(f.profiles[0].island.as_deref(), Some("l/a"));
        assert_eq!(f.profiles[0].game_resolution.unwrap().width, 1280);
        assert!(validate(&f).is_empty());
    }

    #[test]
    fn version_3_rejected() {
        assert!(matches!(
            parse(r#"{"version":3}"#),
            Err(ProfileError::UnsupportedVersion(3))
        ));
    }

    #[test]
    fn revision_is_stable_hex() {
        let r = revision(b"abc");
        assert_eq!(
            r,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(r, revision(b"abc"));
    }

    #[test]
    fn validate_warnings() {
        let f = parse(
            r#"{"version":2,"profiles":[
              {"process":"a","island":"halves-16-9/left","monitor":1},
              {"process":"b","island":"halves-16-9/right","offsets":{"left":1,"top":0,"right":0,"bottom":0}},
              {"process":"c","island":"no-slash"},
              {"process":"d","island":"nope/x"},
              {"process":"e","game_resolution":{"width":-1,"height":720}}]}"#,
        )
        .unwrap();
        let c: Vec<String> = validate(&f).into_iter().map(|w| w.code).collect();
        assert_eq!(
            c.iter()
                .filter(|c| *c == "island_overrides_monitor")
                .count(),
            2
        );
        assert!(c.contains(&"bad_island_ref".to_string()));
        assert!(c.contains(&"island_not_found".to_string()));
        assert!(c.contains(&"bad_game_resolution".to_string()));
    }

    #[test]
    fn canonical_and_legacy_preset_refs_are_valid() {
        for r in [
            "halves-16-9/left",
            "halves-16-9/right",
            "mitades-16-9/izquierda", // legacy alias test
            "mitades-16-9/derecha",   // legacy alias test
        ] {
            let f = parse(&format!(
                r#"{{"version":2,"profiles":[{{"process":"g.exe","island":"{r}"}}]}}"#
            ))
            .unwrap();
            assert_eq!(validate(&f), Vec::new(), "{r}");
        }
    }

    #[test]
    fn upsert_add_replace_remove() {
        let mut f =
            parse(r#"{"version":2,"profiles":[{"process":"a.exe"},{"process":"b.exe"}]}"#).unwrap();
        let mut a2 = f.profiles[0].clone();
        a2.delay_ms = 5;
        let mut c = a2.clone();
        c.process = "c.exe".into();
        upsert(
            &mut f,
            vec![a2, c],
            Vec::new(),
            vec!["B.EXE".into()],
            Vec::new(),
            false,
        );
        let names: Vec<_> = f.profiles.iter().map(|p| p.process.as_str()).collect();
        assert_eq!(names, ["a.exe", "c.exe"]);
        assert_eq!(f.profiles[0].delay_ms, 5);
    }

    fn layout(
        id: &str,
        islands: Vec<(&str, f64, f64, f64, f64, crate::islands::Unit)>,
    ) -> crate::islands::Layout {
        crate::islands::Layout {
            id: id.into(),
            monitor: crate::islands::MonitorSel::Primary,
            islands: islands
                .into_iter()
                .map(|(i, x, y, w, h, unit)| crate::islands::Island {
                    id: i.into(),
                    rect: crate::islands::IslandRect { x, y, w, h, unit },
                    extra: Default::default(),
                })
                .collect(),
            extra: Default::default(),
        }
    }

    fn codes_for(layouts: Vec<crate::islands::Layout>) -> Vec<String> {
        let f = ProfileFile {
            version: 2,
            layouts,
            profiles: Vec::new(),
            extra: Default::default(),
        };
        validate(&f).into_iter().map(|w| w.code).collect()
    }

    #[test]
    fn layout_validation_rules() {
        use crate::islands::Unit::{Fraction, Px};
        let ok = layout("l", vec![("a", 0.0, 0.0, 0.5, 1.0, Fraction)]);
        assert!(codes_for(vec![ok.clone()]).is_empty());
        let has = |l: Vec<crate::islands::Layout>, c: &str| codes_for(l).iter().any(|x| x == c);
        let mut empty = ok.clone();
        empty.id = String::new();
        assert!(has(vec![empty], "bad_layout_id"));
        let mut slash = ok.clone();
        slash.id = "a/b".into();
        assert!(has(vec![slash], "bad_layout_id"));
        assert!(has(vec![ok.clone(), ok.clone()], "duplicate_layout"));
        let mut bad_i = ok.clone();
        bad_i.islands[0].id = "x/y".into();
        assert!(has(vec![bad_i], "bad_island_id"));
        let dup = layout(
            "l",
            vec![
                ("a", 0.0, 0.0, 0.5, 0.5, Fraction),
                ("a", 0.5, 0.0, 0.5, 0.5, Fraction),
            ],
        );
        assert!(has(vec![dup], "duplicate_island"));
        for (x, y, w, h, u) in [
            (0.0, 0.0, 0.0, 1.0, Fraction),
            (0.0, 0.0, -1.0, 1.0, Fraction),
            (0.0, 0.0, f64::NAN, 1.0, Fraction),
            (0.0, 0.0, f64::INFINITY, 1.0, Px),
            (0.6, 0.0, 0.5, 1.0, Fraction),
            (0.0, 0.5, 0.5, 0.6, Fraction),
            (-0.1, 0.0, 0.5, 0.5, Fraction),
            (0.0, 0.0, 1e9, 1.0, Fraction),
            (0.0, 0.0, 1e12, 100.0, Px),
        ] {
            assert!(
                has(
                    vec![layout("l", vec![("a", x, y, w, h, u)])],
                    "bad_island_rect"
                ),
                "{x} {y} {w} {h}"
            );
        }
        assert!(
            codes_for(vec![layout(
                "l",
                vec![("a", 10.0, 10.0, 1920.0, 1080.0, Px)]
            )])
            .is_empty()
        );
    }

    #[test]
    fn blocking_codes() {
        for c in [
            "island_overrides_monitor",
            "bad_island_ref",
            "bad_game_resolution",
            "bad_layout_id",
            "duplicate_layout",
            "bad_island_id",
            "duplicate_island",
            "bad_island_rect",
        ] {
            assert!(is_blocking(c), "{c}");
        }
        assert!(!is_blocking("island_not_found"));
    }

    #[test]
    fn upsert_removes_layouts() {
        use crate::islands::Unit::Fraction;
        let mut f = parse(r#"{"version":2}"#).unwrap();
        let l = layout("l", vec![("a", 0.0, 0.0, 1.0, 1.0, Fraction)]);
        upsert(
            &mut f,
            Vec::new(),
            vec![l.clone()],
            Vec::new(),
            Vec::new(),
            false,
        );
        assert_eq!(f.layouts.len(), 1);
        upsert(
            &mut f,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec!["l".into()],
            false,
        );
        assert!(f.layouts.is_empty());
    }

    #[test]
    fn optional_fields_not_serialized() {
        let f = parse(r#"{"version":2,"profiles":[{"process":"a.exe"}]}"#).unwrap();
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("island") && !s.contains("game_resolution"));
    }

    #[test]
    fn save_atomic_v2_and_backup_once() {
        let dir = tmp_dir("save");
        let path = dir.join("profiles.json");
        std::fs::write(&path, FULL).unwrap();
        let (mut f, _) = load(&path).unwrap();
        save_atomic(&path, &f).unwrap();
        let bak = dir.join("profiles.v1.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), FULL);
        assert_eq!(load(&path).unwrap().0.version, 2);
        f.profiles.clear();
        save_atomic(&path, &f).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), FULL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_creates_v2_and_keeps_invalid() {
        let dir = tmp_dir("load");
        let path = dir.join("sub").join("profiles.json");
        let (f, rev) = load(&path).unwrap();
        assert_eq!(f.version, 2);
        assert_eq!(rev, revision(&std::fs::read(&path).unwrap()));
        std::fs::write(&path, "garbage").unwrap();
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn io_error_text_carries_only_the_os_code() {
        let e = ProfileError::Io(std::io::Error::from_raw_os_error(5));
        assert_eq!(e.to_string(), "I/O: os error 5");
    }

    #[test]
    fn unknown_fields_survive_a_save() {
        let dir = tmp_dir("extra");
        let path = dir.join("profiles.json");
        std::fs::write(
            &path,
            r#"{"version":2,"_top":1,"profiles":[{"process":"a.exe","_note":"keep"}]}"#,
        )
        .unwrap();
        let (f, _) = load(&path).unwrap();
        save_atomic(&path, &f).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["_top"], 1);
        assert_eq!(v["profiles"][0]["_note"], "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_fields_inside_layouts_and_islands_survive_a_save() {
        let dir = tmp_dir("extra-layout");
        let path = dir.join("profiles.json");
        std::fs::write(
            &path,
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"_lk":{"a":[1,null]},"islands":[
              {"id":"i","_ik":"keep","rect":{"unit":"px","x":0,"y":0,"w":10,"h":10}}]}]}"#,
        )
        .unwrap();
        let (f, _) = load(&path).unwrap();
        save_atomic(&path, &f).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["layouts"][0]["_lk"], serde_json::json!({"a": [1, null]}));
        assert_eq!(v["layouts"][0]["islands"][0]["_ik"], "keep");
        assert_eq!(v["layouts"][0]["monitor"], 0);
        assert_eq!(v["layouts"][0]["islands"][0]["rect"]["w"], 10.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flattened_layouts_still_reject_bad_known_fields() {
        let island = |i: &str| {
            parse(&format!(
                r#"{{"version":2,"layouts":[{{"id":"l","monitor":0,"islands":[{i}]}}]}}"#
            ))
        };
        assert!(island(r#"{"id":"i","rect":{"x":0,"y":0,"w":"1","h":1},"_k":1}"#).is_err());
        assert!(island(r#"{"id":"i","id":"j","rect":{"x":0,"y":0,"w":1,"h":1}}"#).is_err());
        assert!(island(r#"{"id":"i","_k":1}"#).is_err());
        assert!(island(r#"{"id":"i","rect":{"x":0,"y":0,"w":1,"h":1,"unit":"cm"}}"#).is_err());
        for layout in [
            r#"{"id":"l","monitor":-1,"islands":[],"_k":1}"#,
            r#"{"id":"l","monitor":0.5,"islands":[]}"#,
            r#"{"id":"l","monitor":0,"_k":1}"#,
            r#"{"id":"l","id":"m","monitor":0,"islands":[]}"#,
        ] {
            let text = format!(r#"{{"version":2,"layouts":[{layout}]}}"#);
            assert!(parse(&text).is_err(), "{layout}");
        }
    }

    #[test]
    fn save_after_upserting_one_layout_keeps_extras_of_untouched_layouts() {
        let dir = tmp_dir("extra-untouched");
        let path = dir.join("profiles.json");
        std::fs::write(
            &path,
            r#"{"version":2,"layouts":[
              {"id":"a","monitor":"primary","_la":1,"islands":[
                {"id":"i","_ia":[true],"rect":{"x":0,"y":0,"w":1,"h":1}}]},
              {"id":"b","monitor":"primary","islands":[]}]}"#,
        )
        .unwrap();
        let (mut f, _) = load(&path).unwrap();
        let new = parse(
            r#"{"version":2,"layouts":[{"id":"b","monitor":1,"islands":[]}],
              "profiles":[{"process":"g.exe"}]}"#,
        )
        .unwrap();
        upsert(&mut f, new.profiles, new.layouts, vec![], vec![], false);
        save_atomic(&path, &f).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["layouts"][0]["_la"], 1);
        assert_eq!(
            v["layouts"][0]["islands"][0]["_ia"],
            serde_json::json!([true])
        );
        assert_eq!(v["layouts"][1]["monitor"], 1);
        assert_eq!(v["profiles"][0]["process"], "g.exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_keeps_unknown_fields_of_a_replaced_layout_and_its_islands() {
        let mut f = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"_l":"old","islands":[
              {"id":"i","_i":1,"rect":{"x":0,"y":0,"w":0.5,"h":1}},
              {"id":"j","_j":2,"rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}]}"#,
        )
        .unwrap();
        let new = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":1,"islands":[
              {"id":"i","rect":{"x":0,"y":0,"w":0.25,"h":1}},
              {"id":"k","rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}]}"#,
        )
        .unwrap()
        .layouts;
        upsert(&mut f, vec![], new, vec![], vec![], false);
        let v = serde_json::to_value(&f).unwrap();
        let l = &v["layouts"][0];
        assert_eq!(l["monitor"], 1);
        assert_eq!(l["_l"], "old");
        assert_eq!(l["islands"][0]["rect"]["w"], 0.25);
        assert_eq!(l["islands"][0]["_i"], 1);
        assert_eq!(l["islands"][1]["id"], "k");
        assert_eq!(f.layouts[0].islands[1].extra, serde_json::Map::new());
        assert_eq!(f.layouts[0].islands.len(), 2);
    }

    #[test]
    fn upsert_unknown_fields_of_the_new_layout_and_island_win() {
        let mut f = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"_l":"old","_x":1,"islands":[
              {"id":"i","_i":"old","_y":1,"rect":{"x":0,"y":0,"w":1,"h":1}}]}]}"#,
        )
        .unwrap();
        let new = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"_l":"new","islands":[
              {"id":"i","_i":"new","rect":{"x":0,"y":0,"w":1,"h":1}}]}]}"#,
        )
        .unwrap()
        .layouts;
        upsert(&mut f, vec![], new, vec![], vec![], false);
        let v = serde_json::to_value(&f).unwrap();
        let l = &v["layouts"][0];
        assert_eq!(l["_l"], "new");
        assert!(l.get("_x").is_none());
        assert_eq!(l["islands"][0]["_i"], "new");
        assert!(l["islands"][0].get("_y").is_none());
    }

    #[test]
    fn replace_extras_clears_unknown_fields_in_one_call() {
        let mut f = parse(
            r#"{"version":2,"_top":1,
              "layouts":[
                {"id":"l","monitor":0,"_l":1,"islands":[
                  {"id":"i","_i":1,"rect":{"x":0,"y":0,"w":0.5,"h":1}},
                  {"id":"j","_j":1,"_j2":2,"rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]},
                {"id":"u","monitor":0,"_u":1,"islands":[
                  {"id":"i","_ui":1,"rect":{"x":0,"y":0,"w":1,"h":1}}]}],
              "profiles":[{"process":"a.exe","_p":1},{"process":"b.exe","_b":1}]}"#,
        )
        .unwrap();
        let new = parse(
            r#"{"version":2,
              "layouts":[{"id":"l","monitor":0,"islands":[
                {"id":"i","rect":{"x":0,"y":0,"w":0.5,"h":1}},
                {"id":"j","_j2":2,"rect":{"x":0.5,"y":0,"w":0.5,"h":1}}]}],
              "profiles":[{"process":"A.EXE"}]}"#,
        )
        .unwrap();
        upsert(&mut f, new.profiles, new.layouts, vec![], vec![], true);
        let empty = serde_json::Map::new();
        let j2: serde_json::Map<_, _> = [("_j2".to_string(), serde_json::json!(2))]
            .into_iter()
            .collect();
        assert_eq!(f.profiles[0].extra, empty);
        assert_eq!(f.layouts[0].extra, empty);
        assert_eq!(f.layouts[0].islands[0].extra, empty);
        assert_eq!(f.layouts[0].islands[1].extra, j2);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["_top"], 1);
        assert_eq!(v["profiles"][1]["_b"], 1);
        assert_eq!(v["layouts"][1]["_u"], 1);
        assert_eq!(v["layouts"][1]["islands"][0]["_ui"], 1);
    }

    #[test]
    fn upsert_keeps_unknown_fields_of_a_replaced_profile() {
        let mut f =
            parse(r#"{"version":2,"profiles":[{"process":"a.exe","_note":"old"}]}"#).unwrap();
        let new = parse(r#"{"version":2,"profiles":[{"process":"A.EXE","delay_ms":5}]}"#)
            .unwrap()
            .profiles;
        upsert(&mut f, new, vec![], vec![], vec![], false);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["profiles"][0]["delay_ms"], 5);
        assert_eq!(v["profiles"][0]["_note"], "old");
    }

    #[test]
    fn upsert_unknown_fields_of_the_new_profile_win() {
        let mut f = parse(r#"{"version":2,"profiles":[{"process":"a.exe","_note":"old","_x":1}]}"#)
            .unwrap();
        let new = parse(r#"{"version":2,"profiles":[{"process":"a.exe","_note":"new"}]}"#)
            .unwrap()
            .profiles;
        upsert(&mut f, new, vec![], vec![], vec![], false);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["profiles"][0]["_note"], "new");
        assert!(v["profiles"][0].get("_x").is_none());
    }

    #[test]
    fn save_keeps_the_previous_file_as_prev() {
        let dir = tmp_dir("prev");
        let path = dir.join("profiles.json");
        let prev = dir.join("profiles.json.prev");
        let (mut f, _) = load(&path).unwrap();
        assert!(
            !prev.exists(),
            "creating a missing file has nothing to keep"
        );
        let first = std::fs::read(&path).unwrap();
        f.profiles = parse(r#"{"version":2,"profiles":[{"process":"a.exe"}]}"#)
            .unwrap()
            .profiles;
        save_atomic(&path, &f).unwrap();
        assert_eq!(std::fs::read(&prev).unwrap(), first);
        let second = std::fs::read(&path).unwrap();
        f.profiles.clear();
        save_atomic(&path, &f).unwrap();
        assert_eq!(std::fs::read(&prev).unwrap(), second);
        assert!(!dir.join("profiles.json.prev.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_succeeds_when_prev_cannot_be_written() {
        let dir = tmp_dir("prevfail");
        let path = dir.join("profiles.json");
        // A non-empty directory where the copy should go: the copy fails, the save must not.
        std::fs::create_dir_all(dir.join("profiles.json.prev").join("x")).unwrap();
        let (mut f, _) = load(&path).unwrap();
        f.profiles = parse(r#"{"version":2,"profiles":[{"process":"a.exe"}]}"#)
            .unwrap()
            .profiles;
        save_atomic(&path, &f).unwrap();
        assert_eq!(load(&path).unwrap().0.profiles.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf8_bom_is_accepted() {
        let dir = tmp_dir("bom");
        let path = dir.join("profiles.json");
        let bytes = b"\xEF\xBB\xBF{\"version\":2,\"profiles\":[{\"process\":\"a.exe\"}]}";
        std::fs::write(&path, bytes).unwrap();
        let (f, rev) = load(&path).unwrap();
        assert_eq!(f.profiles[0].process, "a.exe");
        assert_eq!(rev, revision(bytes));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf16_is_a_typed_error_and_the_file_is_kept() {
        let dir = tmp_dir("utf16");
        let path = dir.join("profiles.json");
        let json = r#"{"version":2}"#;
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(json.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(json.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        for bytes in [le, be] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(matches!(load(&path), Err(ProfileError::Utf16)));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `parse(text)` is `Parse` and its message contains `needle`.
    fn rejected(text: &str, needle: &str) {
        match parse(text) {
            Err(ProfileError::Parse(e)) => assert!(e.to_string().contains(needle), "{text}: {e}"),
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn duplicate_keys_make_the_file_invalid() {
        let island = |rect: &str, island: &str| {
            format!(
                r#"{{"version":2,"layouts":[{{"id":"l","monitor":0,"islands":[{{"id":"i",
                  "rect":{{"x":0,"y":0,"w":1,"h":1{rect}}}{island}}}]}}]}}"#
            )
        };
        for text in [
            r#"{"version":2,"_a":1,"_a":2}"#.to_string(),
            r#"{"version":2,"profiles":[{"process":"a","_n":1,"_n":2}]}"#.into(),
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"islands":[],"_l":1,"_l":2}]}"#.into(),
            island("", r#","_i":1,"_i":2"#),
            island(r#","_r":1,"_r":2"#, ""),
            r#"{"version":2,"_x":{"k":1,"k":2}}"#.into(),
            r#"{"version":2,"_x":[{"k":[{"j":1,"j":2}]}]}"#.into(),
            r#"{"version":2,"profiles":[{"process":"a","offsets":{"left":1,"_z":0,"_z":0}}]}"#
                .into(),
            r#"{"version":2,"profiles":[{"process":"a",
              "game_resolution":{"width":1,"height":1,"_z":0,"_z":0}}]}"#
                .into(),
            r#"{"version":2,"_a":1,"\u005fa":2}"#.into(),
            r#"{"version":2,"profiles":[{"process":"a","enabled":true,"enabled":false}]}"#.into(),
        ] {
            rejected(&text, "duplicate");
        }
    }

    #[test]
    fn arrays_where_objects_are_expected_make_the_file_invalid() {
        for (text, at) in [
            (
                r#"{"version":2,"profiles":[{"process":"a","offsets":[1,2,3,4]}]}"#,
                "/profiles/0/offsets",
            ),
            (
                r#"{"version":2,"profiles":[{"process":"a","game_resolution":[1920,1080]}]}"#,
                "/profiles/0/game_resolution",
            ),
            (
                r#"{"version":2,"layouts":[{"id":"l","monitor":0,"islands":[
                  {"id":"i","rect":[0,0,0.5,1]}]}]}"#,
                "/layouts/0/islands/0/rect",
            ),
        ] {
            rejected(text, &format!("unexpected array at \"{at}\""));
        }
    }

    #[test]
    fn strict_checks_keep_valid_files_loading() {
        // Arrays, objects and the same key in different objects, in the extras of all four levels.
        let text = r#"{"version":2,"_t":[1,{"k":[2]}],"layouts":[{"id":"l","monitor":0,
          "_l":{"k":[1,2]},"islands":[{"id":"i","_i":[[1],{"k":{}}],
          "rect":{"x":0,"y":0,"w":1,"h":1}}]}],"profiles":[{"process":"a","_p":{"k":[{"k":1}]}}]}"#;
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(crate::ipc::check_arrays::<ProfileFile>(&value), Ok(()));
        let dir = tmp_dir("strict-extras");
        let path = dir.join("profiles.json");
        std::fs::write(&path, text).unwrap();
        let (f, _) = load(&path).unwrap();
        save_atomic(&path, &f).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for at in [
            "/_t",
            "/layouts/0/_l",
            "/layouts/0/islands/0/_i",
            "/profiles/0/_p",
        ] {
            assert_eq!(saved.pointer(at), value.pointer(at), "{at}");
        }
        assert_eq!(load(&path).unwrap().0, f);
        let _ = std::fs::remove_dir_all(&dir);

        let two = parse(
            r#"{"version":2,"layouts":[{"id":"l","monitor":0,"islands":[]},
              {"id":"l","monitor":1,"islands":[]}]}"#,
        )
        .unwrap();
        assert!(validate(&two).iter().any(|w| w.code == "duplicate_layout"));
        let same_key =
            parse(r#"{"version":2,"profiles":[{"process":"a","_n":1},{"process":"b","_n":2}]}"#)
                .unwrap();
        assert_eq!(same_key.profiles[1].extra["_n"], 2);
        assert_eq!(parse(FULL).unwrap().version, 1);
    }

    #[test]
    fn newer_version_wins_over_shape_errors() {
        for text in [
            r#"{"version":99,"_a":1,"_a":2}"#,
            r#"{"version":3,"profiles":[{"process":"a","offsets":[1,2,3,4]}]}"#,
        ] {
            assert!(
                matches!(parse(text), Err(ProfileError::UnsupportedVersion(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn invalid_v1_with_a_duplicate_key_is_kept_and_not_backed_up() {
        let dir = tmp_dir("v1-dup");
        let path = dir.join("profiles.json");
        let bak = dir.join("profiles.v1.bak");
        let prev = dir.join("profiles.json.prev");
        let dup = FULL.replacen(r#""process""#, r#""_n":1,"_n":2,"process""#, 1);
        std::fs::write(&path, &dup).unwrap();
        assert!(matches!(load(&path), Err(ProfileError::Parse(_))));
        assert_eq!(std::fs::read(&path).unwrap(), dup.as_bytes());
        assert!(!bak.exists() && !prev.exists());

        std::fs::write(&path, FULL).unwrap();
        let (f, _) = load(&path).unwrap();
        save_atomic(&path, &f).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), FULL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignored_values_are_still_parsed_strictly_and_file_is_kept() {
        let dir = tmp_dir("strict-ignored");
        let path = dir.join("profiles.json");
        for bad in [r#"1e999"#, r#""\ud800""#] {
            let text = format!(
                r#"{{"version":2,"profiles":[{{"process":"a","offsets":{{"_x":{bad}}}}}]}}"#
            );
            std::fs::write(&path, &text).unwrap();
            assert!(matches!(load(&path), Err(ProfileError::Parse(_))), "{bad}");
            assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
