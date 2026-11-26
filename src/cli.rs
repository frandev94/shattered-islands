//! Command-line client (`borderless-shatter <subcommand>`, docs/V0.2.md "CLI"): one IPC request
//! per run, sent to the resident, which the CLI never starts. Exit codes: 0 ok (result JSON on
//! stdout), 1 the resident answered `error` (`{"error":…}` on stdout), 2 usage error (stderr,
//! resident not contacted), 3 resident unreachable / transport failure / protocol mismatch.
//! Usage and the CLI's own errors are localized (`cli.*` keys); stdout JSON never is.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::time::Duration;

use serde_json::{Map, Value};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_OUTPUT_HANDLE,
};

use crate::i18n::{t, tf};
use crate::ipc::pipe::{Client, InvalidInstance, instance_suffix};
use crate::ipc::{
    self, ApplyParams, CallError, DiagnoseParams, ErrorBody, GetProfilesParams, IpcError,
    ListIslandsParams, ListWindowsParams, MAX_DELAY_MS, MAX_LINE, MAX_OBSERVE_MS, PlaceParams,
    RestoreAllParams, RestoreParams, SetProfilesParams,
};

/// The CLI only checks whether the resident is there; it never starts it.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Largest `profiles set` body: a request line (≤ [`MAX_LINE`]) also carries the envelope.
const MAX_BODY: usize = MAX_LINE - 256;

/// The usage text in the current locale (`help` prints exactly this).
pub fn usage() -> Cow<'static, str> {
    t("cli.usage")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Help,
    Call(Invocation),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub method: &'static str,
    pub params: Value,
    pub pretty: bool,
}

/// Reads the `profiles set` body: `-` = stdin, anything else = a file path.
pub type ReadBody<'a> = &'a mut dyn FnMut(&str) -> io::Result<Vec<u8>>;

/// Flags of one subcommand: name and whether it takes a value.
type Spec = &'static [(&'static str, bool)];

const NONE: Spec = &[];
const LIST_WINDOWS: Spec = &[
    ("--process", true),
    ("--include-hidden", false),
    ("--only-managed", false),
];
const WINDOW: Spec = &[("--hwnd", true), ("--process", true), ("--title", true)];
const APPLY: Spec = &[
    ("--hwnd", true),
    ("--process", true),
    ("--title", true),
    ("--island", true),
    ("--monitor", true),
    ("--offsets", true),
    ("--delay-ms", true),
    ("--no-profile", false),
];
const PLACE: Spec = &[
    ("--hwnd", true),
    ("--process", true),
    ("--title", true),
    ("--island", true),
    ("--game-resolution", true),
];
const DIAGNOSE: Spec = &[
    ("--hwnd", true),
    ("--process", true),
    ("--title", true),
    ("--island", true),
    ("--monitor", true),
    ("--observe-ms", true),
    ("--keep", false),
];
const PROFILES_SET: Spec = &[("--expected-revision", true)];

/// Flag → value (`""` for boolean flags). Sorted, so validation order ignores argument order.
type Flags<'a> = BTreeMap<&'static str, &'a str>;

/// Command line (without the program name) → IPC request. Pure apart from `read_body`.
pub fn parse(args: &[String], read_body: ReadBody) -> Result<Parsed, String> {
    if args.first().is_some_and(|a| a == "help") || args.iter().any(|a| a == "--help") {
        return Ok(Parsed::Help);
    }
    let pretty = match args.iter().filter(|a| *a == "--pretty").count() {
        0 => false,
        1 => true,
        _ => return Err(tf("cli.error.repeated_flag", &[("flag", &"--pretty")])),
    };
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--pretty")
        .collect();
    let (method, spec, rest) = subcommand(&args)?;
    let (flags, positional) = collect(rest, spec)?;
    let params = build(method, &flags, &positional, read_body)?;
    validate(method, &params)?;
    Ok(Parsed::Call(Invocation {
        method,
        params,
        pretty,
    }))
}

fn subcommand<'a>(args: &'a [&'a str]) -> Result<(&'static str, Spec, &'a [&'a str]), String> {
    let (method, spec, words) = match args {
        [] => return Err(t("cli.error.missing_subcommand").into_owned()),
        ["list-windows", ..] => ("list_windows", LIST_WINDOWS, 1),
        ["apply", ..] => ("apply", APPLY, 1),
        ["place", ..] => ("place", PLACE, 1),
        ["restore", ..] => ("restore", WINDOW, 1),
        ["restore-all", ..] => ("restore_all", NONE, 1),
        ["list-islands", ..] => ("list_islands", NONE, 1),
        ["diagnose", ..] => ("diagnose", DIAGNOSE, 1),
        ["profiles", "get", ..] => ("get_profiles", NONE, 2),
        ["profiles", "set", ..] => ("set_profiles", PROFILES_SET, 2),
        ["profiles", ..] => return Err(t("cli.error.profiles_needs_get_or_set").into_owned()),
        [other, ..] => return Err(tf("cli.error.unknown_subcommand", &[("name", other)])),
    };
    Ok((method, spec, &args[words..]))
}

fn collect<'a>(rest: &[&'a str], spec: Spec) -> Result<(Flags<'a>, Vec<&'a str>), String> {
    let mut flags = Flags::new();
    let mut positional = Vec::new();
    let mut it = rest.iter().copied();
    while let Some(a) = it.next() {
        if !a.starts_with("--") {
            positional.push(a);
            continue;
        }
        let Some(&(name, takes_value)) = spec.iter().find(|(n, _)| *n == a) else {
            return Err(tf("cli.error.unknown_option", &[("option", &a)]));
        };
        let value = if takes_value {
            match it.next() {
                Some(v) if !v.is_empty() && !v.starts_with("--") => v,
                _ => return Err(tf("cli.error.missing_value", &[("flag", &name)])),
            }
        } else {
            ""
        };
        if flags.insert(name, value).is_some() {
            return Err(tf("cli.error.repeated_flag", &[("flag", &name)]));
        }
    }
    Ok((flags, positional))
}

fn build(
    method: &str,
    flags: &Flags,
    positional: &[&str],
    read_body: ReadBody,
) -> Result<Value, String> {
    let mut p = match (method, positional) {
        ("set_profiles", [path]) => read_profiles_body(path, read_body)?,
        ("set_profiles", _) => return Err(t("cli.error.profiles_set_needs_file").into_owned()),
        (_, []) => Map::new(),
        (_, [a, ..]) => return Err(tf("cli.error.unexpected_argument", &[("arg", a)])),
    };
    if matches!(method, "apply" | "place" | "restore" | "diagnose") {
        p.insert("window".into(), window(flags)?);
    }
    for (&flag, &v) in flags {
        let (key, value) = match flag {
            // Parts of `window`, except `list-windows --process` (a filter).
            "--hwnd" | "--title" => continue,
            "--process" if method != "list_windows" => continue,
            "--process" => ("process", Value::from(v)),
            "--include-hidden" => ("include_hidden", Value::Bool(true)),
            "--only-managed" => ("only_managed", Value::Bool(true)),
            "--keep" => ("keep", Value::Bool(true)),
            "--no-profile" => ("use_profile", Value::Bool(false)),
            "--island" => ("island", island(v)?),
            "--monitor" => ("monitor", monitor(v)?),
            "--offsets" => ("offsets", offsets(v)?),
            "--delay-ms" => ("delay_ms", bounded(flag, v, MAX_DELAY_MS)?),
            "--observe-ms" => ("observe_ms", bounded(flag, v, MAX_OBSERVE_MS)?),
            "--game-resolution" => ("game_resolution", resolution(v)?),
            "--expected-revision" => ("expected_revision", Value::from(v)),
            _ => return Err(tf("cli.error.unsupported_option", &[("flag", &flag)])),
        };
        p.insert(key.into(), value);
    }
    if method == "place" && !flags.contains_key("--island") {
        return Err(t("cli.error.place_needs_island").into_owned());
    }
    Ok(Value::Object(p))
}

fn window(flags: &Flags) -> Result<Value, String> {
    let mut w = Map::new();
    match (
        flags.get("--hwnd"),
        flags.get("--process"),
        flags.get("--title"),
    ) {
        (Some(h), None, None) => {
            w.insert("hwnd".into(), Value::from(hwnd(h)?));
        }
        (None, Some(&p), t) => {
            w.insert("process".into(), Value::from(p));
            if let Some(&t) = t {
                w.insert("title".into(), Value::from(t));
            }
        }
        (Some(_), ..) => return Err(t("cli.error.hwnd_exclusive").into_owned()),
        (None, None, Some(_)) => return Err(t("cli.error.title_needs_process").into_owned()),
        (None, None, None) => return Err(t("cli.error.missing_window").into_owned()),
    }
    Ok(Value::Object(w))
}

fn number<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, String> {
    v.parse()
        .map_err(|_| tf("cli.error.invalid_value", &[("flag", &flag), ("value", &v)]))
}

fn hwnd(v: &str) -> Result<isize, String> {
    // shortcut: negative HWNDs (sign-extended 32-bit handles with the high bit set) are rejected
    // along with pseudo-handles like -1; never seen for top-level windows, relax if one shows up.
    match number::<isize>("--hwnd", v)? {
        h if h > 0 => Ok(h),
        _ => Err(tf("cli.error.invalid_hwnd", &[("value", &v)])),
    }
}

fn bounded(flag: &str, v: &str, max: u64) -> Result<Value, String> {
    match number::<u64>(flag, v)? {
        n if n <= max => Ok(Value::from(n)),
        _ => Err(tf(
            "cli.error.out_of_range",
            &[("flag", &flag), ("max", &max)],
        )),
    }
}

fn island(v: &str) -> Result<Value, String> {
    crate::islands::parse_ref(v)
        .map(|_| Value::from(v))
        .map_err(|_| tf("cli.error.invalid_island", &[("value", &v)]))
}

/// Integer → monitor index; anything else → device name or `primary`.
fn monitor(v: &str) -> Result<Value, String> {
    if v.bytes()
        .all(|b| b.is_ascii_digit() || b == b'-' || b == b'+')
    {
        number::<usize>("--monitor", v).map(Value::from)
    } else {
        Ok(Value::from(v))
    }
}

fn offsets(v: &str) -> Result<Value, String> {
    let n = v
        .split(',')
        .map(|s| number::<i32>("--offsets", s.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    let [left, top, right, bottom] = n[..] else {
        return Err(tf("cli.error.invalid_offsets", &[("value", &v)]));
    };
    let mut m = Map::new();
    for (k, x) in [
        ("left", left),
        ("top", top),
        ("right", right),
        ("bottom", bottom),
    ] {
        m.insert(k.into(), Value::from(x));
    }
    Ok(Value::Object(m))
}

fn resolution(v: &str) -> Result<Value, String> {
    let size = v
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?)))
        .filter(|&(w, h)| w > 0 && h > 0);
    let Some((w, h)) = size else {
        return Err(tf("cli.error.invalid_resolution", &[("value", &v)]));
    };
    let mut m = Map::new();
    m.insert("width".into(), Value::from(w));
    m.insert("height".into(), Value::from(h));
    Ok(Value::Object(m))
}

fn read_profiles_body(path: &str, read_body: ReadBody) -> Result<Map<String, Value>, String> {
    let shown = if path == "-" { "stdin" } else { path };
    let bytes = read_body(path)
        .map_err(|e| tf("cli.error.read_failed", &[("file", &shown), ("reason", &e)]))?;
    if bytes.len() > MAX_BODY {
        return Err(tf(
            "cli.error.body_too_large",
            &[("file", &shown), ("max", &MAX_BODY)],
        ));
    }
    // PowerShell 5 writes UTF-8 files with a BOM and `>` produces UTF-16: strip the first, name the second.
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(tf("cli.error.body_utf16", &[("file", &shown)]));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    match serde_json::from_slice(bytes) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(tf("cli.error.body_not_object", &[("file", &shown)])),
        Err(e) => Err(tf(
            "cli.error.body_invalid_json",
            &[("file", &shown), ("reason", &e)],
        )),
    }
}

/// Same deserialisation the resident applies, so a shape error never leaves this process.
fn validate(method: &str, params: &Value) -> Result<(), String> {
    let p = params.clone();
    match method {
        "list_windows" => ipc::parse_params::<ListWindowsParams>(p).map(drop),
        "apply" => ipc::parse_params::<ApplyParams>(p).map(drop),
        "place" => ipc::parse_params::<PlaceParams>(p).map(drop),
        "restore" => ipc::parse_params::<RestoreParams>(p).map(drop),
        "restore_all" => ipc::parse_params::<RestoreAllParams>(p).map(drop),
        "list_islands" => ipc::parse_params::<ListIslandsParams>(p).map(drop),
        "get_profiles" => ipc::parse_params::<GetProfilesParams>(p).map(drop),
        "set_profiles" => ipc::parse_params::<SetProfilesParams>(p).map(drop),
        "diagnose" => ipc::parse_params::<DiagnoseParams>(p).map(drop),
        _ => return Err(tf("cli.error.unknown_method", &[("method", &method)])),
    }
    .map_err(|e| e.message)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done(Value),
    Remote(ErrorBody),
    Usage(String),
    Unreachable(String),
}

/// Outcome → (text, exit code). 0/1 go to stdout, 2/3 to stderr (see `emit`).
pub fn render(outcome: &Outcome, pretty: bool) -> (String, i32) {
    let json = |v: &Value| {
        if pretty {
            serde_json::to_string_pretty(v)
        } else {
            serde_json::to_string(v)
        }
        .unwrap_or_else(|e| e.to_string())
    };
    match outcome {
        Outcome::Done(v) => (json(v), 0),
        Outcome::Remote(e) => {
            let mut m = Map::new();
            m.insert(
                "error".into(),
                serde_json::to_value(e).unwrap_or(Value::Null),
            );
            (json(&Value::Object(m)), 1)
        }
        Outcome::Usage(msg) => (
            tf("cli.usage_error", &[("message", msg), ("usage", &usage())]),
            2,
        ),
        Outcome::Unreachable(msg) => (msg.clone(), 3),
    }
}

fn args_to_strings(args: Vec<OsString>) -> Result<Vec<String>, String> {
    args.into_iter()
        .map(|a| {
            a.into_string().map_err(|a| {
                tf(
                    "cli.error.invalid_argument",
                    &[("arg", &a.to_string_lossy())],
                )
            })
        })
        .collect()
}

fn read_body(path: &str) -> io::Result<Vec<u8>> {
    // One byte past the limit is enough to report "too big" without reading everything.
    let limit = MAX_BODY as u64 + 1;
    let mut buf = Vec::new();
    if path == "-" {
        io::stdin().lock().take(limit).read_to_end(&mut buf)?;
    } else {
        std::fs::File::open(path)?
            .take(limit)
            .read_to_end(&mut buf)?;
    }
    Ok(buf)
}

/// One request on its own pipe connection; protocol checked with `hello` first.
fn call(inv: &Invocation) -> Outcome {
    let mut client = match Client::connect(CONNECT_TIMEOUT) {
        Ok(c) => c,
        Err(IpcError::NotRunning) => {
            return Outcome::Unreachable(t("cli.error.not_running").into_owned());
        }
        Err(e) => return Outcome::Unreachable(tf("cli.error.transport", &[("reason", &e)])),
    };
    if let Err(e) = client
        .call("hello", Value::Object(Map::new()))
        .and_then(crate::mcp::check_hello)
    {
        return Outcome::Unreachable(tf("cli.error.protocol", &[("reason", &e)]));
    }
    match client.call(inv.method, inv.params.clone()) {
        Ok(v) => Outcome::Done(v),
        Err(CallError::Remote(e)) => Outcome::Remote(e),
        Err(CallError::Ipc(e)) => {
            Outcome::Unreachable(tf("cli.error.transport", &[("reason", &e)]))
        }
    }
}

/// Release builds are GUI-subsystem: run from a console without redirection they start with no
/// standard handles, so borrow the parent's console. AttachConsole replaces only NULL handles
/// when the parent redirected any (STARTF_USESTDHANDLES), so `> f.json` and pipes keep working.
fn attach_parent_console() {
    let missing = |which: STD_HANDLE| {
        // SAFETY: GetStdHandle only reads this process's standard handle table.
        match unsafe { GetStdHandle(which) } {
            Ok(h) => h.is_invalid(),
            Err(_) => true,
        }
    };
    if missing(STD_OUTPUT_HANDLE) || missing(STD_ERROR_HANDLE) {
        // SAFETY: plain call with a constant argument. Failure (no parent console, already
        // attached) is ignored: output then goes nowhere, the exit code still reports.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

/// The only stdout writer in `src/` (AC-4): exit codes 0/1 → stdout, 2/3 → stderr. Write errors
/// (closed pipe, no console) are ignored: the exit code still carries the outcome.
fn emit(text: &str, code: i32) {
    let line = format!("{text}\n");
    if code < 2 {
        let mut out = io::stdout().lock();
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    } else {
        let _ = io::stderr().lock().write_all(line.as_bytes());
    }
}

/// A set-but-invalid `BORDERLESS_SHATTER_INSTANCE` is a usage error (exit 2): never fall back to
/// the real resident's pipe.
fn check_instance(suffix: Result<Option<String>, InvalidInstance>) -> Result<(), String> {
    suffix
        .map(drop)
        .map_err(|e| tf("cli.error.invalid_instance", &[("value", &e.0)]))
}

/// `borderless-shatter <subcommand> …` (program name already stripped). Returns the exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    attach_parent_console();
    crate::i18n::init();
    let parsed = check_instance(instance_suffix())
        .and_then(|()| args_to_strings(args))
        .and_then(|a| parse(&a, &mut read_body));
    let (text, code) = match parsed {
        Ok(Parsed::Help) => (usage().into_owned(), 0),
        Ok(Parsed::Call(inv)) => render(&call(&inv), inv.pretty),
        Err(msg) => render(&Outcome::Usage(msg), false),
    };
    emit(&text, code);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Locale, scoped_locale};
    use crate::ipc::{
        self, ApplyParams, DiagnoseParams, ErrorCode, GetProfilesParams, ListIslandsParams,
        ListWindowsParams, MAX_LINE, PlaceParams, RestoreAllParams, RestoreParams,
        SetProfilesParams,
    };
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;
    use serde_json::json;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Parse with a reader that fails: only `profiles set` tests read a body.
    fn p(a: &[&str]) -> Result<Parsed, String> {
        parse(&args(a), &mut |_| Err(io::Error::other("no body")))
    }

    /// Parse `profiles set` with `body` as the content of every file (and stdin).
    fn p_body(a: &[&str], body: &[u8]) -> Result<Parsed, String> {
        let body = body.to_vec();
        parse(&args(a), &mut move |_| Ok(body.clone()))
    }

    fn call(a: &[&str]) -> (&'static str, Value) {
        match p(a) {
            Ok(Parsed::Call(i)) => (i.method, i.params),
            other => panic!("{a:?} → {other:?}"),
        }
    }

    fn usage_err(a: &[&str]) -> String {
        match p(a) {
            Err(m) => {
                assert!(!m.is_empty(), "{a:?}: empty message");
                assert!(
                    !m.contains("cli.error."),
                    "{a:?}: missing catalog key in {m:?}"
                );
                m
            }
            ok => panic!("{a:?} should be a usage error, got {ok:?}"),
        }
    }

    // ------------------------------------------------------------ subcommands

    #[test]
    fn list_windows_maps_flags() {
        assert_eq!(call(&["list-windows"]), ("list_windows", json!({})));
        assert_eq!(
            call(&[
                "list-windows",
                "--process",
                "game.exe",
                "--include-hidden",
                "--only-managed"
            ]),
            (
                "list_windows",
                json!({"process": "game.exe", "include_hidden": true, "only_managed": true})
            )
        );
    }

    #[test]
    fn apply_maps_every_flag() {
        let (m, params) = call(&[
            "apply",
            "--hwnd",
            "4242",
            "--island",
            "halves-16-9/right",
            "--monitor",
            "1",
            "--offsets",
            "1,-2,3,4",
            "--delay-ms",
            "60000",
            "--no-profile",
        ]);
        assert_eq!(m, "apply");
        assert_eq!(
            params,
            json!({
                "window": {"hwnd": 4242},
                "island": "halves-16-9/right",
                "monitor": 1,
                "offsets": {"left": 1, "top": -2, "right": 3, "bottom": 4},
                "delay_ms": 60000,
                "use_profile": false
            })
        );
    }

    #[test]
    fn apply_with_process_and_title() {
        assert_eq!(
            call(&["apply", "--process", "game.exe", "--title", "Main Window"]),
            (
                "apply",
                json!({"window": {"process": "game.exe", "title": "Main Window"}})
            )
        );
    }

    #[test]
    fn monitor_variants() {
        for (v, want) in [
            ("1", json!(1)),
            ("0", json!(0)),
            ("primary", json!("primary")),
            (r"\\.\DISPLAY2", json!(r"\\.\DISPLAY2")),
        ] {
            let (_, params) = call(&["apply", "--hwnd", "1", "--monitor", v]);
            assert_eq!(params["monitor"], want, "{v}");
        }
    }

    #[test]
    fn place_maps_island_and_resolution() {
        assert_eq!(
            call(&[
                "place",
                "--hwnd",
                "7",
                "--island",
                "a/b",
                "--game-resolution",
                "1920x1080"
            ]),
            (
                "place",
                json!({"window": {"hwnd": 7}, "island": "a/b", "game_resolution": {"width": 1920, "height": 1080}})
            )
        );
    }

    #[test]
    fn restore_and_restore_all() {
        assert_eq!(
            call(&["restore", "--hwnd", "9"]),
            ("restore", json!({"window": {"hwnd": 9}}))
        );
        assert_eq!(call(&["restore-all"]), ("restore_all", json!({})));
    }

    #[test]
    fn list_islands_and_profiles_get() {
        assert_eq!(call(&["list-islands"]), ("list_islands", json!({})));
        assert_eq!(call(&["profiles", "get"]), ("get_profiles", json!({})));
    }

    #[test]
    fn diagnose_maps_flags() {
        assert_eq!(
            call(&[
                "diagnose",
                "--process",
                "g.exe",
                "--island",
                "a/b",
                "--monitor",
                "primary",
                "--observe-ms",
                "15000",
                "--keep"
            ]),
            (
                "diagnose",
                json!({"window": {"process": "g.exe"}, "island": "a/b", "monitor": "primary", "observe_ms": 15000, "keep": true})
            )
        );
    }

    #[test]
    fn profiles_set_reads_file_and_adds_revision() {
        let mut asked = Vec::new();
        let r = parse(
            &args(&[
                "profiles",
                "set",
                "profiles.json",
                "--expected-revision",
                "abc",
            ]),
            &mut |path| {
                asked.push(path.to_string());
                Ok(br#"{"remove":["old.exe"]}"#.to_vec())
            },
        );
        assert_eq!(asked, ["profiles.json"]);
        assert_eq!(
            r,
            Ok(Parsed::Call(Invocation {
                method: "set_profiles",
                params: json!({"remove": ["old.exe"], "expected_revision": "abc"}),
                pretty: false,
            }))
        );
    }

    #[test]
    fn profiles_set_stdin_valid_body() {
        let r = p_body(&["profiles", "set", "-"], br#"{"remove_layouts":["x"]}"#);
        assert_eq!(
            r,
            Ok(Parsed::Call(Invocation {
                method: "set_profiles",
                params: json!({"remove_layouts": ["x"]}),
                pretty: false,
            }))
        );
    }

    #[test]
    fn profiles_set_accepts_utf8_bom() {
        let mut body = b"\xEF\xBB\xBF".to_vec();
        body.extend(br#"{"remove_layouts":["x"]}"#);
        let r = p_body(&["profiles", "set", "-"], &body);
        assert_eq!(
            r,
            Ok(Parsed::Call(Invocation {
                method: "set_profiles",
                params: json!({"remove_layouts": ["x"]}),
                pretty: false,
            }))
        );
    }

    #[test]
    fn profiles_set_utf16_body_gets_a_clear_error() {
        let _g = scoped_locale(Locale::En);
        let units = |le: bool| {
            let mut b = if le {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            for u in r#"{"remove":["a"]}"#.encode_utf16() {
                b.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            b
        };
        for le in [true, false] {
            let e = p_body(&["profiles", "set", "-"], &units(le)).unwrap_err();
            assert!(e.contains("UTF-16"), "{e}");
        }
    }

    #[test]
    fn profiles_set_stdin_invalid_bodies() {
        for body in [
            &b"not json"[..],
            b"",
            b"[1,2]",
            b"\"x\"",
            br#"{"profiles":"x"}"#,
            br#"{"remove":[1]}"#,
            b"{\"remove\":[\"\xff\"]}",
        ] {
            assert!(
                p_body(&["profiles", "set", "-"], body).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn profiles_set_rejects_huge_body() {
        let mut body = br#"{"remove":[""#.to_vec();
        body.extend(std::iter::repeat_n(b'a', MAX_LINE));
        body.extend(br#""]}"#);
        assert!(p_body(&["profiles", "set", "-"], &body).is_err());
    }

    #[test]
    fn profiles_set_read_error_is_usage() {
        let e = p(&["profiles", "set", "missing.json"]).unwrap_err();
        assert!(e.contains("missing.json"), "{e}");
    }

    #[test]
    fn help_variants() {
        assert_eq!(p(&["help"]), Ok(Parsed::Help));
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert_eq!(p(&["apply", "--help"]), Ok(Parsed::Help));
    }

    #[test]
    fn pretty_anywhere() {
        for a in [
            &["--pretty", "list-islands"][..],
            &["list-islands", "--pretty"],
            &["restore", "--pretty", "--hwnd", "3"],
            &["profiles", "--pretty", "get"],
        ] {
            match p(a) {
                Ok(Parsed::Call(i)) => assert!(i.pretty, "{a:?}"),
                other => panic!("{a:?} → {other:?}"),
            }
        }
        match p(&["list-islands"]) {
            Ok(Parsed::Call(i)) => assert!(!i.pretty),
            other => panic!("{other:?}"),
        }
    }

    // ------------------------------------------------------------ usage errors

    #[test]
    fn usage_errors() {
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            usage_errors_in_current_locale();
        }
    }

    #[test]
    fn usage_error_text_follows_locale() {
        {
            let _g = scoped_locale(Locale::En);
            assert_eq!(usage_err(&["nope"]), "unknown subcommand \"nope\"");
            assert_eq!(
                usage_err(&["apply", "--hwnd", "1", "--delay-ms", "60001"]),
                "--delay-ms out of range (maximum 60000)"
            );
        }
        let _g = scoped_locale(Locale::Es);
        assert_eq!(
            usage_err(&["nope"]),
            tf("cli.error.unknown_subcommand", &[("name", &"nope")])
        );
        assert_ne!(usage_err(&["nope"]), "unknown subcommand \"nope\"");
    }

    fn usage_errors_in_current_locale() {
        for a in [
            &[][..],
            &["nope"],
            &["list-windows", "--bogus"],
            &["list-windows", "-x"],
            &["list-windows", "extra"],
            &["list-islands", "--pretty", "--pretty"],
            &["apply"],
            &["apply", "--hwnd"],
            &["apply", "--hwnd", "--no-profile"],
            &["apply", "--hwnd", ""],
            &["apply", "--hwnd", "1", "--hwnd", "2"],
            &["apply", "--hwnd", "1", "--no-profile", "--no-profile"],
            &["apply", "--hwnd", "1", "--process", "g.exe"],
            &["apply", "--title", "x"],
            &["apply", "--hwnd", "1", "--title", "x"],
            &["apply", "--hwnd", "-1"],
            &["apply", "--hwnd", "0"],
            &["apply", "--hwnd", "abc"],
            &["apply", "--hwnd", "99999999999999999999"],
            &["apply", "--hwnd", "1", "--offsets", "1,2,3"],
            &["apply", "--hwnd", "1", "--offsets", "1,2,3,4,5"],
            &["apply", "--hwnd", "1", "--offsets", "1,2,3,x"],
            &["apply", "--hwnd", "1", "--delay-ms", "60001"],
            &["apply", "--hwnd", "1", "--delay-ms", "-1"],
            &["apply", "--hwnd", "1", "--monitor", "-1"],
            &[
                "apply",
                "--hwnd",
                "1",
                "--monitor",
                "99999999999999999999999",
            ],
            &["apply", "--hwnd", "1", "--island", "no-slash"],
            &["apply", "--hwnd", "1", "--island", "a/b/c"],
            &["apply", "--hwnd", "1", "--keep"],
            &["place", "--hwnd", "1"],
            &["place", "--island", "a/b"],
            &[
                "place",
                "--hwnd",
                "1",
                "--island",
                "a/b",
                "--game-resolution",
                "12x",
            ],
            &[
                "place",
                "--hwnd",
                "1",
                "--island",
                "a/b",
                "--game-resolution",
                "0x10",
            ],
            &[
                "place",
                "--hwnd",
                "1",
                "--island",
                "a/b",
                "--game-resolution",
                "axb",
            ],
            &["restore"],
            &["restore", "--hwnd", "1", "--island", "a/b"],
            &["restore-all", "--hwnd", "1"],
            &["diagnose", "--hwnd", "1", "--observe-ms", "15001"],
            &["diagnose", "--hwnd", "1", "--observe-ms", "abc"],
            &["profiles"],
            &["profiles", "bogus"],
            &["profiles", "get", "extra"],
            &["profiles", "set"],
            &["profiles", "set", "a.json", "b.json"],
            &["profiles", "set", "a.json", "--expected-revision"],
            &["profiles", "get", "--expected-revision", "r"],
        ] {
            usage_err(a);
        }
    }

    #[test]
    fn invalid_utf16_argument_is_usage() {
        use std::os::windows::ffi::OsStringExt;
        let bad = OsString::from_wide(&[0x61, 0xD800, 0x62]);
        let _g = scoped_locale(Locale::En);
        let e = args_to_strings(vec!["list-windows".into(), "--process".into(), bad]);
        assert_eq!(
            e,
            Err("argument with invalid characters: \"a\u{FFFD}b\"".into())
        );
        assert_eq!(
            args_to_strings(vec!["list-islands".into()]),
            Ok(vec!["list-islands".to_string()])
        );
    }

    // ------------------------------------------------------------------ render

    #[test]
    fn render_ok_compact_and_pretty() {
        let v = json!({"a": 1, "b": [2]});
        assert_eq!(
            render(&Outcome::Done(v.clone()), false),
            (r#"{"a":1,"b":[2]}"#.to_string(), 0)
        );
        let (text, code) = render(&Outcome::Done(v.clone()), true);
        assert_eq!(code, 0);
        assert!(text.lines().count() > 1, "{text}");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    }

    #[test]
    fn render_remote_error_exit_1() {
        let e = ErrorBody::new(ErrorCode::WindowNotFound, "no window").with_data(json!(7));
        let (text, code) = render(&Outcome::Remote(e), false);
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"error": {"code": "window_not_found", "message": "no window", "data": 7}})
        );
    }

    #[test]
    fn render_usage_exit_2_with_localized_usage() {
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            let (text, code) = render(&Outcome::Usage("missing window".into()), false);
            assert_eq!(code, 2);
            assert_eq!(text, format!("error: missing window\n\n{}", usage()));
        }
    }

    #[test]
    fn invalid_instance_is_a_localized_usage_error() {
        use crate::ipc::pipe::parse_instance;
        assert_eq!(check_instance(parse_instance(None)), Ok(()));
        assert_eq!(check_instance(parse_instance(Some(""))), Ok(()));
        assert_eq!(check_instance(parse_instance(Some("fix-ipc-2"))), Ok(()));
        let _g = scoped_locale(Locale::En);
        let en = check_instance(parse_instance(Some("fix ipc"))).expect_err("invalid");
        assert_eq!(
            en,
            "BORDERLESS_SHATTER_INSTANCE \"fix ipc\" is invalid: use 1 to 32 characters from \
             A-Z, a-z, 0-9 and -, or leave it empty"
        );
        assert_eq!(render(&Outcome::Usage(en.clone()), false).1, 2);
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        let es = check_instance(parse_instance(Some("fix ipc"))).expect_err("invalid");
        assert!(es.contains("fix ipc") && es != en, "{es}");
    }

    #[test]
    fn help_is_the_locale_usage() {
        let _g = scoped_locale(Locale::En);
        assert!(usage().starts_with("Usage: borderless-shatter <subcommand>"));
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        assert_eq!(usage(), t("cli.usage"));
        assert!(!usage().starts_with("Usage:"));
    }

    #[test]
    fn render_unreachable_exit_3() {
        let (text, code) = render(&Outcome::Unreachable("not running".into()), true);
        assert_eq!((text.as_str(), code), ("not running", 3));
    }

    #[test]
    fn usage_lists_every_subcommand() {
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            for s in [
                "list-windows",
                "apply",
                "place",
                "restore",
                "restore-all",
                "list-islands",
                "profiles get",
                "profiles set",
                "diagnose",
                "--pretty",
            ] {
                assert!(usage().contains(s), "{l:?} {s}");
            }
        }
    }

    // -------------------------------------------------------------- properties

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0xB05EED),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    /// Independent of `parse`: what the resident would do with these params.
    fn resident_accepts(method: &str, params: Value) -> bool {
        match method {
            "list_windows" => ipc::parse_params::<ListWindowsParams>(params).is_ok(),
            "apply" => ipc::parse_params::<ApplyParams>(params).is_ok(),
            "place" => ipc::parse_params::<PlaceParams>(params).is_ok(),
            "restore" => ipc::parse_params::<RestoreParams>(params).is_ok(),
            "restore_all" => ipc::parse_params::<RestoreAllParams>(params).is_ok(),
            "list_islands" => ipc::parse_params::<ListIslandsParams>(params).is_ok(),
            "get_profiles" => ipc::parse_params::<GetProfilesParams>(params).is_ok(),
            "set_profiles" => ipc::parse_params::<SetProfilesParams>(params).is_ok(),
            "diagnose" => ipc::parse_params::<DiagnoseParams>(params).is_ok(),
            _ => false,
        }
    }

    const SUBCOMMANDS: &[&[&str]] = &[
        &["list-windows"],
        &["apply"],
        &["place"],
        &["restore"],
        &["restore-all"],
        &["list-islands"],
        &["profiles", "get"],
        &["profiles", "set"],
        &["diagnose"],
    ];

    /// Flag groups (flag + value) the generators mix, hostile values included.
    fn token_group() -> impl Strategy<Value = Vec<String>> {
        let value_flag = prop::sample::select(vec![
            "--hwnd",
            "--process",
            "--title",
            "--island",
            "--monitor",
            "--offsets",
            "--delay-ms",
            "--game-resolution",
            "--observe-ms",
            "--expected-revision",
        ]);
        let value = prop::sample::select(vec![
            "1",
            "-1",
            "0",
            "abc",
            "99999999999999999999",
            "a/b",
            "primary",
            r"\\.\DISPLAY1",
            "1,2,3,4",
            "1,2,3",
            "60001",
            "1920x1080",
            "12x",
            "15000",
            "",
            "--keep",
            "-",
            "x.json",
        ]);
        let bool_flag = prop::sample::select(vec![
            "--include-hidden",
            "--only-managed",
            "--no-profile",
            "--keep",
            "--pretty",
            "--bogus",
            "-",
            "x.json",
        ]);
        prop_oneof![
            (value_flag, value).prop_map(|(f, v)| vec![f.to_string(), v.to_string()]),
            bool_flag.prop_map(|f| vec![f.to_string()]),
        ]
    }

    /// Well-formed groups for one subcommand plus the same groups reordered, so a reorder never
    /// splits a flag from its value.
    #[allow(clippy::type_complexity)]
    fn valid_groups() -> impl Strategy<Value = (usize, Vec<Vec<String>>, Vec<Vec<String>>)> {
        let g = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        prop_oneof![
            Just((
                0usize,
                vec![
                    g(&["--process", "g.exe"]),
                    g(&["--include-hidden"]),
                    g(&["--only-managed"]),
                    g(&["--pretty"])
                ]
            )),
            Just((
                1,
                vec![
                    g(&["--hwnd", "12"]),
                    g(&["--island", "a/b"]),
                    g(&["--monitor", "primary"]),
                    g(&["--offsets", "1,2,3,4"]),
                    g(&["--delay-ms", "5"]),
                    g(&["--no-profile"]),
                    g(&["--pretty"])
                ]
            )),
            Just((
                1,
                vec![
                    g(&["--process", "g.exe"]),
                    g(&["--title", "T"]),
                    g(&["--monitor", "2"])
                ]
            )),
            Just((
                2,
                vec![
                    g(&["--hwnd", "3"]),
                    g(&["--island", "a/b"]),
                    g(&["--game-resolution", "800x600"])
                ]
            )),
            Just((
                8,
                vec![
                    g(&["--process", "g.exe"]),
                    g(&["--title", "T"]),
                    g(&["--island", "a/b"]),
                    g(&["--observe-ms", "10"]),
                    g(&["--keep"]),
                    g(&["--pretty"])
                ]
            )),
            Just((
                7,
                vec![
                    g(&["x.json"]),
                    g(&["--expected-revision", "r1"]),
                    g(&["--pretty"])
                ]
            )),
            Just((1, vec![g(&["--hwnd", "abc"]), g(&["--delay-ms", "x"])])),
            Just((1, vec![g(&["--hwnd", "1"]), g(&["--process", "p"])])),
        ]
        .prop_flat_map(|(cmd, groups)| {
            let n = groups.len();
            (Just(cmd), prop::sample::subsequence(groups, 0..=n))
        })
        .prop_flat_map(|(cmd, groups)| {
            (Just(cmd), Just(groups.clone()), Just(groups).prop_shuffle())
        })
    }

    fn with_body(a: &[String]) -> Result<Parsed, String> {
        parse(a, &mut |_| Ok(br#"{"remove":["a.exe"]}"#.to_vec()))
    }

    proptest! {
        #![proptest_config(cfg())]

        #[test]
        fn every_ok_parse_is_accepted_by_the_resident(
            cmd in prop::sample::select(SUBCOMMANDS.to_vec()),
            groups in prop::collection::vec(token_group(), 0..6),
        ) {
            let mut a: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
            a.extend(groups.into_iter().flatten());
            if let Ok(Parsed::Call(i)) = with_body(&a) {
                prop_assert!(resident_accepts(i.method, i.params.clone()), "{a:?} → {i:?}");
            }
        }

        #[test]
        fn flag_order_does_not_change_the_result((cmd, groups, shuffled) in valid_groups()) {
            let build = |gs: &[Vec<String>]| {
                let mut a: Vec<String> = SUBCOMMANDS[cmd].iter().map(|s| s.to_string()).collect();
                a.extend(gs.iter().flatten().cloned());
                a
            };
            prop_assert_eq!(with_body(&build(&groups)), with_body(&build(&shuffled)));
        }
    }
}
