//! End-to-end tests against a real, isolated resident and fake game windows.
//!
//! Every case creates real windows on the desktop, so all of them are `#[ignore]`d. Run them
//! alone, single-threaded, after building the fake game:
//!
//! ```text
//! cargo build --examples && cargo test --test e2e -- --ignored --test-threads=1
//! ```
//!
//! Each case starts its own resident (`BORDERLESS_SHATTER_INSTANCE` suffix, temp `APPDATA` and
//! `LOCALAPPDATA` under `target/e2e-tmp/`) and `examples/testwin` windows, measures them with
//! Win32 directly and only touches what it created; the harness guard cleans up even on panic.
//! Profiles are only ever written for the fake process `e2e-fake.exe`: a `testwin.exe` profile
//! would make the resident act on its own.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use borderless_shatter::diagnose::Verdict;
use borderless_shatter::ipc::pipe::Client;
use borderless_shatter::ipc::{
    ApplyResult, ErrorBody, ErrorCode, GetProfilesResult, ListWindowsResult, RestoreAllResult,
    SetProfilesResult, TimelineSource,
};
use borderless_shatter::islands::MonitorSel;
use borderless_shatter::profiles::Warning;
use borderless_shatter::win32::DpiAwareness;
use borderless_shatter::window_state::Rect;
use common::*;
use serde_json::{Value, json};

/// WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX, written out
/// independently of `window_state::STYLE_REMOVE`.
const FRAME: u32 = 0x00CF_0000;
/// The ex-style bits a borderless window loses, independent of `window_state::EXSTYLE_REMOVE`.
const EX_FRAME: u32 = 0x020E_0381;
const LEFT: &str = "halves-16-9/left";
const RIGHT: &str = "halves-16-9/right";
const NOTEPAD: &[&str] = &["--preset", "notepad"];
/// Size the Unity-style presets force on themselves. Fits any desktop (frame included, far
/// below a 1024x768 screen's max track size) and any `halves-16-9/right` island (at least 512
/// wide on a 1024 px screen), so it is never capped nor clipped, and always differs from it.
const FORCED: (i32, i32) = (480, 270);
const UNITY: &[&str] = &["--preset", "unity", "--client", "480x270"];
const UNITY_POPUP: &[&str] = &["--preset", "unity-popup", "--client", "480x270"];

// -------------------------------------------------------------------- helpers

fn setup(test: &str, args: &[&str]) -> (Harness, Client, TestWin) {
    let mut h = Harness::new(test);
    let c = h.start_resident();
    let w = h.testwin(args);
    (h, c, w)
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}

fn expect_borderless(s: Snap, orig: Snap) {
    assert_eq!(
        s.style,
        orig.style & !FRAME,
        "style {:#010x} (orig {:#010x})",
        s.style,
        orig.style
    );
    assert_eq!(
        s.exstyle,
        orig.exstyle & !EX_FRAME,
        "exstyle {:#010x} (orig {:#010x})",
        s.exstyle,
        orig.exstyle
    );
}

fn codes(w: &[Warning]) -> Vec<&str> {
    w.iter().map(|w| w.code.as_str()).collect()
}

fn warning<'a>(w: &'a [Warning], code: &str) -> &'a Warning {
    w.iter()
        .find(|w| w.code == code)
        .unwrap_or_else(|| panic!("no {code} warning in {w:?}"))
}

/// Codes of `error.data.warnings[]` of a `profile_error` rejection.
fn rejected_codes(e: &ErrorBody) -> Vec<String> {
    e.data
        .as_ref()
        .and_then(|d| d.get("warnings"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|w| w.get("code").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut s = String::new();
    for b in Sha256::digest(bytes).iter() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn island(id: &str, x: f64, y: f64, w: f64, h: f64) -> Value {
    json!({ "id": id, "rect": { "x": x, "y": y, "w": w, "h": h } })
}

fn island_px(id: &str, x: f64, y: f64, w: f64, h: f64) -> Value {
    json!({ "id": id, "rect": { "x": x, "y": y, "w": w, "h": h, "unit": "px" } })
}

fn layout(id: &str, monitor: Value, islands: Vec<Value>) -> Value {
    json!({ "id": id, "monitor": monitor, "islands": islands })
}

fn get_profiles(c: &mut Client) -> GetProfilesResult {
    call(c, "get_profiles", json!({}))
}

fn set_profiles(c: &mut Client, params: Value) -> SetProfilesResult {
    call(c, "set_profiles", params)
}

fn layout_ids(c: &mut Client) -> Vec<String> {
    list_islands(c).layouts.into_iter().map(|l| l.id).collect()
}

fn place_with(c: &mut Client, params: Value) -> ApplyResult {
    call(c, "place", params)
}

fn managed_hwnds(c: &mut Client) -> Vec<isize> {
    windows(c, true).iter().map(|w| w.hwnd).collect()
}

/// Asserts a window is back exactly as it was and no longer managed.
fn expect_restored(c: &mut Client, w: &TestWin) {
    let r = restore(c, w.hwnd);
    assert!(r.restored && r.exact, "restore {r:?}");
    assert_eq!(snap(w.hwnd), w.orig);
    assert!(!managed_hwnds(c).contains(&w.hwnd));
}

// ----------------------------------------------------------------------- cases

/// Creates real windows (resident + raw pipe); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn hello_and_protocol_errors() {
    let mut h = Harness::new("hello_and_protocol_errors");
    let mut c = h.start_resident();
    let hi = hello(&mut c);
    assert_eq!(hi.protocol, 2);
    assert_eq!(hi.pid, h.resident_pid());
    assert_eq!(hi.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(hi.profiles_revision.len(), 64);
    assert!(
        hi.profiles_revision
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch)),
        "{}",
        hi.profiles_revision
    );

    let e = call_err(&mut c, "no_such_method", json!({}));
    assert_eq!(e.code, ErrorCode::UnknownMethod);
    let e = call_err(&mut c, "list_windows", json!({ "include_hidden": "x" }));
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let e = call_err(&mut c, "place", json!({ "window": {}, "island": RIGHT }));
    assert_eq!(e.code, ErrorCode::InvalidParams);

    let raw = h.raw_pipe();
    let mut reader = BufReader::new(&raw);
    let mut writer: &fs::File = &raw;
    let mut exchange = |line: &str| -> Value {
        writer
            .write_all(line.as_bytes())
            .expect("write to the pipe");
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("read from the pipe");
        serde_json::from_str(&answer).unwrap_or_else(|e| panic!("bad answer {answer:?}: {e}"))
    };
    let v = exchange("not json\n");
    assert_eq!(v["error"]["code"], "invalid_json", "{v}");
    let v = exchange("{\"id\":\"abc\",\"method\":\"hello\"}\n");
    assert_eq!(v["id"], "abc", "{v}");
    assert_eq!(v["result"]["protocol"], 2, "{v}");
    // A repeated key is `invalid_params`; a repeated `id` is answered with id 0.
    let v = exchange("{\"id\":5,\"id\":6,\"method\":\"hello\"}\n");
    assert_eq!(v["id"], 0, "{v}");
    assert_eq!(v["error"]["code"], "invalid_params", "{v}");
    // An array where params has an object is never a positional struct.
    let v = exchange("{\"id\":7,\"method\":\"restore\",\"params\":{\"window\":[1]}}\n");
    assert_eq!(v["id"], 7, "{v}");
    assert_eq!(v["error"]["code"], "invalid_params", "{v}");
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn islands_resolve_to_primary_monitor_halves() {
    let mut h = Harness::new("islands_resolve_to_primary_monitor_halves");
    let mut c = h.start_resident();
    let list = list_islands(&mut c);
    let pm = primary_monitor();
    let primaries: Vec<_> = list.monitors.iter().filter(|m| m.primary).collect();
    assert_eq!(primaries.len(), 1, "{:?}", list.monitors);
    assert_eq!(primaries[0].rect, pm);
    for (i, m) in list.monitors.iter().enumerate() {
        assert_eq!(m.index, i);
    }
    let l = list
        .layouts
        .iter()
        .find(|l| l.id == "halves-16-9")
        .expect("preset listed");
    assert_eq!(l.monitor, MonitorSel::Primary);
    let mid = pm.left + (0.5 * f64::from(pm.right - pm.left)).round() as i32;
    let by_id = |id: &str| {
        l.islands
            .iter()
            .find(|i| i.id == id)
            .unwrap_or_else(|| panic!("island {id}"))
    };
    assert_eq!(by_id("left").rect, rect(pm.left, pm.top, mid, pm.bottom));
    assert_eq!(by_id("right").rect, rect(mid, pm.top, pm.right, pm.bottom));
    assert!(l.islands.iter().all(|i| i.occupants.is_empty()));
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn custom_layouts_and_remove_layouts() {
    let mut h = Harness::new("custom_layouts_and_remove_layouts");
    let mut c = h.start_resident();
    let pm = primary_monitor();
    let (w, hh) = size_of_rect(pm);
    let (l, t) = (pm.left, pm.top);
    let frac = |v: f64, n: i32| (v * f64::from(n)).round() as i32;

    set_profiles(
        &mut c,
        json!({ "layouts": [layout("e2e-grid", json!("primary"), vec![
            island_px("px", 10.0, 20.0, 300.0, 200.0),
            island("frac", 0.25, 0.0, 0.5, 0.5),
        ])] }),
    );
    assert_eq!(
        island_rect(&mut c, "e2e-grid", "px"),
        rect(l + 10, t + 20, l + 310, t + 220)
    );
    assert_eq!(
        island_rect(&mut c, "e2e-grid", "frac"),
        rect(l + frac(0.25, w), t, l + frac(0.75, w), t + frac(0.5, hh))
    );
    assert!(
        get_profiles(&mut c)
            .layouts
            .iter()
            .any(|x| x.id == "e2e-grid")
    );

    set_profiles(&mut c, json!({ "remove_layouts": ["e2e-grid"] }));
    assert!(!layout_ids(&mut c).contains(&"e2e-grid".to_string()));
    assert!(
        !get_profiles(&mut c)
            .layouts
            .iter()
            .any(|x| x.id == "e2e-grid")
    );

    let preset_islands = |c: &mut Client| {
        list_islands(c)
            .layouts
            .into_iter()
            .find(|x| x.id == "halves-16-9")
            .expect("preset listed")
            .islands
            .len()
    };
    set_profiles(
        &mut c,
        json!({ "layouts": [layout("halves-16-9", json!("primary"), vec![
            island("whole", 0.0, 0.0, 1.0, 1.0),
        ])] }),
    );
    assert_eq!(preset_islands(&mut c), 1);
    set_profiles(&mut c, json!({ "remove_layouts": ["halves-16-9"] }));
    assert_eq!(preset_islands(&mut c), 2);

    set_profiles(
        &mut c,
        json!({ "layouts": [layout("e2e-lejos", json!(99), vec![
            island("a", 0.0, 0.0, 0.5, 0.5),
        ])] }),
    );
    assert!(!layout_ids(&mut c).contains(&"e2e-lejos".to_string()));
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn invalid_layouts_are_rejected() {
    let mut h = Harness::new("invalid_layouts_are_rejected");
    let mut c = h.start_resident();
    let ok = set_profiles(
        &mut c,
        json!({ "layouts": [layout("e2e-ok", json!("primary"), vec![
            island("a", 0.0, 0.0, 0.5, 0.5),
        ])] }),
    );
    assert!(ok.warnings.is_empty(), "{:?}", ok.warnings);

    let half = |id: &str| island(id, 0.0, 0.0, 0.5, 0.5);
    let bad =
        |islands: Vec<Value>| json!({ "layouts": [layout("e2e-bad", json!("primary"), islands)] });
    let table: Vec<(&str, Value)> = vec![
        (
            "bad_layout_id",
            json!({ "layouts": [layout("", json!("primary"), vec![half("a")])] }),
        ),
        (
            "bad_layout_id",
            json!({ "layouts": [layout("a/b", json!("primary"), vec![half("a")])] }),
        ),
        ("bad_island_id", bad(vec![half("")])),
        ("bad_island_id", bad(vec![half("x/y")])),
        ("duplicate_island", bad(vec![half("a"), half("a")])),
        (
            "bad_island_rect",
            bad(vec![island("a", 0.0, 0.0, 0.0, 0.5)]),
        ),
        (
            "bad_island_rect",
            bad(vec![island("a", 0.0, 0.0, 0.5, -0.5)]),
        ),
        (
            "bad_island_rect",
            bad(vec![island("a", 0.5, 0.0, 0.7, 0.5)]),
        ),
        (
            "bad_island_rect",
            bad(vec![island("a", -0.1, 0.0, 0.5, 0.5)]),
        ),
        (
            "bad_island_rect",
            bad(vec![island_px("a", 0.0, 0.0, 200_000.0, 100.0)]),
        ),
        (
            "island_overrides_monitor",
            json!({ "profiles": [{ "process": "e2e-fake.exe", "island": RIGHT, "monitor": 0 }] }),
        ),
    ];
    for (code, params) in table {
        let before = get_profiles(&mut c).revision;
        let e = call_err(&mut c, "set_profiles", params.clone());
        assert_eq!(e.code, ErrorCode::ProfileError, "{params}");
        let got = rejected_codes(&e);
        assert!(got.iter().any(|g| g == code), "{params}: {got:?}");
        assert_eq!(get_profiles(&mut c).revision, before, "{params}");
    }
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn duplicate_layout_on_disk_is_a_warning() {
    let mut h = Harness::new("duplicate_layout_on_disk_is_a_warning");
    let dup = layout(
        "dup",
        json!("primary"),
        vec![island("a", 0.0, 0.0, 0.5, 1.0)],
    );
    h.write_profiles(
        &json!({ "version": 2, "layouts": [dup.clone(), dup], "profiles": [] }).to_string(),
    );
    let mut c = h.start_resident();
    let g = get_profiles(&mut c);
    assert!(
        codes(&g.warnings).contains(&"duplicate_layout"),
        "{:?}",
        g.warnings
    );
    assert_eq!(g.layouts.len(), 2);
    let r = set_profiles(
        &mut c,
        json!({ "layouts": [layout("e2e-other", json!("primary"), vec![
            island("b", 0.5, 0.0, 0.5, 1.0),
        ])] }),
    );
    assert!(
        codes(&r.warnings).contains(&"duplicate_layout"),
        "{:?}",
        r.warnings
    );
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn set_profiles_writes_v2_with_revision() {
    let mut h = Harness::new("set_profiles_writes_v2_with_revision");
    let mut c = h.start_resident();
    let old = get_profiles(&mut c).revision;
    let path = h.profiles_path();
    let before = fs::read(&path).expect("profiles.json exists before the first save");
    let r = set_profiles(
        &mut c,
        json!({ "profiles": [{ "process": "e2e-fake.exe", "island": RIGHT }] }),
    );
    let bytes = fs::read(&path).expect("profiles.json written");
    assert_eq!(r.revision, sha256_hex(&bytes));
    let v: Value = serde_json::from_slice(&bytes).expect("profiles.json is JSON");
    assert_eq!(v["version"], 2);
    let mut names: Vec<String> = fs::read_dir(path.parent().expect("dir"))
        .expect("read dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["profiles.json", "profiles.json.prev"]);
    let prev = fs::read(path.with_file_name("profiles.json.prev")).expect("profiles.json.prev");
    assert_eq!(prev, before);
    assert_eq!(sha256_hex(&before), old);

    let e = call_err(
        &mut c,
        "set_profiles",
        json!({ "profiles": [{ "process": "e2e-fake.exe" }], "expected_revision": old }),
    );
    assert_eq!(e.code, ErrorCode::RevisionConflict);
    assert_eq!(fs::read(&path).expect("read"), bytes);

    set_profiles(&mut c, json!({ "remove": ["E2E-FAKE.EXE"] }));
    assert!(get_profiles(&mut c).profiles.is_empty());
}

/// Creates real windows (resident); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn invalid_profiles_json_is_never_overwritten() {
    let mut h = Harness::new("invalid_profiles_json_is_never_overwritten");
    h.write_profiles("{ not json");
    let mut c = h.start_resident();
    let path = h.profiles_path();
    let fake = json!({ "profiles": [{ "process": "e2e-fake.exe" }] });
    assert_eq!(
        call_err(&mut c, "get_profiles", json!({})).code,
        ErrorCode::ProfileError
    );
    assert_eq!(
        call_err(&mut c, "set_profiles", fake.clone()).code,
        ErrorCode::ProfileError
    );
    assert_eq!(fs::read(&path).expect("read"), b"{ not json");

    let v99 = r#"{"version":99,"profiles":[]}"#;
    h.write_profiles(v99);
    assert_eq!(
        call_err(&mut c, "set_profiles", fake).code,
        ErrorCode::UnsupportedVersion
    );
    assert_eq!(fs::read(&path).expect("read"), v99.as_bytes());
}

/// Creates real windows (resident + 2 testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn list_windows_reports_window_facts() {
    let mut h = Harness::new("list_windows_reports_window_facts");
    let mut c = h.start_resident();
    let notepad = h.testwin(NOTEPAD);
    let warscape = h.testwin(&["--preset", "warscape"]);
    let primary = list_islands(&mut c)
        .monitors
        .into_iter()
        .find(|m| m.primary)
        .expect("primary monitor");
    let listed: ListWindowsResult =
        call(&mut c, "list_windows", json!({ "process": "testwin.exe" }));
    assert!(listed.windows.iter().all(|w| w.process == "testwin.exe"));
    for (n, w, awareness) in [
        (1, &notepad, DpiAwareness::PerMonitorV2),
        (2, &warscape, DpiAwareness::Unaware),
    ] {
        let info = listed
            .windows
            .iter()
            .find(|i| i.hwnd == w.hwnd)
            .unwrap_or_else(|| panic!("window {n} not listed"));
        let s = snap(w.hwnd);
        let (cw, ch) = client_size(w.hwnd);
        assert_eq!(info.pid, w.pid);
        assert_eq!(info.process, "testwin.exe");
        assert_eq!(info.title, format!("e2e-win-{n}"));
        assert_eq!(info.class, "ShatterTestWin");
        assert!(info.visible);
        assert_eq!(info.style, format!("{:#010x}", s.style));
        assert_eq!(info.exstyle, format!("{:#010x}", s.exstyle));
        assert_eq!(info.rect, s.rect);
        assert_eq!((info.client.w, info.client.h), (cw, ch));
        assert!(!info.managed);
        assert_eq!(info.island, None);
        assert!(!info.maximized && !info.minimized);
        assert_eq!(info.dpi_awareness, awareness);
        if awareness == DpiAwareness::Unaware {
            assert_eq!(info.dpi, 96);
        }
        assert_eq!(info.monitor.index, primary.index);
        assert_eq!(info.monitor.device, primary.device);
    }
}

/// Creates real windows (resident + 2 testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn window_ref_by_process_and_title() {
    let mut h = Harness::new("window_ref_by_process_and_title");
    let mut c = h.start_resident();
    let w1 = h.testwin(NOTEPAD);
    let w2 = h.testwin(NOTEPAD);
    let e = call_err(
        &mut c,
        "place",
        json!({ "window": { "process": "testwin.exe" }, "island": RIGHT }),
    );
    assert_eq!(e.code, ErrorCode::AmbiguousWindow);
    let mut candidates: Vec<i64> = e
        .data
        .as_ref()
        .and_then(|d| d.get("candidates"))
        .and_then(Value::as_array)
        .expect("data.candidates")
        .iter()
        .filter_map(|x| x.get("hwnd").and_then(Value::as_i64))
        .collect();
    candidates.sort_unstable();
    let mut expected = vec![w1.hwnd as i64, w2.hwnd as i64];
    expected.sort_unstable();
    assert_eq!(candidates, expected);

    let r = place_with(
        &mut c,
        json!({ "window": { "process": "testwin.exe", "title": "E2E-WIN-2" }, "island": RIGHT }),
    );
    assert!(r.applied);
    assert_eq!(r.hwnd, w2.hwnd);
    assert_eq!(snap(w1.hwnd), w1.orig);

    h.kill_testwin(&w1);
    let e = call_err(
        &mut c,
        "place",
        json!({ "window": { "hwnd": w1.hwnd }, "island": RIGHT }),
    );
    assert_eq!(e.code, ErrorCode::WindowNotFound);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn notepad_place_and_restore() {
    let (_h, mut c, w) = setup("notepad_place_and_restore", NOTEPAD);
    assert_ne!(w.orig.style & FRAME, 0, "notepad starts with a frame");
    let right_rect = island_rect(&mut c, "halves-16-9", "right");
    let r = place(&mut c, w.hwnd, RIGHT);
    assert!(r.applied);
    assert_eq!(r.reason, None);
    assert_eq!(r.hwnd, w.hwnd);
    assert_eq!(r.target, right_rect);
    assert_eq!(r.r#final, right_rect);
    assert!(r.settled);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let s = snap(w.hwnd);
    assert_eq!(s.rect, r.target);
    expect_borderless(s, w.orig);

    let managed = windows(&mut c, true);
    assert_eq!(managed.len(), 1, "{managed:?}");
    let m = &managed[0];
    assert_eq!(m.hwnd, w.hwnd);
    assert!(m.managed);
    assert_eq!(m.island.as_deref(), Some(RIGHT));
    assert_eq!(m.rect, s.rect);
    assert_eq!(m.style, format!("{:#010x}", s.style));
    assert_eq!(m.exstyle, format!("{:#010x}", s.exstyle));

    let list = list_islands(&mut c);
    let preset = list
        .layouts
        .iter()
        .find(|l| l.id == "halves-16-9")
        .expect("preset");
    for i in &preset.islands {
        let expected: &[isize] = if i.id == "right" { &[w.hwnd] } else { &[] };
        assert_eq!(i.occupants, expected, "island {}", i.id);
    }

    let again = place(&mut c, w.hwnd, RIGHT);
    assert!(!again.applied);
    assert_eq!(again.reason.as_deref(), Some("already_placed"));
    assert_eq!(snap(w.hwnd), s);

    expect_restored(&mut c, &w);
    let second = restore(&mut c, w.hwnd);
    assert!(!second.restored);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn notepad_diagnose_accepted() {
    let (_h, mut c, w) = setup("notepad_diagnose_accepted", NOTEPAD);
    let right_rect = island_rect(&mut c, "halves-16-9", "right");
    let d = diagnose(&mut c, w.hwnd, RIGHT, 1000, false);
    assert_eq!(d.verdict, Verdict::Accepted);
    assert_eq!(d.target, right_rect);
    assert_eq!(d.r#final, right_rect);
    assert_eq!(d.forced_size, None);
    assert!(d.recommendations.is_empty(), "{:?}", d.recommendations);
    assert!(d.restored);
    assert_eq!(d.dpi_awareness, DpiAwareness::PerMonitorV2);
    for op in ["set_styles", "set_pos"] {
        assert!(
            d.timeline
                .iter()
                .any(|e| e.source == TimelineSource::Us && e.op.as_deref() == Some(op)),
            "no {op} write in the timeline"
        );
    }
    assert_eq!(snap(w.hwnd), w.orig);
    assert!(managed_hwnds(&mut c).is_empty());
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn warscape_diagnose_place_restore() {
    let (_h, mut c, w) = setup("warscape_diagnose_place_restore", &["--preset", "warscape"]);

    let d = diagnose(&mut c, w.hwnd, RIGHT, 1500, true);
    assert_eq!(d.verdict, Verdict::StyleAndSizeReverted);
    let forced = d.forced_size.expect("forced_size");
    assert_eq!((forced.width, forced.height), size_of_rect(d.r#final));
    assert_eq!(
        (forced.width, forced.height),
        size_of_rect(snap(w.hwnd).rect)
    );
    assert!(d.revert_latency_ms.style.is_some());
    assert!(d.revert_latency_ms.rect.is_some());
    assert_eq!(d.dpi_awareness, DpiAwareness::Unaware);
    assert!(
        d.recommendations
            .iter()
            .any(|r| r.code == "warscape_resolution"),
        "{:?}",
        d.recommendations
    );
    assert!(!d.restored);
    expect_restored(&mut c, &w);

    let r = place(&mut c, w.hwnd, RIGHT);
    assert!(r.applied);
    assert_ne!(size_of_rect(r.r#final), size_of_rect(r.target));
    let size = warning(&r.warnings, "size_forced");
    assert_eq!(size.data["w"], r.r#final.right - r.r#final.left);
    assert_eq!(size.data["h"], r.r#final.bottom - r.r#final.top);
    let got = codes(&r.warnings);
    assert!(got.contains(&"style_reverted"), "{got:?}");
    assert!(got.contains(&"fights_back"), "{got:?}");
    let managed = windows(&mut c, true);
    let m = managed
        .iter()
        .find(|m| m.hwnd == w.hwnd)
        .expect("warscape managed");
    assert!(m.managed);
    assert_eq!(m.island.as_deref(), Some(RIGHT));
    expect_restored(&mut c, &w);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn unity_diagnose_place_restore() {
    let (_h, mut c, w) = setup("unity_diagnose_place_restore", UNITY);
    let isl = island_rect(&mut c, "halves-16-9", "right");
    assert_ne!(
        size_of_rect(isl),
        FORCED,
        "the island must differ from the forced size"
    );

    let d = diagnose(&mut c, w.hwnd, RIGHT, 1500, false);
    assert_eq!(d.verdict, Verdict::SizeForced);
    let forced = d.forced_size.expect("forced_size");
    assert_eq!((forced.width, forced.height), FORCED);
    assert_eq!(d.revert_latency_ms.style, None);
    assert!(d.revert_latency_ms.rect.is_some());
    assert!(
        d.recommendations.iter().any(|r| r.code == "unity_args"),
        "{:?}",
        d.recommendations
    );
    assert!(d.restored);
    assert_eq!(snap(w.hwnd), w.orig);

    let r = place(&mut c, w.hwnd, RIGHT);
    assert!(r.applied);
    assert_eq!(size_of_rect(r.r#final), FORCED);
    let size = warning(&r.warnings, "size_forced");
    assert_eq!(
        (size.data["w"].as_i64(), size.data["h"].as_i64()),
        (Some(FORCED.0 as i64), Some(FORCED.1 as i64))
    );
    assert!(!codes(&r.warnings).contains(&"style_reverted"));
    expect_borderless(snap(w.hwnd), w.orig);
    expect_restored(&mut c, &w);

    let r = place_with(
        &mut c,
        json!({
            "window": { "hwnd": w.hwnd },
            "island": RIGHT,
            "game_resolution": { "width": FORCED.0, "height": FORCED.1 },
        }),
    );
    let (cx, cy) = (
        isl.left + (isl.right - isl.left) / 2,
        isl.top + (isl.bottom - isl.top) / 2,
    );
    assert_eq!(
        r.target,
        rect(
            cx - FORCED.0 / 2,
            cy - FORCED.1 / 2,
            cx + FORCED.0 / 2,
            cy + FORCED.1 / 2
        )
    );
    assert!(
        !codes(&r.warnings).contains(&"size_forced"),
        "{:?}",
        r.warnings
    );
    assert_eq!(size_of_rect(r.r#final), FORCED);
    expect_restored(&mut c, &w);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn game_resolution_edge_cases() {
    let (_h, mut c, w) = setup("game_resolution_edge_cases", NOTEPAD);
    let isl = island_rect(&mut c, "halves-16-9", "right");
    let e = call_err(
        &mut c,
        "place",
        json!({
            "window": { "hwnd": w.hwnd },
            "island": RIGHT,
            "game_resolution": { "width": 0, "height": 720 },
        }),
    );
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert_eq!(snap(w.hwnd), w.orig);

    let (iw, ih) = size_of_rect(isl);
    let r = place_with(
        &mut c,
        json!({
            "window": { "hwnd": w.hwnd },
            "island": RIGHT,
            "game_resolution": { "width": iw + 100, "height": ih + 100 },
        }),
    );
    assert_eq!(r.target, isl);
    assert!(
        codes(&r.warnings).contains(&"game_resolution_exceeds_island"),
        "{:?}",
        r.warnings
    );
    expect_restored(&mut c, &w);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn unity_popup_place_diagnose_restore() {
    let (_h, mut c, w) = setup("unity_popup_place_diagnose_restore", UNITY_POPUP);
    assert_eq!(w.orig.style, 0x9400_0000);

    let r = place(&mut c, w.hwnd, RIGHT);
    assert!(r.applied, "{r:?}");
    assert_eq!(size_of_rect(r.r#final), FORCED);
    let size = warning(&r.warnings, "size_forced");
    assert_eq!(
        (size.data["w"].as_i64(), size.data["h"].as_i64()),
        (Some(FORCED.0 as i64), Some(FORCED.1 as i64))
    );
    assert!(!codes(&r.warnings).contains(&"style_reverted"));
    assert_eq!(snap(w.hwnd).style, 0x9400_0000);
    assert!(managed_hwnds(&mut c).contains(&w.hwnd));
    expect_restored(&mut c, &w);

    let d = diagnose(&mut c, w.hwnd, RIGHT, 1000, false);
    assert_eq!(d.verdict, Verdict::SizeForced);
    let forced = d.forced_size.expect("forced_size");
    assert_eq!((forced.width, forced.height), FORCED);
    assert_eq!(d.revert_latency_ms.style, None);
    let rect_ms = d.revert_latency_ms.rect.expect("rect latency");
    // docs/GAMES.md: the real popup reverts in < 20 ms, testwin in ~21 ms.
    assert!(rect_ms < 250.0, "rect latency {rect_ms} ms");
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn late_frame_diagnose() {
    let (_h, mut c, w) = setup(
        "late_frame_diagnose",
        &["--preset", "notepad", "--late-frame", "1500"],
    );
    let d = diagnose(&mut c, w.hwnd, RIGHT, 3000, false);
    assert_eq!(d.verdict, Verdict::LateRevert);
    let style_ms = d.revert_latency_ms.style.expect("style latency");
    assert!(
        (1000.0..2500.0).contains(&style_ms),
        "style latency {style_ms} ms"
    );
    assert!(
        d.timeline.iter().any(|e| e.source == TimelineSource::Window
            && e.t_ms >= 1000.0
            && parse_hex(&e.style) & FRAME != 0),
        "no late frame in the timeline"
    );
    assert!(d.restored);
    assert_eq!(snap(w.hwnd), w.orig);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn late_frame_place_reasserts() {
    let (_h, mut c, w) = setup(
        "late_frame_place_reasserts",
        &["--preset", "notepad", "--late-frame", "1500"],
    );
    let r = place(&mut c, w.hwnd, RIGHT);
    assert!(r.applied);
    assert!(
        !codes(&r.warnings).contains(&"style_reverted"),
        "{:?}",
        r.warnings
    );
    wait_until("the late frame", Duration::from_secs(4), || {
        w.log_text().contains("late-frame after")
    });
    wait_until("the guard to reassert", Duration::from_secs(3), || {
        let s = snap(w.hwnd);
        s.rect == r.target && s.style & FRAME == 0
    });
    expect_restored(&mut c, &w);
}

/// Creates real windows (resident killed and restarted + 2 testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn crash_readoption_keeps_island_and_restores_exact() {
    let mut h = Harness::new("crash_readoption_keeps_island_and_restores_exact");
    let mut c = h.start_resident();
    let notepad = h.testwin(NOTEPAD);
    let popup = h.testwin(UNITY_POPUP);
    let rn = place(&mut c, notepad.hwnd, RIGHT);
    let rp = place(&mut c, popup.hwnd, LEFT);
    assert!(rn.applied && rp.applied);

    drop(c);
    h.kill_resident();
    assert_eq!(snap(notepad.hwnd).rect, rn.r#final);
    assert_eq!(snap(popup.hwnd).rect, rp.r#final);

    let mut c = h.start_resident();
    wait_until("both windows re-adopted", Duration::from_secs(5), || {
        let m = managed_hwnds(&mut c);
        m.contains(&notepad.hwnd) && m.contains(&popup.hwnd)
    });
    for (w, island) in [(&notepad, RIGHT), (&popup, LEFT)] {
        let info = windows(&mut c, true)
            .into_iter()
            .find(|i| i.hwnd == w.hwnd)
            .expect("managed");
        assert_eq!(info.island.as_deref(), Some(island));
        let (layout, id) = island.split_once('/').expect("ref");
        let occupants = list_islands(&mut c)
            .layouts
            .into_iter()
            .find(|l| l.id == layout)
            .expect("layout")
            .islands
            .into_iter()
            .find(|i| i.id == id)
            .expect("island")
            .occupants;
        assert_eq!(occupants, [w.hwnd], "{island}");
    }
    expect_restored(&mut c, &notepad);
    expect_restored(&mut c, &popup);
}

/// Creates real windows (resident killed and restarted + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn state_of_dead_window_is_discarded() {
    let mut h = Harness::new("state_of_dead_window_is_discarded");
    let mut c = h.start_resident();
    let w = h.testwin(NOTEPAD);
    assert!(place(&mut c, w.hwnd, RIGHT).applied);
    drop(c);
    h.kill_resident();
    h.kill_testwin(&w);

    let mut c = h.start_resident();
    hello(&mut c);
    assert!(managed_hwnds(&mut c).is_empty());
    assert!(
        list_islands(&mut c)
            .layouts
            .iter()
            .all(|l| l.islands.iter().all(|i| i.occupants.is_empty()))
    );
    let e = call_err(&mut c, "restore", json!({ "window": { "hwnd": w.hwnd } }));
    assert_eq!(e.code, ErrorCode::WindowNotFound);
}

/// Creates real windows (resident + testwin + abandoned raw pipe client); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn killing_client_mid_diagnose_restores_window() {
    let (h, mut c, w) = setup("killing_client_mid_diagnose_restores_window", NOTEPAD);
    let right_rect = island_rect(&mut c, "halves-16-9", "right");
    let raw = h.raw_pipe();
    let request = json!({
        "id": 1,
        "method": "diagnose",
        "params": { "window": { "hwnd": w.hwnd }, "island": RIGHT, "observe_ms": 1500 },
    });
    (&raw)
        .write_all(format!("{request}\n").as_bytes())
        .expect("send diagnose");
    wait_until(
        "the diagnose to place the window",
        Duration::from_secs(3),
        || {
            let s = snap(w.hwnd);
            s.style & FRAME == 0 && s.rect == right_rect
        },
    );
    let e = call_err(
        &mut c,
        "diagnose",
        json!({ "window": { "hwnd": w.hwnd }, "island": RIGHT, "observe_ms": 500 }),
    );
    assert_eq!(e.code, ErrorCode::Busy);
    let e = call_err(
        &mut c,
        "place",
        json!({ "window": { "hwnd": w.hwnd }, "island": RIGHT }),
    );
    assert_eq!(e.code, ErrorCode::Busy);

    drop(raw);
    wait_until("the window to be restored", Duration::from_secs(10), || {
        snap(w.hwnd) == w.orig
    });
    let mut fresh = h.connect();
    hello(&mut fresh);
    assert!(!managed_hwnds(&mut fresh).contains(&w.hwnd));
    let d = diagnose(&mut fresh, w.hwnd, RIGHT, 500, false);
    assert_eq!(d.verdict, Verdict::Accepted);
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn out_of_range_params_are_invalid_and_harmless() {
    let (_h, mut c, w) = setup("out_of_range_params_are_invalid_and_harmless", NOTEPAD);
    let window = json!({ "hwnd": w.hwnd });
    let offsets = |l: i32, t: i32, r: i32, b: i32| json!({ "window": window, "offsets": { "left": l, "top": t, "right": r, "bottom": b } });
    let max = i32::MAX;
    let cases = [
        json!({ "window": window, "delay_ms": 999_999 }),
        json!({ "window": window, "delay_ms": 60_001 }),
        offsets(max, 0, 0, 0),
        offsets(0, max, 0, 0),
        offsets(0, 0, max, 0),
        offsets(0, 0, 0, max),
        offsets(i32::MIN, 0, 0, 0),
    ];
    for params in cases {
        let e = call_err(&mut c, "apply", params.clone());
        assert_eq!(e.code, ErrorCode::InvalidParams, "{params}");
        assert_eq!(snap(w.hwnd), w.orig, "{params}");
        assert!(managed_hwnds(&mut c).is_empty(), "{params}");
        hello(&mut c);
    }
}

/// Creates real windows (resident + testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn place_errors_leave_window_untouched() {
    let (_h, mut c, w) = setup("place_errors_leave_window_untouched", NOTEPAD);
    set_profiles(
        &mut c,
        json!({ "layouts": [layout("e2e-lejos", json!(99), vec![
            island("a", 0.0, 0.0, 0.5, 0.5),
        ])] }),
    );
    for (island, code) in [
        ("halves-16-9/centre", ErrorCode::IslandNotFound),
        ("noslash", ErrorCode::InvalidParams),
        ("e2e-lejos/a", ErrorCode::MonitorUnavailable),
    ] {
        let e = call_err(
            &mut c,
            "place",
            json!({ "window": { "hwnd": w.hwnd }, "island": island }),
        );
        assert_eq!(e.code, code, "{island}");
        assert_eq!(snap(w.hwnd), w.orig, "{island}");
    }
}

/// Creates real windows (two residents on the same instance); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn second_instance_exits_cleanly() {
    let mut h = Harness::new("second_instance_exits_cleanly");
    let mut c = h.start_resident();
    let status = h
        .second_instance_exit(Duration::from_secs(10))
        .expect("the second instance must exit on its own");
    assert!(status.success(), "{status}");
    assert_eq!(hello(&mut c).pid, h.resident_pid());
}

/// Creates real windows (resident + 2 testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn restore_all_restores_every_managed_window() {
    let mut h = Harness::new("restore_all_restores_every_managed_window");
    let mut c = h.start_resident();
    let notepad = h.testwin(NOTEPAD);
    let popup = h.testwin(UNITY_POPUP);
    assert!(place(&mut c, notepad.hwnd, LEFT).applied);
    assert!(place(&mut c, popup.hwnd, RIGHT).applied);

    let r: RestoreAllResult = call(&mut c, "restore_all", json!({}));
    assert_eq!(r.restored, 2);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(snap(notepad.hwnd), notepad.orig);
    assert_eq!(snap(popup.hwnd), popup.orig);
    assert!(managed_hwnds(&mut c).is_empty());
    let again: RestoreAllResult = call(&mut c, "restore_all", json!({}));
    assert_eq!(again.restored, 0);
}

/// Creates real windows (resident + 2 testwin); see module docs.
#[test]
#[ignore = "creates real windows; see module docs"]
fn wm_quit_restores_all_and_exits() {
    let mut h = Harness::new("wm_quit_restores_all_and_exits");
    let mut c = h.start_resident();
    let w1 = h.testwin(NOTEPAD);
    let w2 = h.testwin(NOTEPAD);
    assert!(place(&mut c, w1.hwnd, LEFT).applied);
    assert!(place(&mut c, w2.hwnd, RIGHT).applied);
    drop(c);

    let status = h
        .quit_resident()
        .expect("the resident must exit on WM_QUIT");
    assert!(status.success(), "{status}");
    assert_eq!(snap(w1.hwnd), w1.orig);
    assert_eq!(snap(w2.hwnd), w2.orig);
    assert!(!h.pipe_reachable(Duration::from_millis(500)));
}

/// Creates real windows (resident + testwin); see module docs.
///
/// The pre-rename preset ids stay accepted as aliases: a profile and `place`/`diagnose` with
/// the legacy ref resolve to the canonical island, engine output reports the canonical ref, and
/// `profiles.json` is never rewritten to the new ids.
#[test]
#[ignore = "creates real windows; see module docs"]
fn legacy_preset_ids_resolve() {
    const LEGACY: &str = "mitades-16-9/derecha"; // legacy_preset_ids_resolve
    let mut h = Harness::new("legacy_preset_ids_resolve");
    h.write_profiles(&format!(
        r#"{{"version":2,"profiles":[{{"process":"e2e-fake.exe","island":"{LEGACY}"}}]}}"#
    ));
    let before = fs::read(h.profiles_path()).expect("seeded profiles.json");
    let mut c = h.start_resident();
    let w = h.testwin(NOTEPAD);

    let g = get_profiles(&mut c);
    assert_eq!(g.profiles[0].island.as_deref(), Some(LEGACY));
    assert!(codes(&g.warnings).is_empty(), "{:?}", g.warnings);

    let right_rect = island_rect(&mut c, "halves-16-9", "right");
    let d = diagnose(&mut c, w.hwnd, LEGACY, 500, false);
    assert_eq!(d.target, right_rect);
    assert!(d.restored);

    let r = place(&mut c, w.hwnd, LEGACY);
    assert!(r.applied);
    assert_eq!(r.target, right_rect);
    assert_eq!(r.r#final, right_rect);
    let managed = windows(&mut c, true);
    assert_eq!(managed.len(), 1, "{managed:?}");
    assert_eq!(managed[0].island.as_deref(), Some(RIGHT));
    let preset = list_islands(&mut c)
        .layouts
        .into_iter()
        .find(|l| l.id == "halves-16-9")
        .expect("preset");
    for i in &preset.islands {
        let expected: &[isize] = if i.id == "right" { &[w.hwnd] } else { &[] };
        assert_eq!(i.occupants, expected, "island {}", i.id);
    }

    expect_restored(&mut c, &w);
    assert_eq!(fs::read(h.profiles_path()).expect("profiles.json"), before);
}
