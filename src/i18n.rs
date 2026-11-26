//! Localized user text (plan addendum W2, A1): JSON catalogs under `locales/<lang>/` compiled in
//! with `include_str!`. Lookup falls back `es` → `en` → the key itself. Placeholders are
//! `{name}`; plural entries are `{"one": …, "other": …}`. MCP never calls [`init`]: English only.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::Value;

use crate::ipc::ErrorCode;

/// Overrides the settings file and the OS language: `en` or `es`, case-insensitive.
pub const ENV_VAR: &str = "BORDERLESS_SHATTER_LANG";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Locale {
    En = 0,
    Es = 1,
}

/// One shipped language. `LOCALES[l as usize].locale == l`.
pub struct LocaleInfo {
    pub locale: Locale,
    pub code: &'static str,
    /// `PRIMARYLANGID` of the Windows LANGIDs that select this language.
    pub primary_langid: u16,
    /// Plural rule: does `n` take the `one` form?
    pub is_one: fn(u64) -> bool,
    /// `(namespace, catalog JSON)`.
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! catalog_files {
    ($lang:literal) => {
        &[
            (
                "common",
                include_str!(concat!("../locales/", $lang, "/common.json")),
            ),
            (
                "tray",
                include_str!(concat!("../locales/", $lang, "/tray.json")),
            ),
            (
                "cli",
                include_str!(concat!("../locales/", $lang, "/cli.json")),
            ),
            (
                "gui",
                include_str!(concat!("../locales/", $lang, "/gui.json")),
            ),
            (
                "windows",
                include_str!(concat!("../locales/", $lang, "/windows.json")),
            ),
            (
                "islands",
                include_str!(concat!("../locales/", $lang, "/islands.json")),
            ),
            (
                "editor",
                include_str!(concat!("../locales/", $lang, "/editor.json")),
            ),
            (
                "diagnose",
                include_str!(concat!("../locales/", $lang, "/diagnose.json")),
            ),
            (
                "profiles",
                include_str!(concat!("../locales/", $lang, "/profiles.json")),
            ),
            (
                "preview",
                include_str!(concat!("../locales/", $lang, "/preview.json")),
            ),
        ]
    };
}

fn one_if_1(n: u64) -> bool {
    n == 1
}

pub static LOCALES: &[LocaleInfo] = &[
    LocaleInfo {
        locale: Locale::En,
        code: "en",
        primary_langid: 0x09,
        is_one: one_if_1,
        files: catalog_files!("en"),
    },
    LocaleInfo {
        locale: Locale::Es,
        code: "es",
        primary_langid: 0x0A,
        is_one: one_if_1,
        files: catalog_files!("es"),
    },
];

impl Locale {
    pub fn info(self) -> &'static LocaleInfo {
        &LOCALES[self as usize]
    }

    pub fn code(self) -> &'static str {
        self.info().code
    }

    /// `en`/`es`, case-insensitive; anything else is `None`.
    pub fn from_code(s: &str) -> Option<Locale> {
        LOCALES
            .iter()
            .find(|l| l.code.eq_ignore_ascii_case(s))
            .map(|l| l.locale)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Text(String),
    Plural { one: String, other: String },
}

/// Every entry of one locale, all namespaces merged.
#[derive(Debug, Default)]
pub struct Catalog(HashMap<String, Entry>);

impl Catalog {
    /// A file that is not a JSON object is skipped (logged); so is an entry of the wrong shape.
    pub fn parse(files: &[(&str, &str)]) -> Catalog {
        let mut map = HashMap::new();
        for (ns, json) in files {
            let obj = match serde_json::from_str::<Value>(json) {
                Ok(Value::Object(m)) => m,
                _ => {
                    eprintln!("i18n: catalog {ns} is not a JSON object; ignored");
                    continue;
                }
            };
            for (k, v) in obj {
                let entry = match v {
                    Value::String(s) => Entry::Text(s),
                    Value::Object(o) => match (o.get("one"), o.get("other")) {
                        (Some(Value::String(one)), Some(Value::String(other))) => Entry::Plural {
                            one: one.clone(),
                            other: other.clone(),
                        },
                        _ => {
                            eprintln!("i18n: {ns}: plural {k} needs \"one\" and \"other\"");
                            continue;
                        }
                    },
                    _ => {
                        eprintln!("i18n: {ns}: {k} is neither a string nor a plural");
                        continue;
                    }
                };
                map.insert(k, entry);
            }
        }
        Catalog(map)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Locale::En as u8);

#[cfg(test)]
thread_local! {
    /// Tests run on parallel threads, so each sees only the locale it set (English by default).
    static TEST_LOCALE: std::cell::Cell<u8> = const { std::cell::Cell::new(Locale::En as u8) };
}

pub fn locale() -> Locale {
    #[cfg(test)]
    let current = TEST_LOCALE.get();
    #[cfg(not(test))]
    let current = CURRENT.load(Ordering::Relaxed);
    match current {
        1 => Locale::Es,
        _ => Locale::En,
    }
}

pub fn set_locale(l: Locale) {
    #[cfg(test)]
    TEST_LOCALE.set(l as u8);
    CURRENT.store(l as u8, Ordering::Relaxed);
}

static CATALOGS: LazyLock<Vec<Catalog>> =
    LazyLock::new(|| LOCALES.iter().map(|l| Catalog::parse(l.files)).collect());

fn catalogs() -> &'static [Catalog] {
    &CATALOGS
}

/// `key` in `locale`, else in English.
fn lookup<'a>(cats: &'a [Catalog], locale: Locale, key: &str) -> Option<&'a Entry> {
    let get = |l: Locale| cats.get(l as usize).and_then(|c| c.0.get(key));
    get(locale).or_else(|| get(Locale::En))
}

fn text(e: &Entry) -> &str {
    match e {
        Entry::Text(s) => s,
        Entry::Plural { other, .. } => other,
    }
}

/// Whether `key` exists in the current locale or in English (for computed keys with a fallback).
pub fn has(key: &str) -> bool {
    lookup(catalogs(), locale(), key).is_some()
}

/// Text of `key`; a missing key yields the key itself.
pub fn t(key: &str) -> Cow<'static, str> {
    match lookup(catalogs(), locale(), key) {
        Some(e) => Cow::Borrowed(text(e)),
        None => Cow::Owned(key.to_owned()),
    }
}

/// `key` with its `{name}` placeholders replaced; a placeholder without an argument stays as is.
pub fn tf(key: &str, args: &[(&str, &dyn Display)]) -> String {
    render(&t(key), args)
}

/// Plural `key` for `n` (`{n}` is available as a placeholder unless `args` supplies it).
pub fn tn(key: &str, n: u64, args: &[(&str, &dyn Display)]) -> String {
    let l = locale();
    let Some(template) = plural_text(lookup(catalogs(), l, key), l, n) else {
        return key.to_owned();
    };
    let mut all: Vec<(&str, &dyn Display)> = args.to_vec();
    all.push(("n", &n));
    render(template, &all)
}

/// The plural form of `e` for `n` under `locale`'s rule; a plain text entry ignores `n`.
fn plural_text(e: Option<&Entry>, locale: Locale, n: u64) -> Option<&str> {
    Some(match e? {
        Entry::Text(s) => s,
        Entry::Plural { one, .. } if (locale.info().is_one)(n) => one,
        Entry::Plural { other, .. } => other,
    })
}

/// `v` with `digits` decimals and the locale's decimal separator; never a thousands separator.
pub fn fmt_decimal(v: f64, digits: usize) -> String {
    let s = format!("{v:.digits$}");
    match t("common.decimal_separator") {
        sep if sep == "." => s,
        sep => s.replace('.', &sep),
    }
}

/// Plain sentence for an engine error code (`common.error.<code>`).
pub fn error_sentence(code: ErrorCode) -> Cow<'static, str> {
    let name = serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".to_owned());
    t(&format!("common.error.{name}"))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

enum Part<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

/// Splits `template` into literal text and `{ident}` placeholders.
fn parts(template: &str) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        match after.find('}') {
            Some(j) if is_ident(&after[..j]) => {
                out.push(Part::Literal(&rest[..i]));
                out.push(Part::Placeholder(&after[..j]));
                rest = &after[j + 1..];
            }
            _ => {
                out.push(Part::Literal(&rest[..=i]));
                rest = after;
            }
        }
    }
    out.push(Part::Literal(rest));
    out
}

/// Placeholder names in `template` (`{ident}` only, so `{"a":{...}}` is literal text).
pub fn placeholders(template: &str) -> Vec<&str> {
    parts(template)
        .into_iter()
        .filter_map(|p| match p {
            Part::Placeholder(name) => Some(name),
            Part::Literal(_) => None,
        })
        .collect()
}

/// Single pass: a substituted argument is never scanned again.
fn render(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    for p in parts(template) {
        match p {
            Part::Literal(s) => out.push_str(s),
            Part::Placeholder(name) => match args.iter().find(|(k, _)| *k == name) {
                Some((_, v)) => {
                    let _ = write!(out, "{v}");
                }
                None => {
                    let _ = write!(out, "{{{name}}}");
                }
            },
        }
    }
    out
}

/// Locale from, in order: `BORDERLESS_SHATTER_LANG`, the settings file (`None` = system), and
/// the OS UI LANGID (any LANGID whose primary language is Spanish gives Spanish).
pub fn resolve(env: Option<&str>, setting: Option<Locale>, langid: u16) -> Locale {
    env.and_then(Locale::from_code)
        .or(setting)
        .unwrap_or_else(|| from_langid(langid))
}

/// `PRIMARYLANGID(langid)` (low 10 bits) → the shipped language for it, else English.
fn from_langid(langid: u16) -> Locale {
    let primary = langid & 0x3FF;
    LOCALES
        .iter()
        .find(|l| l.primary_langid == primary)
        .map_or(Locale::En, |l| l.locale)
}

/// The user's UI language as a Windows LANGID.
pub fn os_ui_language() -> u16 {
    // SAFETY: no arguments, no pointers; it only reads the user's language settings.
    unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() }
}

/// Sets the process locale from the env var, `settings.json` and the OS. Called once at start by
/// the resident, the panel and the CLI.
pub fn init() -> Locale {
    let env = std::env::var(ENV_VAR).ok();
    let setting =
        crate::settings::default_path().and_then(|p| crate::settings::load(&p).settings.language);
    let l = resolve(env.as_deref(), setting, os_ui_language());
    set_locale(l);
    l
}

/// Sets this test thread's locale; restores the previous one on drop.
#[cfg(test)]
pub(crate) struct LocaleGuard(Locale);

#[cfg(test)]
pub(crate) fn scoped_locale(l: Locale) -> LocaleGuard {
    let prev = locale();
    set_locale(l);
    LocaleGuard(prev)
}

#[cfg(test)]
impl Drop for LocaleGuard {
    fn drop(&mut self) {
        set_locale(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;

    fn test_catalogs() -> Vec<Catalog> {
        vec![
            Catalog::parse(&[(
                "x",
                r#"{"x.both": "en both", "x.en_only": "en only",
                    "x.items": {"one": "{n} item", "other": "{n} items"}}"#,
            )]),
            Catalog::parse(&[("x", r#"{"x.both": "es ambos", "x.broken": 5}"#)]),
        ]
    }

    #[test]
    fn lookup_prefers_the_locale() {
        let c = test_catalogs();
        let e = lookup(&c, Locale::Es, "x.both").map(text);
        assert_eq!(e, Some("es ambos"));
        assert_eq!(lookup(&c, Locale::En, "x.both").map(text), Some("en both"));
    }

    #[test]
    fn lookup_falls_back_to_english_then_none() {
        let c = test_catalogs();
        assert_eq!(
            lookup(&c, Locale::Es, "x.en_only").map(text),
            Some("en only")
        );
        assert_eq!(lookup(&c, Locale::Es, "x.missing"), None);
        // A wrongly typed entry is dropped, so it falls back too.
        assert_eq!(lookup(&c, Locale::Es, "x.broken"), None);
    }

    #[test]
    fn invalid_catalog_is_empty() {
        let c = Catalog::parse(&[("bad", "{not json"), ("arr", "[1]")]);
        assert!(c.0.is_empty());
    }

    #[test]
    fn t_missing_key_is_the_key() {
        let _g = scoped_locale(Locale::Es);
        // Not a literal at the call, so the used-key scan in tests/i18n.rs skips it.
        let missing = "nope.not_a_key";
        assert_eq!(t(missing), missing);
        assert!(!has(missing));
    }

    #[test]
    fn t_reads_shipped_catalogs() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(t("tray.quit"), "Quit");
        assert!(has("tray.quit"));
    }

    #[test]
    fn a_locale_set_on_another_thread_is_not_seen_here() {
        use std::sync::mpsc::channel;
        let (set_tx, set_rx) = channel();
        let (done_tx, done_rx) = channel::<()>();
        let other = std::thread::spawn(move || {
            let _g = scoped_locale(Locale::Es);
            set_tx.send(()).unwrap();
            let _ = done_rx.recv();
        });
        set_rx.recv().unwrap();
        let seen = (locale(), t("tray.quit"));
        done_tx.send(()).unwrap();
        other.join().unwrap();
        assert_eq!(seen, (Locale::En, "Quit".into()));
    }

    #[test]
    fn render_replaces_placeholders() {
        let out = render("{a} and {b}", &[("a", &1), ("b", &"two")]);
        assert_eq!(out, "1 and two");
    }

    #[test]
    fn render_keeps_missing_args_and_literal_braces() {
        assert_eq!(render("x {name} y", &[]), "x {name} y");
        assert_eq!(render(r#"{"e":{...}} {"#, &[("e", &1)]), r#"{"e":{...}} {"#);
        // An argument that looks like a placeholder is not expanded again.
        assert_eq!(render("{a}", &[("a", &"{b}"), ("b", &2)]), "{b}");
    }

    #[test]
    fn placeholders_lists_idents_only() {
        assert_eq!(
            placeholders("{a} {b_2} {a} {x y} {} {\"q\":1}"),
            ["a", "b_2", "a"]
        );
    }

    #[test]
    fn tf_renders_shipped_key() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(
            tf("tray.apply", &[("process", &"game.exe")]),
            "Apply: game.exe"
        );
        assert_eq!(tf("tray.apply", &[]), "Apply: {process}");
    }

    #[test]
    fn plural_one_and_other() {
        let c = test_catalogs();
        let e = lookup(&c, Locale::En, "x.items").cloned();
        assert_eq!(plural_text(e.as_ref(), Locale::En, 1), Some("{n} item"));
        assert_eq!(plural_text(e.as_ref(), Locale::En, 0), Some("{n} items"));
        assert_eq!(plural_text(e.as_ref(), Locale::Es, 2), Some("{n} items"));
    }

    #[test]
    fn tn_missing_key_and_n_placeholder() {
        let _g = scoped_locale(Locale::En);
        let missing = "nope.items";
        assert_eq!(tn(missing, 3, &[]), missing);
        assert_eq!(render("{n} x", &[("n", &3u64)]), "3 x");
    }

    #[test]
    fn fmt_decimal_per_locale() {
        {
            let _g = scoped_locale(Locale::En);
            assert_eq!(fmt_decimal(33.333, 1), "33.3");
            assert_eq!(fmt_decimal(2560.0, 0), "2560");
        }
        let _g = scoped_locale(Locale::Es);
        assert_eq!(fmt_decimal(33.333, 1), "33,3");
        assert_eq!(fmt_decimal(-0.25, 2), "-0,25");
        assert_eq!(fmt_decimal(2560.5, 1), "2560,5");
    }

    #[test]
    fn error_sentence_uses_the_code() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(error_sentence(ErrorCode::Busy), t("common.error.busy"));
        assert_eq!(
            error_sentence(ErrorCode::Win32Error),
            "Windows did not allow the window to be moved."
        );
    }

    #[test]
    fn locales_table_is_indexed_by_enum() {
        for (i, l) in LOCALES.iter().enumerate() {
            assert_eq!(l.locale as usize, i);
            assert_eq!(Locale::from_code(l.code), Some(l.locale));
        }
    }

    #[test]
    fn resolve_table() {
        use Locale::{En, Es};
        // (env, setting, langid) → locale
        let cases: &[(Option<&str>, Option<Locale>, u16, Locale)] = &[
            (None, None, 0x0C0A, Es),
            (None, None, 0x080A, Es),
            (None, None, 0x0409, En),
            (None, None, 0x0407, En),
            (None, None, 0x1400, En), // LOCALE_CUSTOM_UI_DEFAULT
            (Some("es"), None, 0x0409, Es),
            (Some("EN"), None, 0x0C0A, En),
            (Some("Es"), Some(En), 0x0409, Es),
            (Some("garbage"), None, 0x0C0A, Es),
            (Some("garbage"), None, 0x0409, En),
            (Some(""), Some(Es), 0x0409, Es),
            (Some(" en"), None, 0x080A, Es),
            (None, Some(En), 0x0C0A, En),
            (None, Some(Es), 0x0407, Es),
            (Some("en"), Some(Es), 0x0C0A, En),
            (Some("garbage"), Some(En), 0x0C0A, En),
        ];
        for &(env, setting, langid, want) in cases {
            assert_eq!(
                resolve(env, setting, langid),
                want,
                "{env:?} {setting:?} {langid:#06x}"
            );
        }
    }

    #[test]
    fn os_ui_language_does_not_panic() {
        let _ = resolve(None, None, os_ui_language());
    }

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0x118E),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(cfg())]

        #[test]
        fn render_substitutes_every_placeholder(
            template in "[a-c{} _\"]{0,40}",
            value in "[A-Z0-9]{1,6}",
        ) {
            let names: Vec<String> = placeholders(&template).into_iter().map(str::to_owned).collect();
            let args: Vec<(&str, &dyn Display)> =
                names.iter().map(|n| (n.as_str(), &value as &dyn Display)).collect();
            let out = render(&template, &args);
            for n in &names {
                prop_assert!(!out.contains(&format!("{{{n}}}")) || value.contains(n.as_str()),
                    "{template:?} → {out:?}");
            }
            if !names.is_empty() {
                prop_assert!(out.contains(&value), "{template:?} → {out:?}");
            }
        }

        #[test]
        fn render_never_panics(template in "\\PC{0,60}", value in "\\PC{0,10}") {
            let _ = render(&template, &[("a", &value), ("n", &1)]);
            let _ = placeholders(&template);
        }

        #[test]
        fn resolve_is_total_and_spanish_os_gives_es(
            sub in 0u16..64,
            other in any::<u16>(),
            env in proptest::option::of("\\PC{0,8}"),
        ) {
            let es_langid = (sub << 10) | 0x0A;
            prop_assert_eq!(resolve(None, None, es_langid), Locale::Es);
            prop_assert_eq!(resolve(Some("en"), None, es_langid), Locale::En);
            prop_assert_eq!(resolve(None, Some(Locale::En), es_langid), Locale::En);
            let _ = resolve(env.as_deref(), None, other);
            if other & 0x3FF != 0x0A {
                prop_assert_eq!(resolve(None, None, other), Locale::En);
            }
        }
    }
}
