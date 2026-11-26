//! Process-level tests of `borderless-shatter mcp` over stdio. Run the `#[ignore]` ones with
//! `--ignored --test-threads=1` after `cargo build --examples`.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use common::{FRAME_BITS, Harness, hello, island_rect, snap};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);
const TOOLS: [&str; 9] = [
    "apply",
    "diagnose",
    "get_profiles",
    "list_islands",
    "list_windows",
    "place",
    "restore",
    "restore_all",
    "set_profiles",
];

/// A running `mcp` process: JSON-RPC lines in, stdout lines out (every one kept for the hygiene
/// check).
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    seen: Vec<String>,
    next_id: u64,
}

impl Mcp {
    fn spawn(h: &Harness) -> Self {
        let mut child = h
            .command(&["mcp"])
            .env("BORDERLESS_SHATTER_LANG", "en")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mcp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            seen: Vec::new(),
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").expect("write to mcp");
        stdin.flush().expect("flush");
    }

    /// Sends a request and returns the response with the same id.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(TIMEOUT)
                .unwrap_or_else(|e| panic!("no response to {method}: {e}"));
            self.seen.push(line.clone());
            let v: Value = serde_json::from_str(&line).expect("stdout line is JSON");
            if v["id"] == id {
                return v;
            }
        }
    }

    fn initialize(&mut self) {
        let r = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "e2e", "version": "0"}}),
        );
        assert_eq!(r["result"]["serverInfo"]["name"], "borderless-shatter");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn tools_list(&mut self) -> Vec<Value> {
        let r = self.request("tools/list", json!({}));
        r["result"]["tools"].as_array().expect("tools").clone()
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))
    }

    /// Closes stdin, expects stdout EOF and a clean exit within 5 s, and checks that every
    /// stdout line was a JSON-RPC message.
    fn finish(mut self) {
        self.stdin = None;
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            match self
                .lines
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("stdout did not reach EOF within 5 s"),
            }
        }
        assert!(
            common::poll(Duration::from_secs(5), || self
                .child
                .try_wait()
                .ok()
                .flatten()
                .is_some()),
            "mcp did not exit after stdin closed"
        );
        let status = self.child.try_wait().expect("try_wait").expect("status");
        assert!(status.success(), "mcp exit status {status}");
        for line in &self.seen {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
            assert_eq!(v["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        // No-op after `finish`; on a failed test it keeps the process from outliving us.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_error(r: &Value) -> bool {
    r["result"]["isError"] == true
}

fn text(r: &Value) -> String {
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn mcp_lists_tools_without_resident() {
    let h = Harness::new("mcp_lists_tools_without_resident");
    let mut m = Mcp::spawn(&h);
    m.initialize();
    let tools = m.tools_list();

    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, TOOLS);
    for t in &tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        let d = t["description"].as_str().unwrap_or_default();
        assert!(!d.is_empty(), "{} has no description", t["name"]);
        assert!(
            !d.chars().any(
                |c| "\u{e1}\u{e9}\u{ed}\u{f3}\u{fa}\u{f1}\u{bf}\u{a1}\u{ab}\u{bb}".contains(c)
            ),
            "description of {} is not English: {d}",
            t["name"]
        );
    }

    m.finish();
    assert!(
        !h.pipe_reachable(Duration::ZERO),
        "tools/list must not start the resident"
    );
}

#[test]
#[ignore = "opens a window and a resident; run with --ignored --test-threads=1"]
fn mcp_place_and_restore_testwin() {
    let mut h = Harness::new("mcp_place_and_restore_testwin");
    let mut c = h.start_resident();
    let w = h.testwin(&["--preset", "notepad"]);
    let right = island_rect(&mut c, "halves-16-9", "right");
    let mut m = Mcp::spawn(&h);
    m.initialize();
    assert_eq!(m.tools_list().len(), TOOLS.len());

    let r = m.call("list_islands", json!({}));
    assert!(!is_error(&r), "{r}");
    let layouts = r["result"]["structuredContent"]["layouts"]
        .as_array()
        .expect("layouts");
    let halves = layouts
        .iter()
        .find(|l| l["id"] == "halves-16-9")
        .expect("halves-16-9 layout");
    assert!(
        halves["islands"]
            .as_array()
            .is_some_and(|is| is.iter().any(|i| i["id"] == "right"))
    );

    let r = m.call(
        "place",
        json!({"window": {"hwnd": w.hwnd}, "island": "halves-16-9/right"}),
    );
    assert!(!is_error(&r), "{r}");
    assert_eq!(r["result"]["structuredContent"]["applied"], true);
    let s = snap(w.hwnd);
    assert_eq!(s.rect, right);
    assert_eq!(s.style & FRAME_BITS, 0, "frame removed");

    let r = m.call("restore", json!({"window": {"hwnd": w.hwnd}}));
    assert!(!is_error(&r), "{r}");
    assert_eq!(r["result"]["structuredContent"]["restored"], true);
    assert_eq!(r["result"]["structuredContent"]["exact"], true);
    assert_eq!(snap(w.hwnd), w.orig);

    // legacy alias test: the pre-rename ref reaches the same island
    let r = m.call(
        "place",
        json!({"window": {"hwnd": w.hwnd}, "island": "mitades-16-9/derecha"}), // legacy alias test
    );
    assert!(!is_error(&r), "{r}");
    assert_eq!(snap(w.hwnd).rect, right);
    let r = m.call("restore", json!({"window": {"hwnd": w.hwnd}}));
    assert!(!is_error(&r), "{r}");
    assert_eq!(snap(w.hwnd), w.orig);

    let r = m.call("restore", json!({"window": {"hwnd": 1}}));
    assert!(is_error(&r), "{r}");
    assert!(text(&r).contains("window_not_found"), "{}", text(&r));

    // W1: malformed arguments are a tool error, not JSON-RPC -32602
    let r = m.call("place", json!({"window": {}}));
    assert!(r["error"].is_null(), "{r}");
    assert!(is_error(&r), "{r}");
    assert!(
        text(&r).contains("failed to deserialize parameters"),
        "{}",
        text(&r)
    );

    // an unknown tool is still a protocol error
    let r = m.call("nope", json!({}));
    assert_eq!(r["error"]["code"], -32602, "{r}");

    m.finish();
}

#[test]
#[ignore = "starts a resident; run with --ignored --test-threads=1"]
fn mcp_starts_resident_when_absent() {
    let h = Harness::new("mcp_starts_resident_when_absent");
    let mut m = Mcp::spawn(&h);
    m.initialize();
    m.tools_list();
    assert!(
        !h.pipe_reachable(Duration::ZERO),
        "initialize and tools/list must not start the resident"
    );

    let r = m.call("list_islands", json!({}));
    assert!(!is_error(&r), "{r}");
    assert_eq!(r["result"]["_meta"]["started_resident"], true, "{r}");

    let mut c = h.connect();
    h.track_pid(hello(&mut c).pid);
    drop(c);

    // The detached resident must not hold the client's pipes open.
    m.finish();
    assert!(
        h.pipe_reachable(Duration::from_secs(2)),
        "the resident must outlive the mcp process"
    );
}
