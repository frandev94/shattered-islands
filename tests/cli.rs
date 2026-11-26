//! Process-level tests of the CLI subcommands (`borderless-shatter <subcommand>`): exit codes and
//! output against an isolated resident. Run the `#[ignore]` ones with `--ignored --test-threads=1`
//! after `cargo build --examples`.

mod common;

use std::process::{Command, Output};
use std::time::Duration;

use borderless_shatter::ipc::{ApplyResult, ListIslandsResult, RestoreResult};
use common::{FRAME_BITS, Harness, island_rect, snap};
use serde_json::Value;

/// Runs `cmd` to completion with English output.
fn run(mut cmd: Command) -> Output {
    cmd.env("BORDERLESS_SHATTER_LANG", "en")
        .output()
        .expect("run the CLI")
}

fn cli(h: &Harness, args: &[&str]) -> Output {
    run(h.command(args))
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("exit code")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn catalog_usage(json: &str) -> String {
    let v: Value = serde_json::from_str(json).expect("cli.json");
    v["cli.usage"].as_str().expect("cli.usage").to_owned()
}

#[test]
fn cli_exit_codes_without_resident() {
    let h = Harness::new("cli_exit_codes_without_resident");

    let o = cli(&h, &["list-islands"]);
    assert_eq!(code(&o), 3, "unreachable resident");
    assert!(!o.stderr.is_empty());
    assert!(o.stdout.is_empty());

    assert_eq!(code(&cli(&h, &["apply"])), 2, "apply without a window");
    assert_eq!(
        code(&cli(&h, &["place", "--hwnd", "1"])),
        2,
        "place without --island"
    );
    assert_eq!(code(&cli(&h, &["help"])), 0);

    assert!(
        !h.pipe_reachable(Duration::ZERO),
        "the CLI must never start the resident"
    );
}

#[test]
fn cli_help_follows_lang() {
    let h = Harness::new("cli_help_follows_lang");
    let mut texts = Vec::new();
    for (lang, catalog) in [
        ("en", include_str!("../locales/en/cli.json")),
        ("es", include_str!("../locales/es/cli.json")),
    ] {
        let mut cmd = h.command(&["help"]);
        let o = cmd
            .env("BORDERLESS_SHATTER_LANG", lang)
            .output()
            .expect("run help");
        assert_eq!(code(&o), 0, "{lang}");
        let text = out(&o);
        assert_eq!(text.trim_end(), catalog_usage(catalog).trim_end(), "{lang}");
        texts.push(text);
    }
    assert_ne!(texts[0], texts[1]);
    assert!(!h.pipe_reachable(Duration::ZERO), "help starts no resident");
}

#[test]
#[ignore = "opens a window and a resident; run with --ignored --test-threads=1"]
fn cli_place_and_restore_testwin() {
    let mut h = Harness::new("cli_place_and_restore_testwin");
    let mut c = h.start_resident();
    let w = h.testwin(&["--preset", "notepad"]);
    let hwnd = w.hwnd.to_string();
    let right = island_rect(&mut c, "halves-16-9", "right");

    let o = cli(&h, &["list-islands"]);
    assert_eq!(code(&o), 0);
    let compact: ListIslandsResult = serde_json::from_str(&out(&o)).expect("list-islands json");
    let o = cli(&h, &["list-islands", "--pretty"]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).trim().lines().count() > 1, "--pretty is multi-line");
    let pretty: ListIslandsResult = serde_json::from_str(&out(&o)).expect("pretty json");
    assert_eq!(compact, pretty);

    let o = cli(
        &h,
        &["place", "--hwnd", &hwnd, "--island", "halves-16-9/right"],
    );
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let r: ApplyResult = serde_json::from_str(&out(&o)).expect("apply json");
    assert!(r.applied);
    let s = snap(w.hwnd);
    assert_eq!(s.rect, right);
    assert_eq!(s.style & FRAME_BITS, 0, "frame removed");

    let o = cli(&h, &["restore", "--hwnd", &hwnd]);
    assert_eq!(code(&o), 0);
    let r: RestoreResult = serde_json::from_str(&out(&o)).expect("restore json");
    assert!(r.restored && r.exact);
    assert_eq!(snap(w.hwnd), w.orig);

    // legacy alias test: the pre-rename ref reaches the same island
    let o = cli(
        &h,
        &["place", "--hwnd", &hwnd, "--island", "mitades-16-9/derecha"], // legacy alias test
    );
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(snap(w.hwnd).rect, right);
    assert_eq!(code(&cli(&h, &["restore", "--hwnd", &hwnd])), 0);
    assert_eq!(snap(w.hwnd), w.orig);

    let o = cli(&h, &["restore", "--hwnd", "1"]);
    assert_eq!(code(&o), 1);
    let v: Value = serde_json::from_str(&out(&o)).expect("error json");
    assert_eq!(v["error"]["code"], "window_not_found");

    let o = cli(&h, &["place", "--hwnd", &hwnd, "--island", "nope/x"]);
    assert_eq!(code(&o), 1);
    let v: Value = serde_json::from_str(&out(&o)).expect("error json");
    assert_eq!(v["error"]["code"], "island_not_found");
}
