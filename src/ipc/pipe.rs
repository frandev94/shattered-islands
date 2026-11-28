//! Named-pipe transport: per-user pipe, DACL limited to the current user's SID, remote
//! clients rejected. One JSON line per request/response (see [`super`]).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, PipeMode, ServerOptions};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{Notify, oneshot};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::{PCWSTR, PWSTR};

use super::{
    CallError, ErrorBody, ErrorCode, Id, IpcError, MAX_DELAY_MS, MAX_LINE, MAX_OBSERVE_MS,
    PENDING_GRACE_MS, Request, Response, decode_request, encode,
};

const ERROR_PIPE_BUSY: i32 = 231;
const SDDL_REVISION_1: u32 = 1;

/// Turns a request into a pending response; the resident process answers from its main thread.
pub type Handler = Arc<dyn Fn(Request) -> oneshot::Receiver<Response> + Send + Sync>;

/// Receives a message when the pipe server stops unexpectedly.
pub type ErrorSink = Arc<dyn Fn(String) + Send + Sync>;

/// Only the HRESULT goes into `message`: the OS description is localized.
fn win32_err(call: &'static str) -> impl FnOnce(windows::core::Error) -> IpcError {
    move |e| IpcError::Win32 {
        call,
        message: format!("HRESULT {:#010x}", e.code().0 as u32),
    }
}

/// String form (`S-1-5-21-…`) of the current process token's user SID.
fn user_sid_string() -> Result<String, IpcError> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; the handle is closed below on every path.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(win32_err("OpenProcessToken"))?;
    let result = sid_string_of_token(token);
    // SAFETY: `token` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    result
}

fn sid_string_of_token(token: HANDLE) -> Result<String, IpcError> {
    let mut len = 0u32;
    // SAFETY: size query; a null buffer with length 0 is the documented way to get the needed size.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    if len == 0 {
        return Err(IpcError::Win32 {
            call: "GetTokenInformation",
            message: "unknown TOKEN_USER size".into(),
        });
    }
    // u64 elements keep the buffer 8-byte aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .map_err(win32_err("GetTokenInformation"))?;
    // SAFETY: the buffer was just filled with a TOKEN_USER (aligned, see above).
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut s = PWSTR::null();
    // SAFETY: `sid` points into `buf`, alive here; `s` receives a LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut s) }.map_err(win32_err("ConvertSidToStringSidW"))?;
    // SAFETY: `s` is a valid NUL-terminated wide string returned by the call above.
    let out = unsafe { s.to_string() };
    // SAFETY: `s` was allocated with LocalAlloc by ConvertSidToStringSidW.
    unsafe { LocalFree(Some(HLOCAL(s.0.cast()))) };
    out.map_err(|e| IpcError::Win32 {
        call: "ConvertSidToStringSidW",
        message: e.to_string(),
    })
}

/// `BORDERLESS_SHATTER_INSTANCE` is set but is not `[A-Za-z0-9-]{1,32}`. Never treated as unset:
/// that would silently share the real instance's pipe, mutex and state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "BORDERLESS_SHATTER_INSTANCE {0:?} is invalid: use 1 to 32 characters from A-Z, a-z, 0-9 and -"
)]
pub struct InvalidInstance(pub String);

/// Validates an instance suffix: unset or empty = `None`, `[A-Za-z0-9-]{1,32}` = `Some`.
pub fn parse_instance(raw: Option<&str>) -> Result<Option<String>, InvalidInstance> {
    match raw {
        None | Some("") => Ok(None),
        Some(s) if s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => {
            Ok(Some(s.to_owned()))
        }
        Some(s) => Err(InvalidInstance(s.to_owned())),
    }
}

/// Suffix from `BORDERLESS_SHATTER_INSTANCE` (isolated instances for tests).
pub fn instance_suffix() -> Result<Option<String>, InvalidInstance> {
    // `var_os`: a non-Unicode value is set-but-invalid too (lossy text keeps it reportable).
    let raw = std::env::var_os("BORDERLESS_SHATTER_INSTANCE");
    parse_instance(raw.as_ref().map(|v| v.to_string_lossy()).as_deref())
}

/// `\\.\pipe\BorderlessShatter-<user SID>[-<suffix>]`: one pipe per user (the pipe namespace is
/// machine-wide). An invalid suffix is an error.
pub fn pipe_name_for(suffix: Option<&str>) -> Result<String, IpcError> {
    let suffix = parse_instance(suffix)?;
    let mut n = format!(r"\\.\pipe\BorderlessShatter-{}", user_sid_string()?);
    if let Some(s) = suffix {
        n.push('-');
        n.push_str(&s);
    }
    Ok(n)
}

/// Name of this user's engine pipe (per-user SID, plus the instance suffix when set).
pub fn pipe_name() -> Result<String, IpcError> {
    pipe_name_for(instance_suffix()?.as_deref())
}

/// Security descriptor granting full access to the current user only (protected DACL).
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn current_user_only() -> Result<Self, IpcError> {
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", user_sid_string()?)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated and outlives the call; `sd` is a valid out pointer
        // whose LocalAlloc'd result is freed in `Drop`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(win32_err(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ))?;
        Ok(Self(sd))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW with LocalAlloc.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

// ------------------------------------------------------------- line reading

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Eof,
    Line,
    TooLong,
}

/// Read one `\n`-terminated line into `buf` (cleared first), never buffering more than
/// `MAX_LINE + 1` bytes. An over-long line is drained up to its newline so the stream stays in sync.
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<LineRead> {
    buf.clear();
    let n = (&mut *r)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if buf.last() == Some(&b'\n') || n <= MAX_LINE {
        return Ok(LineRead::Line);
    }
    buf.clear();
    let mut scratch = Vec::new();
    loop {
        scratch.clear();
        let n = (&mut *r)
            .take(64 * 1024)
            .read_until(b'\n', &mut scratch)
            .await?;
        if n == 0 || scratch.last() == Some(&b'\n') {
            return Ok(LineRead::TooLong);
        }
    }
}

// ------------------------------------------------------------------- server

fn salvage_id(line: &[u8]) -> Id {
    #[derive(serde::Deserialize)]
    struct Envelope {
        id: Id,
    }
    // Objects only: the derived decoder would read `[5]` as id 5. A repeated `id` fails as
    // "duplicate field" and falls back to 0 rather than picking one.
    if line.trim_ascii_start().first() != Some(&b'{') {
        return Id::Num(0);
    }
    serde_json::from_slice::<Envelope>(line).map_or(Id::Num(0), |e| e.id)
}

fn encode_or_internal(resp: Response) -> String {
    encode(&resp).unwrap_or_else(|_| {
        let e = ErrorBody::new(
            ErrorCode::Internal,
            "the reply exceeds the maximum of 1 MiB",
        );
        encode(&Response::err(resp.id, e)).unwrap_or_default()
    })
}

async fn dispatch(handler: &Handler, line: &[u8]) -> Response {
    let Ok(text) = std::str::from_utf8(line) else {
        return Response::err(
            Id::Num(0),
            ErrorBody::new(ErrorCode::InvalidJson, "the request is not valid UTF-8"),
        );
    };
    match decode_request(text) {
        Err(e) => Response::err(salvage_id(line), e),
        Ok(req) => {
            let id = req.id.clone();
            match handler(req).await {
                Ok(resp) => resp,
                Err(_) => Response::err(
                    id,
                    ErrorBody::new(ErrorCode::Internal, "the engine did not answer"),
                ),
            }
        }
    }
}

/// Serve one connection: requests are answered in order, one at a time.
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(stream: S, handler: Handler) {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let mut buf = Vec::new();
    loop {
        let resp = match read_line(&mut rd, &mut buf).await {
            Ok(LineRead::Eof) | Err(_) => return,
            Ok(LineRead::TooLong) => Response::err(
                Id::Num(0),
                ErrorBody::new(
                    ErrorCode::InvalidJson,
                    "the line exceeds the maximum of 1 MiB",
                ),
            ),
            Ok(LineRead::Line) => dispatch(&handler, &buf).await,
        };
        let line = encode_or_internal(resp);
        if wr.write_all(line.as_bytes()).await.is_err() || wr.flush().await.is_err() {
            return;
        }
    }
}

async fn accept_loop(
    name: String,
    handler: Handler,
    stop: Arc<Notify>,
    ready: mpsc::Sender<Result<(), IpcError>>,
    on_error: Option<ErrorSink>,
) {
    let sd = match SecurityDescriptor::current_user_only() {
        Ok(sd) => sd,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut attrs = sd.attributes();
    let create = |first: bool, attrs: &mut SECURITY_ATTRIBUTES| {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first)
            .reject_remote_clients(true)
            .pipe_mode(PipeMode::Byte);
        // SAFETY: `attrs` points to a SECURITY_ATTRIBUTES whose descriptor (`sd`) outlives this loop;
        // the kernel copies it during the call.
        unsafe {
            opts.create_with_security_attributes_raw(
                &name,
                (attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    };
    // `first_pipe_instance` makes a pre-existing pipe of the same name (squatting) a hard error.
    let mut server = match create(true, &mut attrs) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    loop {
        tokio::select! {
            r = server.connect() => if r.is_err() {
                // The instance is unusable (e.g. the client vanished mid-connect): replace it and
                // back off, or this loop would spin on the same failing instance.
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Ok(fresh) = create(false, &mut attrs) {
                    server = fresh;
                }
                continue
            },
            _ = stop.notified() => return,
        }
        // Create the next instance before serving this one so clients never see a missing pipe.
        // Retry with back-off; if it keeps failing, report and stop rather than die silently.
        let mut next = None;
        let mut last_err = String::new();
        for attempt in 0..5u64 {
            match create(false, &mut attrs) {
                Ok(n) => {
                    next = Some(n);
                    break;
                }
                Err(e) => {
                    last_err = crate::io_text(&e);
                    tokio::time::sleep(Duration::from_millis(50 << attempt)).await;
                }
            }
        }
        let Some(next) = next else {
            if let Some(cb) = &on_error {
                cb(format!(
                    "could not create the next pipe instance: {last_err}"
                ));
            }
            return;
        };
        tokio::spawn(serve(std::mem::replace(&mut server, next), handler.clone()));
    }
}

/// Handle of the pipe server thread.
pub struct ServerHandle {
    stop: Arc<Notify>,
    thread: JoinHandle<()>,
}

impl ServerHandle {
    /// Stop accepting, drop open connections and join the server thread.
    pub fn stop(self) {
        self.stop.notify_one();
        let _ = self.thread.join();
    }
}

/// Start the pipe server on its own thread (current-thread runtime). Returns once the first
/// instance exists, or with the error that prevented creating it.
pub fn spawn_server(handler: Handler) -> Result<ServerHandle, IpcError> {
    spawn_server_with_status(handler, None)
}

/// Like [`spawn_server`], but `on_error` is told if the accept loop dies later on.
pub fn spawn_server_with_status(
    handler: Handler,
    on_error: Option<ErrorSink>,
) -> Result<ServerHandle, IpcError> {
    let name = pipe_name()?;
    let stop = Arc::new(Notify::new());
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("ipc-pipe".into())
        .spawn(
            move || match Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(accept_loop(name, handler, thread_stop, ready_tx, on_error)),
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                }
            },
        )?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ServerHandle { stop, thread }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(IpcError::Closed)
        }
    }
}

// ------------------------------------------------------------------- client

/// Blocking client: owns a small runtime; do not call from inside another tokio runtime
/// (use `spawn_blocking`).
pub struct Client {
    rt: Runtime,
    io: BufReader<NamedPipeClient>,
    next_id: i64,
    read_timeout: Duration,
}

/// Longest valid reply: an `apply` with the largest delay answers up to `MAX_DELAY_MS +
/// PENDING_GRACE_MS` after the request, a `diagnose` just past `MAX_OBSERVE_MS`.
const LONGEST_REPLY_MS: u64 = if MAX_DELAY_MS + PENDING_GRACE_MS > MAX_OBSERVE_MS {
    MAX_DELAY_MS + PENDING_GRACE_MS
} else {
    MAX_OBSERVE_MS
};
/// Longest wait for one reply: the longest valid one plus slack for a busy resident. It only
/// bounds a resident stalled in a modal tray menu or hung.
const READ_TIMEOUT: Duration = Duration::from_millis(LONGEST_REPLY_MS + 6_000);

fn retryable(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_PIPE_BUSY)
}
/// Refuse a pipe whose server process runs as another user (a squatting server).
fn verify_server_is_us(pipe: &NamedPipeClient) -> Result<(), IpcError> {
    let mut pid = 0u32;
    // SAFETY: the handle belongs to the live client `pipe`; `pid` is a valid out pointer.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .map_err(win32_err("GetNamedPipeServerProcessId"))?;
    // SAFETY: plain handle-returning call; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(win32_err("OpenProcess"))?;
    let mut token = HANDLE::default();
    // SAFETY: `process` is open; `token` is a valid out pointer closed below.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    // SAFETY: `process` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(process) };
    opened.map_err(win32_err("OpenProcessToken"))?;
    let theirs = sid_string_of_token(token);
    // SAFETY: `token` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    if theirs? == user_sid_string()? {
        Ok(())
    } else {
        Err(IpcError::ForeignServer(pid))
    }
}

impl Client {
    /// Connect, retrying while the pipe does not exist yet or is busy, until `timeout` elapses.
    pub fn connect(timeout: Duration) -> Result<Client, IpcError> {
        Self::connect_named(&pipe_name()?, timeout)
    }

    /// Like [`Client::connect`] against an explicit pipe name (server identity still verified).
    pub fn connect_named(name: &str, timeout: Duration) -> Result<Client, IpcError> {
        let rt = Builder::new_current_thread().enable_all().build()?;
        let deadline = Instant::now() + timeout;
        let pipe = rt.block_on(async {
            loop {
                match ClientOptions::new().open(name) {
                    Ok(c) => return Ok(c),
                    Err(e) if retryable(&e) => {
                        let absent = e.kind() == io::ErrorKind::NotFound;
                        if Instant::now() >= deadline {
                            return Err(if absent {
                                IpcError::NotRunning
                            } else {
                                IpcError::Connect("the pipe stayed busy".into())
                            });
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(e) => return Err(IpcError::Io(e)),
                }
            }
        })?;
        verify_server_is_us(&pipe)?;
        Ok(Client {
            rt,
            io: BufReader::new(pipe),
            next_id: 1,
            read_timeout: READ_TIMEOUT,
        })
    }

    /// Replace the per-call reply deadline (tests).
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    /// Send one request and wait for its response: `Ok(result)` or `Err(Remote(error))`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode(&Request {
            id: Id::Num(id),
            method: method.into(),
            params,
        })?;
        let Client {
            rt,
            io,
            read_timeout,
            ..
        } = self;
        let exchange = async {
            io.get_mut().write_all(line.as_bytes()).await?;
            io.get_mut().flush().await?;
            let mut buf = Vec::new();
            match read_line(io, &mut buf).await? {
                LineRead::Eof => Err(IpcError::Closed),
                LineRead::TooLong => Err(IpcError::LineTooLong),
                LineRead::Line => Ok(serde_json::from_slice(&buf)?),
            }
        };
        let resp: Response = rt
            .block_on(async { tokio::time::timeout(*read_timeout, exchange).await })
            .unwrap_or(Err(IpcError::Timeout))
            .map_err(CallError::Ipc)?;
        if resp.id != Id::Num(id) {
            return Err(IpcError::Protocol(format!("id {:?} instead of {id}", resp.id)).into());
        }
        match resp.error {
            Some(e) => Err(CallError::Remote(e)),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn echo_handler() -> Handler {
        Arc::new(|req| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Response::ok(
                req.id,
                json!({"method": req.method, "params": req.params}),
            ));
            rx
        })
    }

    fn rt() -> Runtime {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn read_line_limits_and_resyncs() {
        rt().block_on(async {
            let mut data = vec![b'a'; MAX_LINE + 10];
            data.extend_from_slice(b"\nok\n");
            let mut r = BufReader::new(&data[..]);
            let mut buf = Vec::new();
            assert_eq!(
                read_line(&mut r, &mut buf).await.expect("io"),
                LineRead::TooLong
            );
            assert_eq!(
                read_line(&mut r, &mut buf).await.expect("io"),
                LineRead::Line
            );
            assert_eq!(buf, b"ok\n");
            assert_eq!(
                read_line(&mut r, &mut buf).await.expect("io"),
                LineRead::Eof
            );
            // exactly MAX_LINE bytes of body is still accepted
            let mut data = vec![b'a'; MAX_LINE];
            data.push(b'\n');
            let mut r = BufReader::new(&data[..]);
            assert_eq!(
                read_line(&mut r, &mut buf).await.expect("io"),
                LineRead::Line
            );
        });
    }

    #[test]
    fn serve_answers_in_order_and_reports_bad_lines() {
        rt().block_on(async {
            let (client, server) = tokio::io::duplex(4 << 20);
            let task = tokio::spawn(serve(server, echo_handler()));
            let (mut rd, mut wr) = tokio::io::split(client);
            let mut input = String::new();
            input.push_str("{\"id\":1,\"method\":\"hello\",\"params\":{\"a\":1}}\n");
            input.push_str("{\"id\":\"s\",\"method\":\"x\"}\n");
            input.push_str("not json\n");
            input.push_str("{\"id\":9}\n");
            input.push_str(&"a".repeat(MAX_LINE + 5));
            input.push('\n');
            input.push_str("{\"id\":3,\"id\":4,\"method\":\"m\"}\n");
            input.push_str("{\"id\":8,\"method\":\"m\",\"params\":{\"a\":1,\"a\":2}}\n");
            input.push_str("{\"id\":2,\"method\":\"last\"}\n");
            wr.write_all(input.as_bytes()).await.expect("write");
            wr.shutdown().await.expect("shutdown");
            let mut out = String::new();
            rd.read_to_string(&mut out).await.expect("read");
            task.await.expect("serve");
            let v: Vec<Value> = out
                .lines()
                .map(|l| serde_json::from_str(l).expect("json"))
                .collect();
            assert_eq!(v.len(), 8);
            assert_eq!(
                v[0],
                json!({"id": 1, "result": {"method": "hello", "params": {"a": 1}}})
            );
            assert_eq!(
                v[1],
                json!({"id": "s", "result": {"method": "x", "params": null}})
            );
            assert_eq!(v[2]["error"]["code"], "invalid_json");
            assert_eq!(v[3]["id"], 9); // id salvaged from a malformed request
            assert_eq!(v[3]["error"]["code"], "invalid_params");
            assert_eq!(v[4]["error"]["code"], "invalid_json");
            assert_eq!(v[5]["id"], 0); // a repeated id is not salvaged
            assert_eq!(v[5]["error"]["code"], "invalid_params");
            assert_eq!(v[6]["id"], 8); // a nested repeated key keeps the request id
            assert_eq!(v[6]["error"]["code"], "invalid_params");
            assert_eq!(v[7]["id"], 2);
        });
    }

    #[test]
    fn dropped_reply_is_internal_error() {
        rt().block_on(async {
            let handler: Handler = Arc::new(|_| oneshot::channel().1);
            let r = dispatch(&handler, br#"{"id":4,"method":"x"}"#).await;
            assert_eq!(r.id, Id::Num(4));
            assert_eq!(r.error.expect("error").code, ErrorCode::Internal);
        });
    }

    #[test]
    fn pipe_name_is_per_user() {
        let n = pipe_name_for(None).expect("name");
        assert!(n.starts_with(r"\\.\pipe\BorderlessShatter-S-1-"), "{n}");
        assert!(!n.ends_with('-'), "{n}");
    }

    /// A pipe server that accepts one client and then never answers; stops when `stop` fires.
    fn silent_server(name: &str) -> (mpsc::Sender<()>, JoinHandle<()>) {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let name = name.to_owned();
        let t = std::thread::spawn(move || {
            rt().block_on(async {
                let server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&name)
                    .expect("server");
                ready_tx.send(()).expect("ready");
                server.connect().await.expect("connect");
                while stop_rx.try_recv().is_err() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
        });
        ready_rx.recv().expect("ready");
        (stop_tx, t)
    }

    #[test]
    fn call_times_out_when_the_server_never_answers() {
        let name = format!(r"\\.\pipe\bs-test-timeout-{}", std::process::id());
        let (stop, t) = silent_server(&name);
        let mut c = Client::connect_named(&name, Duration::from_secs(2)).expect("connect");
        c.set_read_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let e = c.call("hello", Value::Null).expect_err("must time out");
        assert!(matches!(e, CallError::Ipc(IpcError::Timeout)), "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = stop.send(());
        t.join().expect("server thread");
    }

    #[test]
    fn absent_pipe_is_not_running() {
        let name = format!(r"\\.\pipe\bs-test-absent-{}", std::process::id());
        let e = Client::connect_named(&name, Duration::ZERO)
            .err()
            .expect("no server");
        assert!(matches!(e, IpcError::NotRunning), "{e:?}");
    }

    #[test]
    fn read_timeout_outlasts_every_valid_reply() {
        // An `apply` with the largest valid delay replies up to delay + grace after the request.
        assert!(READ_TIMEOUT > Duration::from_millis(MAX_DELAY_MS + PENDING_GRACE_MS));
        assert!(READ_TIMEOUT > Duration::from_millis(MAX_OBSERVE_MS));
    }

    #[test]
    fn pipe_errors_never_carry_localized_os_text() {
        // Windows localizes the message of an OS error; only its code may reach CLI/MCP text.
        let io = IpcError::Io(io::Error::from_raw_os_error(5)).to_string();
        assert_eq!(io, "I/O error on the pipe: os error 5");
        let hresult = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(5));
        let w = win32_err("OpenProcess")(hresult).to_string();
        assert_eq!(w, "Win32 call failed (OpenProcess): HRESULT 0x80070005");
    }

    #[test]
    fn parse_instance_validation() {
        assert_eq!(parse_instance(None), Ok(None));
        assert_eq!(parse_instance(Some("")), Ok(None), "empty = unset");
        assert_eq!(parse_instance(Some("Abc-123")), Ok(Some("Abc-123".into())));
        let max = "a".repeat(32);
        assert_eq!(parse_instance(Some(&max)), Ok(Some(max.clone())));
        let long = "a".repeat(33);
        for bad in [
            long.as_str(),
            "a b",
            "a.b",
            "a_b",
            "a\\b",
            "a/b",
            "ñ",
            "日本",
            "a\n",
            "a\0",
            " ",
        ] {
            // Set but invalid must never fall back to the real instance's pipe, mutex and state.
            assert_eq!(
                parse_instance(Some(bad)),
                Err(InvalidInstance(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn pipe_name_refuses_an_invalid_suffix() {
        assert!(matches!(
            pipe_name_for(Some("a b")),
            Err(IpcError::InvalidInstance(InvalidInstance(s))) if s == "a b"
        ));
        let real = pipe_name_for(None).expect("name");
        assert_eq!(pipe_name_for(Some("")).expect("name"), real);
        assert_eq!(
            pipe_name_for(Some("x-1")).expect("name"),
            format!("{real}-x-1")
        );
    }

    /// Needs a free pipe name (no resident instance running): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn end_to_end_echo_over_real_pipe() {
        let server = spawn_server(echo_handler()).expect("server");
        let mut c = Client::connect(Duration::from_secs(2)).expect("connect");
        let r = c.call("hello", json!({"x": 1})).expect("call");
        assert_eq!(r, json!({"method": "hello", "params": {"x": 1}}));
        let r = c.call("again", Value::Null).expect("call");
        assert_eq!(r["method"], "again");
        // a second pipe with the same name cannot be created while the first exists
        assert!(spawn_server(echo_handler()).is_err());
        drop(c);
        server.stop();
    }

    /// The panel's checked connect sends nothing but `hello` to a resident on another protocol.
    /// Needs a free pipe name (no resident instance running).
    #[test]
    #[ignore]
    fn checked_connect_refuses_a_mismatched_resident_before_any_request() {
        use crate::gui::{Failure, connect_checked};
        use crate::ipc::PROTOCOL;
        use std::sync::Mutex;

        let protocol = Arc::new(Mutex::new(PROTOCOL + 1));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (p, s) = (protocol.clone(), seen.clone());
        let server = spawn_server(Arc::new(move |req| {
            let (tx, rx) = oneshot::channel();
            let n = *p.lock().expect("protocol");
            s.lock().expect("seen").push(req.method.clone());
            let _ = tx.send(Response::ok(req.id, json!({"protocol": n})));
            rx
        }))
        .expect("server");
        assert_eq!(
            connect_checked().err(),
            Some(Failure::Protocol(PROTOCOL + 1))
        );
        assert_eq!(*seen.lock().expect("seen"), ["hello"]);
        *protocol.lock().expect("protocol") = PROTOCOL;
        assert!(connect_checked().is_ok());
        server.stop();
    }
}
