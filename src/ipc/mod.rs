//! IPC protocol: JSON-lines requests/responses and
//! the typed params/results of every method. Transport lives in [`pipe`].
//!
//! Every coordinate is in physical pixels. The same types back the MCP tool schemas
//! (`schemars`), so the pipe protocol and the MCP schemas cannot diverge.
//!
//! ```
//! use borderless_shatter::ipc::{decode_request, encode, ErrorCode, Id, Response};
//!
//! let req = decode_request(r#"{"id":1,"method":"hello"}"#).unwrap();
//! assert_eq!(req.id, Id::Num(1));
//! let line = encode(&Response::ok(req.id, serde_json::json!({"protocol": 2}))).unwrap();
//! assert_eq!(line, "{\"id\":1,\"result\":{\"protocol\":2}}\n");
//! // A request that is not an object is `invalid_params`, never a silent positional parse.
//! assert_eq!(decode_request("[1,\"hello\"]").unwrap_err().code, ErrorCode::InvalidParams);
//! // So is a repeated key: no "last one wins".
//! assert_eq!(decode_request(r#"{"id":1,"id":2,"method":"hello"}"#).unwrap_err().code, ErrorCode::InvalidParams);
//! ```

pub mod pipe;

use std::borrow::Cow;
use std::collections::HashSet;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diagnose::{Latencies, Recommendation, Verdict};
use crate::islands::{Layout, MonitorInfo, MonitorSel, Size};
use crate::profiles::{Profile, ProfileKey, Warning};
use crate::win32::DpiAwareness;
use crate::window_state::{Offsets, Rect};

/// Protocol revision returned by `hello`; clients must refuse a different one.
pub const PROTOCOL: u32 = 3;
/// The protocol a `hello` declares when it is not ours (`None`: same revision). The single
/// comparison CLI, MCP and the panel share; it reads only `protocol`, so another change to the
/// `hello` shape never reads as "not running".
pub fn protocol_mismatch(hello: &Value) -> Result<Option<u32>, IpcError> {
    let protocol = u32::deserialize(&hello["protocol"])?;
    Ok((protocol != PROTOCOL).then_some(protocol))
}

/// `hello` result → `Ok` only for our protocol revision (CLI and MCP).
pub fn check_hello(hello: Value) -> Result<(), CallError> {
    match protocol_mismatch(&hello)? {
        None => Ok(()),
        Some(protocol) => Err(IpcError::Protocol(format!(
            "the resident speaks protocol {protocol} and this client {PROTOCOL}"
        ))
        .into()),
    }
}

/// Maximum length of one JSON line (without the trailing `\n`).
pub const MAX_LINE: usize = 1 << 20;
/// Upper bound of `diagnose.observe_ms`.
pub const MAX_OBSERVE_MS: u64 = 15_000;
/// Largest `delay_ms` accepted from a request or a profile; keeps every deadline sum far from
/// overflow.
pub const MAX_DELAY_MS: u64 = 60_000;
/// Longest an `apply` reply waits beyond its delay (the guard normally ends sooner).
pub const PENDING_GRACE_MS: u64 = 4000;

/// Transport-level failure of the pipe or of the protocol framing.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("I/O error on the pipe: {}", crate::io_text(.0))]
    /// Reading or writing the pipe failed.
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    /// A line is not valid JSON.
    Json(#[from] serde_json::Error),
    #[error("the line exceeds the maximum of {MAX_LINE} bytes")]
    /// A line exceeds [`MAX_LINE`].
    LineTooLong,
    #[error("could not connect to Borderless Shatter: {0}")]
    /// The engine pipe could not be opened.
    Connect(String),
    #[error("Borderless Shatter is not running")]
    /// The engine pipe does not exist: no resident is running.
    NotRunning,
    #[error("the pipe server (pid {0}) does not run as the current user; connection refused")]
    /// The pipe is owned by a process of another user (a squatting server).
    ForeignServer(u32),
    #[error("Borderless Shatter did not answer in time")]
    /// No reply arrived within the client's read deadline.
    Timeout,
    #[error("the pipe closed without a reply")]
    /// The peer closed the pipe before replying.
    Closed,
    #[error("Win32 call failed ({call}): {message}")]
    /// A Win32 call failed.
    Win32 {
        /// Name of the failing Win32 function.
        call: &'static str,
        /// System error message.
        message: String,
    },
    #[error("unexpected reply from the server: {0}")]
    /// The reply violates the protocol (wrong id, wrong shape, ...).
    Protocol(String),
    #[error(transparent)]
    /// `BORDERLESS_SHATTER_INSTANCE` is set but invalid; no pipe name is derived from it.
    InvalidInstance(#[from] pipe::InvalidInstance),
}

/// Failure of [`pipe::Client::call`]: transport problem or an `error` response.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error(transparent)]
    /// Transport or framing problem.
    Ipc(#[from] IpcError),
    #[error("{}", .0.message)]
    /// The engine answered with an `error` response; match its `code`.
    Remote(ErrorBody),
}

// ---------------------------------------------------------------- envelope

/// Request id chosen by the client (integer or string); echoed unchanged in the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Id {
    /// Integer id.
    Num(i64),
    /// String id.
    Str(String),
}

/// One request line: `{"id", "method", "params"?}`. A missing `params` is `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// Correlation id echoed in the response.
    pub id: Id,
    /// Method name, e.g. `hello` or `apply`.
    pub method: String,
    #[serde(default)]
    /// Method parameters; `null` or absent means `{}`.
    pub params: Value,
}

/// Stable machine-readable error code (`snake_case` on the wire). Clients match the
/// code, never the message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request or its params are well-formed JSON but have the wrong shape or values,
    /// including a key repeated in one object and an array where the schema allows none.
    InvalidParams,
    /// The method name is not supported.
    UnknownMethod,
    /// The line is not valid JSON, or exceeds [`MAX_LINE`].
    InvalidJson,
    /// No window matches the [`WindowRef`].
    WindowNotFound,
    /// More than one window matches the [`WindowRef`].
    AmbiguousWindow,
    /// The window disappeared while the operation ran.
    WindowGone,
    /// The window belongs to a process the engine may not modify (e.g. elevated).
    AccessDenied,
    /// The process is on the exclusion list and is never managed.
    ExcludedProcess,
    /// The `layout/island` reference does not resolve.
    IslandNotFound,
    /// The requested monitor is not connected.
    MonitorUnavailable,
    /// Loading, validating or saving `profiles.json` failed.
    ProfileError,
    /// `expected_revision` no longer matches the file on disk.
    RevisionConflict,
    /// The profiles file has a newer schema version than this build supports.
    UnsupportedVersion,
    /// A Win32 call failed unexpectedly.
    Win32Error,
    /// The engine is busy with a conflicting operation; retry later.
    Busy,
    /// Unexpected engine failure.
    Internal,
}

/// Error payload of a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    /// Stable error code.
    pub code: ErrorCode,
    /// Human-readable English description; not part of the stable contract.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional structured context.
    pub data: Option<Value>,
}

impl ErrorBody {
    /// An error with `code` and `message` and no `data`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attaches structured context.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One response line: exactly one of `result` and `error` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    /// Id of the request being answered.
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Method result on success.
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Error on failure.
    pub error: Option<ErrorBody>,
}

impl Response {
    /// A successful response carrying `result`.
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response carrying `error`.
    pub fn err(id: Id, error: ErrorBody) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// One JSON line (with trailing `\n`). Fails if the body exceeds [`MAX_LINE`].
pub fn encode(msg: &impl Serialize) -> Result<String, IpcError> {
    let mut s = serde_json::to_string(msg)?;
    if s.len() > MAX_LINE {
        return Err(IpcError::LineTooLong);
    }
    s.push('\n');
    Ok(s)
}

/// Parse one request line. Syntax problems are `invalid_json`; a well-formed JSON
/// that is not a request (not an object, a key repeated in any object, missing `id`/`method`,
/// wrong types) is `invalid_params`.
pub fn decode_request(line: &str) -> Result<Request, ErrorBody> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() > MAX_LINE {
        return Err(ErrorBody::new(
            ErrorCode::InvalidJson,
            "the line exceeds the maximum of 1 MiB",
        ));
    }
    let error = |code, e: serde_json::Error| ErrorBody::new(code, format!("invalid request: {e}"));
    // Parse to `Value` first so syntax and depth errors are `invalid_json` whatever the shape.
    let value: Value = serde_json::from_str(line).map_err(|e| error(ErrorCode::InvalidJson, e))?;
    // `Value` keeps the last of repeated keys; RFC 8259 allows them, so they are a shape error.
    check_unique_keys(line.as_bytes()).map_err(|e| error(ErrorCode::InvalidParams, e))?;
    // Derived struct deserialisers also accept a positional array; requests are objects only.
    if value.is_array() {
        return Err(error(ErrorCode::InvalidParams, not_an_object()));
    }
    serde_json::from_value(value).map_err(|e| error(ErrorCode::InvalidParams, e))
}

/// Fails on a key repeated in any object of the JSON document (compared after unescaping).
pub fn check_unique_keys(bytes: &[u8]) -> Result<(), serde_json::Error> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    UniqueKeys::deserialize(&mut de).and_then(|_| de.end())
}

/// Deserialise a method's `params` (`null`/absent is treated as `{}`; unknown fields are ignored).
/// An array where `T`'s schema allows none (a positional struct) is `invalid_params`.
pub fn parse_params<T: DeserializeOwned + JsonSchema>(params: Value) -> Result<T, ErrorBody> {
    let invalid =
        |e: String| ErrorBody::new(ErrorCode::InvalidParams, format!("invalid params: {e}"));
    let params = match params {
        Value::Null => Value::Object(Default::default()),
        p => p,
    };
    check_arrays::<T>(&params).map_err(invalid)?;
    serde_json::from_value(params).map_err(|e| invalid(e.to_string()))
}

/// `T` decoded with the array rule of [`parse_params`], for MCP tool arguments (rmcp decodes them
/// before the pipe sees anything). Its schema is exactly `T`'s.
#[derive(Debug, Clone, PartialEq)]
pub struct Strict<T>(pub T);

impl<'de, T: DeserializeOwned + JsonSchema> Deserialize<'de> for Strict<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        check_arrays::<T>(&value).map_err(de::Error::custom)?;
        T::deserialize(value).map(Strict).map_err(de::Error::custom)
    }
}

impl<T: JsonSchema> JsonSchema for Strict<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }
    fn schema_id() -> Cow<'static, str> {
        T::schema_id()
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        T::json_schema(generator)
    }
}

/// Checks a JSON document without building it: fails on the first key repeated in one object
/// (compared after unescaping).
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(UniqueKeys)
    }
}

impl<'de> Visitor<'de> for UniqueKeys {
    type Value = UniqueKeys;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(self)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if seen.contains(&key) {
                // A repeated key can be huge; keep the reply well under the line limit.
                let shown: String = key.chars().take(64).collect();
                let more = if shown.len() < key.len() { "…" } else { "" };
                return Err(de::Error::custom(format_args!(
                    "duplicate key {shown:?}{more}"
                )));
            }
            map.next_value::<UniqueKeys>()?;
            seen.insert(key);
        }
        Ok(self)
    }
}

/// Longest `$ref` chain followed; no params type is recursive today.
const MAX_REF_HOPS: usize = 32;

/// Rejects a JSON array wherever `T`'s published schema allows none, naming its JSON Pointer.
pub(crate) fn check_arrays<T: JsonSchema>(value: &Value) -> Result<(), String> {
    // shortcut: schema regenerated per call, cache by TypeId if parse_params or profiles::load becomes hot.
    let schema = SchemaGenerator::default().into_root_schema_for::<T>();
    let root = schema.as_value();
    walk_schema(root, &[root], value, &mut String::new())
}

/// Checks `value` against the union of `schemas`. Arrays descend through `items`; objects through
/// `properties[key]` or a schema-valued `additionalProperties`. A key no schema describes
/// (unknown, or extra under `additionalProperties: true`) is not checked.
fn walk_schema(
    root: &Value,
    schemas: &[&Value],
    value: &Value,
    at: &mut String,
) -> Result<(), String> {
    let mut leaves = Vec::new();
    for s in schemas {
        schema_leaves(root, s, 0, &mut leaves)?;
    }
    match value {
        Value::Array(items) => {
            let allowing: Vec<&Value> = leaves.into_iter().filter(|l| admits(l, "array")).collect();
            if allowing.is_empty() {
                return Err(format!("unexpected array at \"{at}\""));
            }
            // A leaf without `items` (e.g. `true`) constrains nothing below.
            let item: Option<Vec<&Value>> = allowing.iter().map(|l| l.get("items")).collect();
            if let Some(item) = item {
                for (i, v) in items.iter().enumerate() {
                    walk_child(root, &item, v, at, &i.to_string())?;
                }
            }
        }
        Value::Object(fields) => {
            let allowing: Vec<&Value> =
                leaves.into_iter().filter(|l| admits(l, "object")).collect();
            for (k, v) in fields {
                let field: Option<Vec<&Value>> = allowing
                    .iter()
                    .map(|l| {
                        l.get("properties")
                            .and_then(|p| p.get(k))
                            .or_else(|| l.get("additionalProperties").filter(|a| a.is_object()))
                    })
                    .collect();
                // `Some(empty)`: no leaf allows an object here, and serde rejects it anyway.
                if let Some(field) = field.filter(|f| !f.is_empty()) {
                    let token = k.replace('~', "~0").replace('/', "~1");
                    walk_child(root, &field, v, at, &token)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// [`walk_schema`] one level down, with `token` appended to the pointer `at`.
fn walk_child(
    root: &Value,
    schemas: &[&Value],
    value: &Value,
    at: &mut String,
    token: &str,
) -> Result<(), String> {
    let len = at.len();
    at.push('/');
    at.push_str(token);
    walk_schema(root, schemas, value, at)?;
    at.truncate(len);
    Ok(())
}

/// Leaf schemas of `s`: `$ref` (`#/…`) followed, `anyOf`/`oneOf` flattened into their union.
fn schema_leaves<'a>(
    root: &'a Value,
    s: &'a Value,
    hops: usize,
    out: &mut Vec<&'a Value>,
) -> Result<(), String> {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .filter(|_| hops < MAX_REF_HOPS)
            .ok_or_else(|| format!("unresolved schema reference {r:?}"))?;
        return schema_leaves(root, target, hops + 1, out);
    }
    let branches: Vec<&Value> = ["anyOf", "oneOf"]
        .iter()
        .filter_map(|k| s.get(k).and_then(Value::as_array))
        .flatten()
        .collect();
    if branches.is_empty() {
        out.push(s);
    }
    for b in branches {
        schema_leaves(root, b, hops, out)?;
    }
    Ok(())
}

/// Whether the leaf schema allows a value of JSON type `kind` (`"array"` or `"object"`). A schema
/// with no `type`, `const` or `enum` (`true`, `serde_json::Value`) is unconstrained.
fn admits(leaf: &Value, kind: &str) -> bool {
    let of_kind = |v: &Value| match kind {
        "array" => v.is_array(),
        _ => v.is_object(),
    };
    match leaf {
        Value::Object(o) => match (o.get("type"), o.get("const"), o.get("enum")) {
            (Some(t), _, _) => {
                t == kind || t.as_array().is_some_and(|ts| ts.iter().any(|t| t == kind))
            }
            (None, Some(c), _) => of_kind(c),
            (None, None, Some(e)) => e.as_array().is_some_and(|vs| vs.iter().any(of_kind)),
            (None, None, None) => true,
        },
        Value::Bool(b) => *b,
        _ => true,
    }
}

fn not_an_object() -> serde_json::Error {
    serde::de::Error::invalid_type(serde::de::Unexpected::Seq, &"a JSON object")
}

/// Lower-case `0x…` rendering used for window styles (`"0x94000000"`).
pub fn hex(v: u32) -> String {
    format!("{v:#010x}")
}

fn default_true() -> bool {
    true
}

fn default_observe_ms() -> u64 {
    3000
}

// ------------------------------------------------------------ common types

/// Selects a window by handle or by process name and optional title substring.
///
/// Either `{hwnd}` or `{process, title?}` (title: case-insensitive substring).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Window handle; takes precedence over the other fields.
    pub hwnd: Option<isize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Process executable name, case-insensitive.
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Case-insensitive substring of the window title.
    pub title: Option<String>,
}

// ------------------------------------------------------------------- hello

/// Parameters of `hello` (none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelloParams {}

/// Handshake answer; clients must check `protocol` against [`PROTOCOL`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    /// Protocol revision, see [`PROTOCOL`].
    pub protocol: u32,
    /// Engine version string.
    pub version: String,
    /// Process id of the resident engine.
    pub pid: u32,
    /// Current revision of `profiles.json` (see [`crate::profiles::revision`]).
    pub profiles_revision: String,
}

// ------------------------------------------------------------ list_windows

/// Parameters of `list_windows`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListWindowsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Only windows of this process name.
    pub process: Option<String>,
    #[serde(default)]
    /// Also list invisible windows. Default false.
    pub include_hidden: bool,
    #[serde(default)]
    /// Only windows currently placed by the engine. Default false.
    pub only_managed: bool,
}

/// Client-area size in physical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientSize {
    /// Width.
    pub w: i32,
    /// Height.
    pub h: i32,
}

/// The monitor a window is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowMonitor {
    /// 0-based monitor index.
    pub index: usize,
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub device: String,
}

/// One top-level window as reported by `list_windows`. Geometry is in physical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    /// Window handle.
    pub hwnd: isize,
    /// Owning process id.
    pub pid: u32,
    /// Process executable name.
    pub process: String,
    /// Window title.
    pub title: String,
    /// Window class name.
    pub class: String,
    /// Whether the window is visible.
    pub visible: bool,
    /// Hexadecimal string, e.g. `"0x94000000"` (see [`hex`]).
    pub style: String,
    /// Extended style, hexadecimal string like `style`.
    pub exstyle: String,
    /// Outer rectangle.
    pub rect: Rect,
    /// Client-area size.
    pub client: ClientSize,
    /// Monitor the window is on.
    pub monitor: WindowMonitor,
    /// Effective DPI of the window (96 = 100 %).
    pub dpi: u32,
    /// DPI awareness of the owning process.
    pub dpi_awareness: DpiAwareness,
    /// `null` when the process cannot be queried.
    pub elevated: Option<bool>,
    /// Whether the window is maximized.
    pub maximized: bool,
    /// Whether the window is minimized.
    pub minimized: bool,
    /// Whether the engine currently holds a restore state for it.
    pub managed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// `layout/island` the window occupies, if any.
    pub island: Option<String>,
}

/// Answer of `list_windows`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListWindowsResult {
    /// Matching windows.
    pub windows: Vec<WindowInfo>,
}

// ------------------------------------------------------------ apply / place

/// Parameters of `apply`: make a window borderless and place it.
///
/// Explicit `island`/`monitor`/`offsets`/`delay_ms` override the matching profile, which is used
/// only when `use_profile` is true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyParams {
    /// Window to act on.
    pub window: WindowRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// `layout/island` target; overrides `monitor` and `offsets`.
    pub island: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Target monitor.
    pub monitor: Option<MonitorSel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Per-edge adjustment in physical pixels.
    pub offsets: Option<Offsets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Milliseconds to wait before applying, 0 to `MAX_DELAY_MS` (60000); larger values are rejected.
    pub delay_ms: Option<u64>,
    #[serde(default = "default_true")]
    /// Fall back to the process profile for unset fields. Default true.
    pub use_profile: bool,
}

/// Answer of `apply` and `place`. Rectangles are in physical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyResult {
    /// Whether the window was changed.
    pub applied: bool,
    /// `"already_placed"` or `"excluded"` when `applied` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Window handle.
    pub hwnd: isize,
    /// Rectangle the engine aimed for.
    pub target: Rect,
    #[serde(rename = "final")]
    /// Rectangle the window ended up with (`final` on the wire).
    pub r#final: Rect,
    /// Whether the window stayed at the target after the re-assert guard settled.
    pub settled: bool,
    /// Non-fatal findings.
    pub warnings: Vec<Warning>,
}

/// Parameters of `place`: put a window in an island without touching its styles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlaceParams {
    /// Window to act on.
    pub window: WindowRef,
    /// `layout/island` target.
    pub island: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Declared game resolution in physical pixels; the window is centred in the island.
    pub game_resolution: Option<Size>,
}

/// `place` answers like `apply`.
pub type PlaceResult = ApplyResult;

// ------------------------------------------------------------------ restore

/// Parameters of `restore`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreParams {
    /// Window to restore.
    pub window: WindowRef,
}

/// Answer of `restore`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreResult {
    /// Whether a saved state was replayed.
    pub restored: bool,
    /// Whether the window now matches the saved state exactly.
    pub exact: bool,
    /// Window handle.
    pub hwnd: isize,
}

/// Parameters of `restore_all` (none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreAllParams {}

/// A window `restore_all` could not restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreFailure {
    /// Window handle.
    pub hwnd: isize,
    /// Error description.
    pub error: String,
}

/// Answer of `restore_all`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreAllResult {
    /// Number of windows restored.
    pub restored: usize,
    /// Windows that could not be restored.
    pub failed: Vec<RestoreFailure>,
}

// ------------------------------------------------------------- list_islands

/// Parameters of `list_islands` (none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListIslandsParams {}

/// An island resolved to physical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IslandInfo {
    /// Island id within its layout.
    pub id: String,
    /// Already resolved to physical pixels.
    pub rect: Rect,
    /// Handles of managed windows currently in the island.
    pub occupants: Vec<isize>,
}

/// A layout with its resolved islands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LayoutInfo {
    /// Layout id.
    pub id: String,
    /// Monitor selector of the layout.
    pub monitor: MonitorSel,
    /// Resolved islands.
    pub islands: Vec<IslandInfo>,
}

/// Answer of `list_islands`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListIslandsResult {
    /// Connected monitors.
    pub monitors: Vec<MonitorInfo>,
    /// Presets merged with user layouts.
    pub layouts: Vec<LayoutInfo>,
}

// ----------------------------------------------------------------- profiles

/// Parameters of `get_profiles` (none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GetProfilesParams {}

/// Answer of `get_profiles`: the file content plus its revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GetProfilesResult {
    /// Absolute path of `profiles.json`.
    pub path: String,
    /// SHA-256 (lower-case hex) of the file bytes.
    pub revision: String,
    /// Schema version.
    pub version: u32,
    /// Per-game profiles.
    pub profiles: Vec<Profile>,
    /// User layouts (presets excluded).
    pub layouts: Vec<Layout>,
    /// Validation findings.
    pub warnings: Vec<Warning>,
}

/// Parameters of `set_profiles`: upsert, then remove.
///
/// A blocking warning (see [`crate::profiles::is_blocking`]) rejects the write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SetProfilesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Profiles to insert or replace, by [`ProfileKey`] (process file name and title rule).
    pub profiles: Option<Vec<Profile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Layouts to insert or replace (by id).
    pub layouts: Option<Vec<Layout>>,
    /// Profiles to delete: a process name deletes its process-only profile,
    /// `{process, title?, title_regex?}` the profile with that key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<ProfileKey>>,
    /// Layout ids to delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_layouts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Reject with `revision_conflict` unless the file still has this revision.
    pub expected_revision: Option<String>,
    /// Store every sent profile, layout and island exactly as sent, dropping unknown fields it
    /// does not repeat. When `false` (the default), a replaced item keeps its old unknown fields
    /// unless it brings its own (islands matched by `id` in the replaced layout).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_extras: bool,
}

/// Answer of `set_profiles`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SetProfilesResult {
    /// Revision of the file after the write.
    pub revision: String,
    /// Non-blocking findings.
    pub warnings: Vec<Warning>,
}

// ----------------------------------------------------------------- diagnose

/// Parameters of `diagnose`: place a window, watch whether it reverts, report why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnoseParams {
    /// Window to act on.
    pub window: WindowRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// `layout/island` target.
    pub island: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Target monitor.
    pub monitor: Option<MonitorSel>,
    /// Observation window in ms (default 3000, max [`MAX_OBSERVE_MS`]).
    #[serde(default = "default_observe_ms")]
    pub observe_ms: u64,
    /// Leave the window placed instead of restoring it.
    #[serde(default)]
    pub keep: bool,
}

/// Who produced a [`TimelineEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimelineSource {
    /// Our own write.
    Us,
    /// Change observed in the window.
    Window,
}

/// One sample of the diagnose timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineEntry {
    /// Milliseconds since the observation started.
    pub t_ms: f64,
    /// Who changed the window.
    pub source: TimelineSource,
    /// `"set_styles"` or `"set_pos"` for our own writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// Style bits as a hexadecimal string.
    pub style: String,
    /// Extended style bits as a hexadecimal string.
    pub exstyle: String,
    /// Outer rectangle in physical pixels.
    pub rect: Rect,
}

/// Answer of `diagnose`. Rectangles are in physical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnoseResult {
    /// Window handle.
    pub hwnd: isize,
    /// Process executable name.
    pub process: String,
    /// Rectangle the engine aimed for.
    pub target: Rect,
    /// Classification of the window's behaviour.
    pub verdict: Verdict,
    /// Observed changes in time order.
    pub timeline: Vec<TimelineEntry>,
    #[serde(rename = "final")]
    /// Rectangle at the end of the observation (`final` on the wire).
    pub r#final: Rect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Size the window insists on, if it enforces one.
    pub forced_size: Option<Size>,
    /// How quickly the window undid our changes.
    pub revert_latency_ms: Latencies,
    /// DPI awareness of the owning process.
    pub dpi_awareness: DpiAwareness,
    /// Suggested profile adjustments.
    pub recommendations: Vec<Recommendation>,
    /// Whether the window was restored afterwards (false with `keep`).
    pub restored: bool,
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello(protocol: Value) -> Value {
        json!({"protocol": protocol, "version": "0", "pid": 1, "profiles_revision": "r"})
    }

    #[test]
    fn protocol_mismatch_reads_only_the_revision() {
        assert_eq!(
            protocol_mismatch(&hello(json!(PROTOCOL))).expect("hello"),
            None
        );
        assert_eq!(protocol_mismatch(&hello(json!(7))).expect("hello"), Some(7));
        // Any other field may change shape without hiding the revision.
        let odd = json!({"protocol": PROTOCOL + 1, "pid": "not a number"});
        assert_eq!(protocol_mismatch(&odd).expect("hello"), Some(PROTOCOL + 1));
        assert!(matches!(
            protocol_mismatch(&hello(json!("3"))),
            Err(IpcError::Json(_))
        ));
        assert!(protocol_mismatch(&json!({})).is_err());
    }

    #[test]
    fn check_hello_accepts_only_our_protocol() {
        assert!(check_hello(hello(json!(PROTOCOL))).is_ok());
        let err = check_hello(hello(json!(PROTOCOL + 1))).expect_err("mismatch");
        assert!(
            matches!(&err, CallError::Ipc(IpcError::Protocol(m))
                if m.contains(&(PROTOCOL + 1).to_string())),
            "{err:?}"
        );
    }

    /// `v` must survive deserialise → serialise unchanged (canonical form).
    fn roundtrip<T: Serialize + DeserializeOwned>(v: Value) {
        let t: T = serde_json::from_value(v.clone()).expect("deserialize");
        assert_eq!(serde_json::to_value(&t).expect("serialize"), v);
    }

    /// Deserialise → serialise → deserialise must be a fixed point.
    fn stable<T: Serialize + DeserializeOwned>(v: Value) {
        let a: T = serde_json::from_value(v).expect("deserialize");
        let v1 = serde_json::to_value(&a).expect("serialize");
        let b: T = serde_json::from_value(v1.clone()).expect("redeserialize");
        assert_eq!(serde_json::to_value(&b).expect("serialize"), v1);
    }

    fn schema_of<T: JsonSchema>() {
        let s = serde_json::to_value(schemars::schema_for!(T)).expect("schema");
        assert!(s.is_object());
    }

    fn rect(l: i32, t: i32, r: i32, b: i32) -> Value {
        json!({"left": l, "top": t, "right": r, "bottom": b})
    }

    #[test]
    fn request_ids_int_and_string() {
        let r = decode_request(r#"{"id":7,"method":"hello","params":{}}"#).expect("int");
        assert_eq!(r.id, Id::Num(7));
        assert_eq!(r.method, "hello");
        let r = decode_request(r#"{"id":"abc","method":"hello"}"#).expect("str");
        assert_eq!(r.id, Id::Str("abc".into()));
        assert_eq!(r.params, Value::Null);
        assert_eq!(
            encode(&r).expect("enc"),
            "{\"id\":\"abc\",\"method\":\"hello\",\"params\":null}\n"
        );
    }

    #[test]
    fn decode_errors_are_classified() {
        assert_eq!(
            decode_request("{nope").expect_err("json").code,
            ErrorCode::InvalidJson
        );
        assert_eq!(
            decode_request(r#"{"id":1}"#).expect_err("shape").code,
            ErrorCode::InvalidParams
        );
        let long = format!(r#"{{"id":1,"method":"{}"}}"#, "x".repeat(MAX_LINE));
        assert_eq!(
            decode_request(&long).expect_err("long").code,
            ErrorCode::InvalidJson
        );
    }

    #[test]
    fn positional_array_is_not_a_request() {
        for line in [r#"["",""]"#, r#"[1,"hello",{}]"#, "[1,\"hello\"]"] {
            assert_eq!(
                decode_request(line).expect_err(line).code,
                ErrorCode::InvalidParams,
                "{line}"
            );
        }
        // A truncated array is still a syntax error.
        assert_eq!(
            decode_request("[1,").expect_err("cut").code,
            ErrorCode::InvalidJson
        );
    }

    #[test]
    fn leading_whitespace_before_request_is_accepted() {
        for line in [
            "  {\"id\":1,\"method\":\"m\"}",
            "\t{\"id\":1,\"method\":\"m\"}",
            "\n{\"id\":1,\"method\":\"m\"}",
        ] {
            assert_eq!(decode_request(line).expect(line).method, "m");
        }
    }

    #[test]
    fn truncated_positional_array_is_invalid_json() {
        // Fuzz inputs: the derived deserialiser used to fail on sequence length first.
        for line in [r#"["hwnd",15"#, r#"["x",1221850"#, r#"["window",false,["#] {
            assert_eq!(
                decode_request(line).expect_err(line).code,
                ErrorCode::InvalidJson,
                "{line}"
            );
        }
    }

    #[test]
    fn nesting_too_deep_for_value_is_invalid_json() {
        let n = 127;
        let line = format!(
            r#"{{"id":1,"method":"m","x":{}{}}}"#,
            "[".repeat(n),
            "]".repeat(n)
        );
        // Oracle: a plain JSON parse rejects this depth.
        assert!(serde_json::from_str::<Value>(&line).is_err());
        assert_eq!(
            decode_request(&line).expect_err("deep").code,
            ErrorCode::InvalidJson
        );
    }

    #[test]
    fn positional_array_params_are_rejected() {
        assert_eq!(
            parse_params::<RestoreParams>(json!([{"hwnd": 5}]))
                .expect_err("seq")
                .code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            parse_params::<HelloParams>(json!([]))
                .expect_err("empty seq")
                .code,
            ErrorCode::InvalidParams
        );
    }

    fn code_of(line: &str) -> ErrorCode {
        decode_request(line).expect_err(line).code
    }

    #[test]
    fn duplicate_envelope_key_is_invalid_params() {
        assert_eq!(
            code_of(r#"{"id":1,"id":2,"method":"hello"}"#),
            ErrorCode::InvalidParams
        );
        assert_eq!(
            code_of(r#"{"id":1,"method":"a","method":"b"}"#),
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn duplicate_nested_key_is_invalid_params() {
        assert_eq!(
            code_of(r#"{"id":1,"method":"apply","params":{"window":{"hwnd":1,"hwnd":2}}}"#),
            ErrorCode::InvalidParams
        );
        assert_eq!(
            code_of(
                r#"{"id":1,"method":"set_profiles","params":{"layouts":[{"id":"l","monitor":0,
                "islands":[{"id":"i","id":"j","rect":{"x":0,"y":0,"w":1,"h":1}}]}]}}"#
            ),
            ErrorCode::InvalidParams
        );
        // Inside an unknown field too.
        assert_eq!(
            code_of(r#"{"id":1,"method":"m","x":[{"k":1,"k":1}]}"#),
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn keys_equal_after_unescaping_are_duplicates() {
        assert_eq!(
            code_of(r#"{"id":1,"method":"m","params":{"a":1,"\u0061":2}}"#),
            ErrorCode::InvalidParams
        );
        // Distinct keys are fine, at any depth.
        let r =
            decode_request(r#"{"id":1,"method":"m","params":{"a":{"a":1},"b":[{"a":1},{"a":2}]}}"#)
                .expect("distinct keys");
        assert_eq!(r.params["b"][1]["a"], 2);
    }

    #[test]
    fn paramless_methods_take_null_or_an_object() {
        fn check<T: DeserializeOwned + JsonSchema + std::fmt::Debug>() {
            accepted::<T>(Value::Null);
            accepted::<T>(json!({}));
            accepted::<T>(json!({"extra": [1]}));
            assert_eq!(
                parse_params::<T>(json!([1, [2]])).expect_err("array").code,
                ErrorCode::InvalidParams
            );
        }
        check::<HelloParams>();
        check::<RestoreAllParams>();
        check::<ListIslandsParams>();
        check::<GetProfilesParams>();
    }

    #[test]
    fn huge_duplicate_key_yields_a_short_invalid_params() {
        let key = "k".repeat(400_000);
        let line = format!(r#"{{"id":1,"method":"m","params":{{"{key}":1,"{key}":2}}}}"#);
        let e = decode_request(&line).expect_err("duplicate");
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(e.message.len() < 200, "{} bytes", e.message.len());
        assert!(e.message.contains("duplicate key"), "{}", e.message);
    }

    /// Keywords the array walk reads (`schema_leaves`, `admits`, `walk_schema`).
    const WALKED: &[&str] = &[
        "$ref",
        "anyOf",
        "oneOf",
        "type",
        "const",
        "enum",
        "properties",
        "items",
        "additionalProperties",
    ];
    /// Keywords that only describe or bound scalars and cannot change where an array is allowed.
    const INERT: &[&str] = &[
        "$schema",
        "$defs",
        "title",
        "description",
        "required",
        "default",
        "format",
        "minimum",
        "maximum",
    ];

    /// Every schema keyword in `s` (a schema, not a value), recursing through subschemas.
    fn keywords(s: &Value, out: &mut std::collections::BTreeSet<String>) {
        let Value::Object(o) = s else { return };
        for (k, v) in o {
            out.insert(k.clone());
            match k.as_str() {
                "properties" | "$defs" | "definitions" | "patternProperties" => {
                    v.as_object()
                        .into_iter()
                        .flatten()
                        .for_each(|(_, v)| keywords(v, out));
                }
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                    v.as_array()
                        .into_iter()
                        .flatten()
                        .for_each(|v| keywords(v, out));
                }
                "items" | "additionalProperties" | "not" | "if" | "then" | "else" => {
                    keywords(v, out);
                }
                _ => {}
            }
        }
    }

    fn collect<T: JsonSchema>(out: &mut std::collections::BTreeSet<String>) {
        keywords(
            SchemaGenerator::default()
                .into_root_schema_for::<T>()
                .as_value(),
            out,
        );
    }

    #[test]
    fn every_params_schema_keyword_is_known_to_the_walk() {
        let mut found = std::collections::BTreeSet::new();
        collect::<HelloParams>(&mut found);
        collect::<ListWindowsParams>(&mut found);
        collect::<ApplyParams>(&mut found);
        collect::<PlaceParams>(&mut found);
        collect::<RestoreParams>(&mut found);
        collect::<RestoreAllParams>(&mut found);
        collect::<ListIslandsParams>(&mut found);
        collect::<GetProfilesParams>(&mut found);
        collect::<SetProfilesParams>(&mut found);
        collect::<DiagnoseParams>(&mut found);
        collect::<crate::profiles::ProfileFile>(&mut found);
        let unknown: Vec<&String> = found
            .iter()
            .filter(|k| !WALKED.contains(&k.as_str()) && !INERT.contains(&k.as_str()))
            .collect();
        assert!(
            unknown.is_empty(),
            "schema keyword(s) {unknown:?} are new: the array walk (`walk_schema`/`schema_leaves`) \
             must learn them, then add them to WALKED (or INERT if they cannot admit an array)"
        );
    }

    #[test]
    fn syntax_errors_win_over_duplicate_keys() {
        for line in [
            r#"{"id":1,"id":2,"method":"m""#,
            r#"{"id":1,"id":2,"method":"m",}"#,
            r#"{"id":1,"id":2,"method":"m"} x"#,
        ] {
            assert_eq!(code_of(line), ErrorCode::InvalidJson, "{line}");
        }
    }

    /// `params` is `invalid_params` because of the schema walk, which names the array's pointer.
    fn rejected<T: DeserializeOwned + JsonSchema + std::fmt::Debug>(params: Value, at: &str) {
        let e = parse_params::<T>(params.clone()).expect_err(&params.to_string());
        assert_eq!(e.code, ErrorCode::InvalidParams, "{params}");
        let want = format!("unexpected array at \"{at}\"");
        assert!(e.message.contains(&want), "{params}: {}", e.message);
    }

    fn accepted<T: DeserializeOwned + JsonSchema>(params: Value) -> T {
        parse_params::<T>(params.clone()).unwrap_or_else(|e| panic!("{params}: {}", e.message))
    }

    /// A minimal valid island for the `set_profiles` cases.
    fn island() -> Value {
        json!({"id": "i", "rect": {"x": 0, "y": 0, "w": 1, "h": 1}})
    }

    #[test]
    fn array_in_place_of_a_struct_is_invalid_params() {
        let w = json!({"hwnd": 1});
        // `$ref` into `$defs` (`WindowRef`).
        rejected::<ApplyParams>(json!({"window": [1]}), "/window");
        rejected::<PlaceParams>(json!({"window": [1], "island": "a/b"}), "/window");
        rejected::<RestoreParams>(json!({"window": [1]}), "/window");
        rejected::<DiagnoseParams>(json!({"window": [1]}), "/window");
        // `$ref` inside `anyOf` (`Option<Struct>`).
        rejected::<ApplyParams>(json!({"window": w, "offsets": [1, 2, 3, 4]}), "/offsets");
        rejected::<PlaceParams>(
            json!({"window": w, "island": "a/b", "game_resolution": [1280, 720]}),
            "/game_resolution",
        );
        // Structs inside list items.
        rejected::<SetProfilesParams>(
            json!({"profiles": [{"process": "a.exe", "offsets": [1, 2, 3, 4]}]}),
            "/profiles/0/offsets",
        );
        rejected::<SetProfilesParams>(
            json!({"profiles": [{"process": "a.exe", "game_resolution": [1, 2]}]}),
            "/profiles/0/game_resolution",
        );
        rejected::<SetProfilesParams>(
            json!({"layouts": [{"id": "l", "monitor": 0, "islands": [{"id": "i", "rect": [0, 0, 1, 1]}]}]}),
            "/layouts/0/islands/0/rect",
        );
        // The whole params.
        rejected::<RestoreParams>(json!([{"hwnd": 5}]), "");
    }

    #[test]
    fn array_where_a_non_object_is_expected_is_invalid_params() {
        let w = json!({"hwnd": 1});
        // `MonitorSel`: manual `anyOf` of integer and string, plain and inside `Option`.
        rejected::<ApplyParams>(json!({"window": w, "monitor": [0]}), "/monitor");
        rejected::<DiagnoseParams>(json!({"window": w, "monitor": ["primary"]}), "/monitor");
        rejected::<SetProfilesParams>(
            json!({"layouts": [{"id": "l", "monitor": [0], "islands": []}]}),
            "/layouts/0/monitor",
        );
        // `MonitorChoice`: manual `oneOf`.
        rejected::<SetProfilesParams>(
            json!({"profiles": [{"process": "a.exe", "monitor": [0]}]}),
            "/profiles/0/monitor",
        );
        // `Unit`: `oneOf` of `const`.
        let mut i = island();
        i["rect"]["unit"] = json!(["px"]);
        rejected::<SetProfilesParams>(
            json!({"layouts": [{"id": "l", "monitor": 0, "islands": [i]}]}),
            "/layouts/0/islands/0/rect/unit",
        );
        // Scalars: a plain `type`, a type list (`["integer","null"]`) and a field of a `$ref`.
        rejected::<PlaceParams>(json!({"window": w, "island": ["a/b"]}), "/island");
        rejected::<ApplyParams>(json!({"window": {"hwnd": [1]}}), "/window/hwnd");
        rejected::<PlaceParams>(
            json!({"window": w, "island": "a/b", "game_resolution": {"width": [1], "height": 2}}),
            "/game_resolution/width",
        );
    }

    #[test]
    fn list_items_must_have_their_own_shape() {
        rejected::<SetProfilesParams>(json!({"profiles": [["a.exe"]]}), "/profiles/0");
        rejected::<SetProfilesParams>(json!({"layouts": [["l", 0, []]]}), "/layouts/0");
        rejected::<SetProfilesParams>(
            json!({"layouts": [{"id": "l", "monitor": 0, "islands": [["i"]]}]}),
            "/layouts/0/islands/0",
        );
        rejected::<SetProfilesParams>(json!({"remove": [["a.exe"]]}), "/remove/0");
        rejected::<SetProfilesParams>(
            json!({"remove": [{"process": "a.exe", "title": ["x"]}]}),
            "/remove/0/title",
        );
        rejected::<SetProfilesParams>(json!({"remove_layouts": [["l"]]}), "/remove_layouts/0");
    }

    #[test]
    fn remove_items_are_a_process_name_or_a_key_object() {
        let p: SetProfilesParams = accepted(json!({"remove": [
            "a.exe",
            {"process": "B.exe", "title": " Main "},
            {"process": "c.exe", "title_regex": "^X", "title": null}
        ]}));
        assert_eq!(
            p.remove,
            Some(vec![
                ProfileKey::new("a.exe", None, None),
                ProfileKey::new("b.exe", Some("main"), None),
                ProfileKey::new("c.exe", None, Some("^X")),
            ])
        );
        // A misspelt rule field is an error, never the process-only key (it would delete that).
        for bad in [
            json!({"remove": [1]}),
            json!({"remove": [{"title": "x"}]}),
            json!({"remove": [{"process": "a.exe", "titel": "x"}]}),
        ] {
            assert_eq!(
                parse_params::<SetProfilesParams>(bad.clone())
                    .expect_err(&bad.to_string())
                    .code,
                ErrorCode::InvalidParams
            );
        }
    }

    #[test]
    fn pointer_escapes_tilde_and_slash() {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct Odd {
            #[serde(rename = "a/b~c")]
            _x: Offsets,
        }
        rejected::<Odd>(json!({"a/b~c": [1]}), "/a~1b~0c");
    }

    #[test]
    fn maps_check_their_values() {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct WithMaps {
            #[serde(default)]
            typed: std::collections::BTreeMap<String, Offsets>,
            #[serde(default)]
            any: serde_json::Map<String, Value>,
        }
        rejected::<WithMaps>(json!({"typed": {"k": [1, 2, 3, 4]}}), "/typed/k");
        let m: WithMaps = accepted(json!({"typed": {"k": {"left": 1}}, "any": {"k": [1, [2]]}}));
        assert_eq!(m.typed["k"].left, 1);
        assert_eq!(m.any["k"], json!([1, [2]]));
    }

    #[test]
    fn objects_nulls_lists_and_extras_are_still_accepted() {
        let w = json!({"hwnd": 1});
        accepted::<ApplyParams>(json!({"window": w, "offsets": {"left": 1}, "monitor": "primary"}));
        accepted::<ApplyParams>(json!({"window": w, "monitor": 0}));
        let a: ApplyParams = accepted(json!({
            "window": w, "offsets": null, "monitor": null, "game_resolution": null, "island": null
        }));
        assert_eq!((a.offsets, a.monitor, a.island), (None, None, None));
        accepted::<PlaceParams>(json!({"window": w, "island": "a/b", "game_resolution": null}));
        accepted::<PlaceParams>(
            json!({"window": w, "island": "a/b", "game_resolution": {"width": 1, "height": 2}}),
        );
        let mut i = island();
        i["_x"] = json!([1, {"k": [2]}]);
        let p: SetProfilesParams = accepted(json!({
            "profiles": [{"process": "a.exe", "monitor": "current", "offsets": {"top": 2},
                          "game_resolution": {"width": 1, "height": 2}, "_x": [[1]]}],
            "layouts": [{"id": "l", "monitor": "primary", "islands": [i], "_y": [{"a": []}]}],
            "remove": ["b.exe"],
            "remove_layouts": ["m"],
            "unknown": [1, [2], {"k": []}]
        }));
        assert_eq!(p.profiles.expect("profiles")[0].extra["_x"], json!([[1]]));
        assert_eq!(
            p.layouts.expect("layouts")[0].islands[0].extra["_x"],
            json!([1, {"k": [2]}])
        );
        accepted::<SetProfilesParams>(json!({"profiles": null, "layouts": null, "remove": null}));
        accepted::<DiagnoseParams>(json!({"window": {"process": "a.exe"}, "extra": [1]}));
    }

    #[test]
    fn encode_rejects_oversized_line() {
        let r = Response::ok(Id::Num(1), Value::String("x".repeat(MAX_LINE)));
        assert!(matches!(encode(&r), Err(IpcError::LineTooLong)));
    }

    #[test]
    fn response_shapes() {
        let ok = Response::ok(Id::Num(7), json!({"a": 1}));
        assert_eq!(
            encode(&ok).expect("enc"),
            "{\"id\":7,\"result\":{\"a\":1}}\n"
        );
        let err = Response::err(Id::Str("k".into()), ErrorBody::new(ErrorCode::Busy, "busy"));
        assert_eq!(
            encode(&err).expect("enc"),
            "{\"id\":\"k\",\"error\":{\"code\":\"busy\",\"message\":\"busy\"}}\n"
        );
        let with_data = ErrorBody::new(ErrorCode::Win32Error, "x")
            .with_data(json!({"call": "SetWindowPos", "code": 5}));
        roundtrip::<Response>(
            json!({"id": 1, "error": serde_json::to_value(with_data).expect("v")}),
        );
        roundtrip::<Response>(json!({"id": 1, "result": {"k": [1, 2]}}));
    }

    #[test]
    fn error_codes_are_stable() {
        use ErrorCode::*;
        let table = [
            (InvalidParams, "invalid_params"),
            (UnknownMethod, "unknown_method"),
            (InvalidJson, "invalid_json"),
            (WindowNotFound, "window_not_found"),
            (AmbiguousWindow, "ambiguous_window"),
            (WindowGone, "window_gone"),
            (AccessDenied, "access_denied"),
            (ExcludedProcess, "excluded_process"),
            (IslandNotFound, "island_not_found"),
            (MonitorUnavailable, "monitor_unavailable"),
            (ProfileError, "profile_error"),
            (RevisionConflict, "revision_conflict"),
            (UnsupportedVersion, "unsupported_version"),
            (Win32Error, "win32_error"),
            (Busy, "busy"),
            (Internal, "internal"),
        ];
        for (code, s) in table {
            assert_eq!(serde_json::to_value(code).expect("ser"), json!(s));
            assert_eq!(
                serde_json::from_value::<ErrorCode>(json!(s)).expect("de"),
                code
            );
        }
    }

    #[test]
    fn unknown_params_fields_are_ignored_and_defaults_apply() {
        let p: ApplyParams =
            parse_params(json!({"window": {"hwnd": 5}, "bogus": true})).expect("apply");
        assert!(p.use_profile);
        assert_eq!(p.window.hwnd, Some(5));
        let d: DiagnoseParams =
            parse_params(json!({"window": {"process": "a.exe"}, "x": 1})).expect("diag");
        assert_eq!(d.observe_ms, 3000);
        assert!(!d.keep);
        let l: ListWindowsParams = parse_params(Value::Null).expect("null params");
        assert_eq!(l, ListWindowsParams::default());
        assert_eq!(
            parse_params::<ApplyParams>(json!({}))
                .expect_err("no window")
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn params_roundtrip() {
        roundtrip::<HelloParams>(json!({}));
        roundtrip::<WindowRef>(json!({"process": "PWAAT.exe", "title": "x"}));
        roundtrip::<ListWindowsParams>(
            json!({"process": "a.exe", "include_hidden": true, "only_managed": false}),
        );
        roundtrip::<ApplyParams>(json!({
            "window": {"hwnd": 197934},
            "island": "halves-16-9/right",
            "monitor": "primary",
            "offsets": {"left": 1, "top": 2, "right": 3, "bottom": 4},
            "delay_ms": 100,
            "use_profile": false
        }));
        roundtrip::<PlaceParams>(json!({
            "window": {"hwnd": 1}, "island": "l/i", "game_resolution": {"width": 1280, "height": 720}
        }));
        roundtrip::<RestoreParams>(json!({"window": {"hwnd": 1}}));
        roundtrip::<SetProfilesParams>(json!({
            "remove": ["a.exe"], "remove_layouts": ["l"], "expected_revision": "abc"
        }));
        roundtrip::<DiagnoseParams>(json!({
            "window": {"hwnd": 1}, "island": "l/i", "monitor": 1, "observe_ms": 3000, "keep": true
        }));
    }

    #[test]
    fn results_roundtrip() {
        roundtrip::<HelloResult>(
            json!({"protocol": 2, "version": "0.2.0", "pid": 4, "profiles_revision": "ab"}),
        );
        roundtrip::<ApplyResult>(json!({
            "applied": true, "hwnd": 9, "target": rect(0, 0, 2, 2), "final": rect(0, 0, 2, 2),
            "settled": true, "warnings": []
        }));
        roundtrip::<ApplyResult>(json!({
            "applied": false, "reason": "already_placed", "hwnd": 9, "target": rect(0, 0, 2, 2),
            "final": rect(0, 0, 2, 2), "settled": true, "warnings": []
        }));
        roundtrip::<RestoreResult>(json!({"restored": true, "exact": true, "hwnd": 9}));
        roundtrip::<RestoreAllResult>(
            json!({"restored": 2, "failed": [{"hwnd": 3, "error": "boom"}]}),
        );
        roundtrip::<ListIslandsResult>(json!({
            "monitors": [],
            "layouts": [{"id": "l", "monitor": "primary",
                         "islands": [{"id": "i", "rect": rect(0, 0, 5, 5), "occupants": [1]}]}]
        }));
        roundtrip::<GetProfilesResult>(json!({
            "path": "C:\\p.json", "revision": "ab", "version": 2, "profiles": [], "layouts": [], "warnings": []
        }));
        roundtrip::<SetProfilesResult>(json!({"revision": "ab", "warnings": []}));
        roundtrip::<ListWindowsResult>(json!({"windows": [{
            "hwnd": 1, "pid": 2, "process": "a.exe", "title": "t", "class": "c", "visible": true,
            "style": hex(0x9400_0000), "exstyle": hex(0), "rect": rect(0, 0, 4, 4),
            "client": {"w": 4, "h": 4}, "monitor": {"index": 0, "device": "\\\\.\\DISPLAY1"},
            "dpi": 96, "dpi_awareness": "per_monitor_v2", "elevated": null,
            "maximized": false, "minimized": false, "managed": true, "island": "l/i"
        }]}));
        // `Latencies`/`Recommendation` belong to `diagnose`: only require a stable re-encode.
        stable::<DiagnoseResult>(json!({
            "hwnd": 1, "process": "a.exe", "target": rect(0, 0, 4, 4), "verdict": "size_forced",
            "timeline": [{"t_ms": 0.5, "source": "us", "op": "set_pos", "style": "0x00000000",
                          "exstyle": "0x00000000", "rect": rect(0, 0, 4, 4)},
                         {"t_ms": 60.0, "source": "window", "style": "0x00000000",
                          "exstyle": "0x00000000", "rect": rect(0, 0, 2, 2)}],
            "final": rect(0, 0, 2, 2), "forced_size": {"width": 2, "height": 2},
            "revert_latency_ms": {"style": null, "rect": 60.0}, "dpi_awareness": "unaware",
            "recommendations": [{"code": "c", "text": "t"}], "restored": true
        }));
    }

    #[test]
    fn hex_is_lower_case_padded() {
        assert_eq!(hex(0x9400_0000), "0x94000000");
        assert_eq!(hex(0x10), "0x00000010");
    }

    #[test]
    fn schemas_generate_for_every_type() {
        schema_of::<Request>();
        schema_of::<Response>();
        schema_of::<ErrorBody>();
        schema_of::<WindowRef>();
        schema_of::<HelloParams>();
        schema_of::<HelloResult>();
        schema_of::<ListWindowsParams>();
        schema_of::<ListWindowsResult>();
        schema_of::<ApplyParams>();
        schema_of::<ApplyResult>();
        schema_of::<PlaceParams>();
        schema_of::<RestoreParams>();
        schema_of::<RestoreResult>();
        schema_of::<RestoreAllParams>();
        schema_of::<RestoreAllResult>();
        schema_of::<ListIslandsParams>();
        schema_of::<ListIslandsResult>();
        schema_of::<GetProfilesParams>();
        schema_of::<GetProfilesResult>();
        schema_of::<SetProfilesParams>();
        schema_of::<SetProfilesResult>();
        schema_of::<DiagnoseParams>();
        schema_of::<DiagnoseResult>();
        let s = serde_json::to_value(schemars::schema_for!(ApplyResult)).expect("schema");
        assert!(
            s["properties"].get("final").is_some(),
            "final field renamed in the schema"
        );
    }
}
