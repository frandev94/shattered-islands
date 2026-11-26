//! Catalog completeness (plan addendum W2, I4) and the "no Spanish outside catalogs" scan (A5).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use borderless_shatter::i18n::{LOCALES, placeholders};
use borderless_shatter::ipc::ErrorCode;
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Locale code → key → value, failing on any malformed file or duplicate key.
fn catalogs() -> BTreeMap<&'static str, BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for l in LOCALES {
        let mut keys = BTreeMap::new();
        for (ns, json) in l.files {
            let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(json) else {
                panic!("locales/{}/{ns}.json is not a JSON object", l.code);
            };
            for (k, v) in obj {
                assert!(
                    k.starts_with(&format!("{ns}.")),
                    "locales/{}/{ns}.json: key {k} outside its namespace",
                    l.code
                );
                assert!(
                    keys.insert(k.clone(), v).is_none(),
                    "{}: duplicate key {k}",
                    l.code
                );
            }
        }
        out.insert(l.code, keys);
    }
    out
}

/// Placeholder names of an entry (both plural forms together).
fn names(v: &Value) -> BTreeSet<String> {
    let forms: Vec<&str> = match v {
        Value::String(s) => vec![s],
        Value::Object(o) => o.values().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    forms
        .into_iter()
        .flat_map(placeholders)
        .map(str::to_owned)
        .collect()
}

#[test]
fn entries_are_strings_or_one_other_plurals() {
    for (code, keys) in catalogs() {
        for (k, v) in keys {
            match &v {
                Value::String(_) => {}
                Value::Object(o) => {
                    let forms: BTreeSet<&str> = o.keys().map(String::as_str).collect();
                    assert_eq!(forms, BTreeSet::from(["one", "other"]), "{code}: {k}");
                    assert!(o.values().all(Value::is_string), "{code}: {k}");
                }
                _ => panic!("{code}: {k} is neither a string nor a plural"),
            }
        }
    }
}

#[test]
fn every_locale_has_exactly_the_english_keys() {
    let cats = catalogs();
    let en: BTreeSet<&String> = cats["en"].keys().collect();
    for (code, keys) in &cats {
        let mine: BTreeSet<&String> = keys.keys().collect();
        let missing: Vec<_> = en.difference(&mine).collect();
        let extra: Vec<_> = mine.difference(&en).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{code}: missing {missing:?}, extra {extra:?}"
        );
    }
}

#[test]
fn placeholders_and_plural_shape_match_english() {
    let cats = catalogs();
    for (code, keys) in &cats {
        for (k, v) in keys {
            let Some(en) = cats["en"].get(k) else {
                continue;
            };
            assert_eq!(names(v), names(en), "{code}: {k} placeholders");
            assert_eq!(v.is_object(), en.is_object(), "{code}: {k} plural shape");
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Key literals at `t("…")`, `tf("…"` and `tn("…"` call sites, also when rustfmt puts the key
/// on its own line.
fn used_keys(src: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for call in ["t(", "tf(", "tn("] {
        for (i, _) in src.match_indices(call) {
            let before = src[..i].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = src[i + call.len()..].trim_start();
            if let Some(end) = rest.strip_prefix('"').and_then(|r| r.find('"')) {
                keys.push(rest[1..=end].to_owned());
            }
        }
    }
    keys
}

#[test]
fn used_key_scanner_skips_other_calls() {
    let src = r#"t("a.b") tf("c.d", &[]) split("x") i18n::tn("e.f", 1, &[]) let t = 1;"#;
    assert_eq!(used_keys(src), ["a.b", "c.d", "e.f"]);
}

#[test]
fn used_key_scanner_sees_calls_split_across_lines() {
    let src = "i18n::tf(\n        \"g.h\",\n        &[],\n    ) t( \"i.j\") t(x)";
    assert_eq!(used_keys(src), ["i.j", "g.h"]);
}

#[test]
fn every_used_key_exists() {
    let cats = catalogs();
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    let mut missing = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        for k in used_keys(&src) {
            if !cats["en"].contains_key(&k) {
                missing.push(format!("{}: {k}", f.display()));
            }
        }
    }
    assert!(missing.is_empty(), "{missing:#?}");
}

#[test]
fn every_error_code_has_a_sentence() {
    use ErrorCode::*;
    let all = [
        InvalidParams,
        UnknownMethod,
        InvalidJson,
        WindowNotFound,
        AmbiguousWindow,
        WindowGone,
        AccessDenied,
        ExcludedProcess,
        IslandNotFound,
        MonitorUnavailable,
        ProfileError,
        RevisionConflict,
        UnsupportedVersion,
        Win32Error,
        Busy,
        Internal,
    ];
    // No wildcard: a new variant fails to compile here until it is listed above.
    for c in all {
        match c {
            InvalidParams | UnknownMethod | InvalidJson | WindowNotFound | AmbiguousWindow
            | WindowGone | AccessDenied | ExcludedProcess | IslandNotFound | MonitorUnavailable
            | ProfileError | RevisionConflict | UnsupportedVersion | Win32Error | Busy
            | Internal => {}
        }
    }
    let cats = catalogs();
    for c in all {
        let name = serde_json::to_value(c).unwrap();
        let key = format!("common.error.{}", name.as_str().unwrap());
        for (code, keys) in &cats {
            assert!(keys.contains_key(&key), "{code}: {key}");
        }
    }
}

#[test]
fn every_preset_has_a_name() {
    let cats = catalogs();
    for layout in borderless_shatter::islands::presets() {
        let key = format!("common.preset.{}", layout.id);
        for (code, keys) in &cats {
            assert!(keys.contains_key(&key), "{code}: {key}");
        }
    }
}

/// Codes the engine emits: the first string literal of every `rec(` call in `src/diagnose.rs`.
fn recommendation_codes() -> Vec<String> {
    let src = std::fs::read_to_string(root().join("src/diagnose.rs")).unwrap();
    let mut codes = Vec::new();
    for (i, _) in src.match_indices("rec(") {
        if src[..i].ends_with("fn ")
            || src[..i]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let rest = src[i + 4..].trim_start();
        if let Some(body) = rest.strip_prefix('"')
            && let Some(end) = body.find('"')
        {
            codes.push(body[..end].to_owned());
        }
    }
    codes.sort();
    codes.dedup();
    codes
}

#[test]
fn every_recommendation_code_has_a_sentence() {
    let codes = recommendation_codes();
    assert!(codes.len() >= 5, "scanner found {codes:?}");
    let cats = catalogs();
    for code in codes {
        let key = format!("diagnose.rec.{code}");
        for (locale, keys) in &cats {
            assert!(keys.contains_key(&key), "{locale}: {key}");
        }
    }
}

/// Spanish letters and punctuation (written as escapes so this file passes its own scan).
const SPANISH: &[char] = &[
    '\u{e1}', '\u{e9}', '\u{ed}', '\u{f3}', '\u{fa}', '\u{fc}', '\u{f1}', '\u{c1}', '\u{c9}',
    '\u{cd}', '\u{d3}', '\u{da}', '\u{dc}', '\u{d1}', '\u{bf}', '\u{a1}', '\u{ab}', '\u{bb}',
];

/// Hostile/unicode test inputs allowed by A5 check 1.
fn allowed(file: &str, line: &str) -> bool {
    line.contains("// i18n-allow: test input")
        || (file == "src/gui/editor_model.rs"
            && (line.contains("\"\u{c9}L\"") || line.contains("\"\u{e9}l\"")))
        || (file == "src/ipc/pipe.rs" && line.contains("\"\u{f1}\""))
}

#[test]
fn no_spanish_outside_catalogs() {
    let root = root();
    let mut files = Vec::new();
    for dir in ["src", "tests", "examples"] {
        rust_files(&root.join(dir), &mut files);
    }
    let mut hits = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(&f).unwrap();
        for (n, line) in src.lines().enumerate() {
            if line.contains(SPANISH) && !allowed(&rel, line) {
                hits.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{} lines:\n{}",
        hits.len(),
        hits.join("\n")
    );
}
