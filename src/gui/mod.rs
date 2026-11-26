//! `borderless-shatter gui`: separate GPUI process; talks to the resident engine ONLY through
//! the named-pipe IPC (`ipc::pipe::Client`). It never touches windows directly.
//!
//! Stack: the single dependency `gpui-kit` =0.11.0 (Apache-2.0) = `gpui-pre` 0.3.14 (gpui
//! snapshot) + `gpui_platform` + `gpui-component`. Code says `use gpui_kit::…` for gpui
//! (re-exported) and `gpui_kit::component::…` for widgets (button, list, table, input…).
//! Never copy code from Zed's GPL `crates/ui`.
//!
//! # Patterns (follow them so views can be written in parallel)
//!
//! **View**: one struct per section in its own file `src/gui/<name>.rs`, `impl Render`.
//! Keep state in plain fields; `render` only reads them. The root `App` owns every view as
//! `Entity<View>` (created once in `App::new`) and shows the one picked in the sidebar. The
//! view's root element is `div().size_full().flex().flex_col()`; start it with
//! `section_header(title, subtitle)`. Scrolling content goes in
//! `div().id("x").flex_1().overflow_y_scroll()` (the shell clips, it does not scroll).
//!
//! **Look** (`theme.rs`; read its module doc for the rules). Never hard-code colours, radii or
//! paddings: use the helpers and constants. Spacing `SP_1..SP_8` (4/8/12/16/24/32), radii
//! `R_SM/R_MD/R_LG` (6/10/14; outer = inner + padding, see `inner_radius`), text `TEXT_SM/MD/XL`
//! (12/14/22), colours `bg()/surface()/raised()/text()/text_muted()/accent()`.
//! - `card()`: surface panel (r14, p16, soft shadow) — `.child(..)` your content in it.
//! - `section_header(title, subtitle)`: top of every view.
//! - `primary_button(id, label, on_click)` (one main action) / `secondary_button(..)` (the
//!   rest): 40 px hit area, `.icon(IconName)`, `.disabled(bool)` while an IPC call is in
//!   flight. `on_click` is typically `cx.listener(|this, _, _, cx| ..)`.
//! - `badge(text, BadgeKind::{Ok,Warn,Error,Info,Neutral})`: status pill; status always has
//!   text, never colour alone.
//! - `empty_state(title, body)`: nothing-to-show placeholder; `stat(label, value)`: big number
//!   + label (tabular digits; wrap any other changing number in `.font_features(tabular())`).
//! - `monitor_frame(label)`: bezel to draw a monitor to scale (Islas / Editor).
//! - Depth uses `.shadow(shadow_soft())` / `shadow_pop()`, not borders.
//!
//! Widgets from `gpui_kit::component` (input, select, table…) already use the same palette.
//!
//! **IPC from a view** (the pipe client is blocking and owns its own tokio runtime, so it
//! must NEVER run on the UI thread): call [`ipc_call`] with a closure that gets a connected
//! `Client`, then write the result back into `self`:
//! ```ignore
//! fn refresh(&mut self, cx: &mut Context<Self>) {
//!     self.state = Load::Loading;
//!     cx.spawn(async move |this, cx| {
//!         let r = ipc_call(cx, |c| Ok(c.call("list_windows", json!({}))?)).await;
//!         this.update(cx, |t, cx| { t.state = Load::from(r); cx.notify(); }).ok();
//!     }).detach();
//! }
//! ```
//! `cx.notify()` after every state change or nothing is repainted. Show [`Load::Error`]
//! with `error_view` (it explains "resident not running"). Every call reconnects: a stale
//! connection can never wedge the panel; pipe connect is sub-millisecond.
//!
//! **New section**: add a variant to `Section` (+ `ALL`, `label`, `icon`), a `mod` + field in
//! `App`, and a match arm in `App::render`. Nothing else. Ctrl+1..5 and the sidebar follow
//! `Section::ALL` automatically (extend the `1..=5` range in `App::on_key`).
//!
//! **Units**: coordinates are physical pixels (IPC contract); GPUI uses logical `px`, so
//! the island editor converts preview px <-> physical px itself.
//!
//! **Engine errors vs transport errors**: `ipc_call` turns every failure into a string, which
//! loses `ErrorBody.code`. A view that must branch on the code (`revision_conflict`,
//! `profile_error`, `unsupported_version`, `busy`, `island_not_found`, `monitor_unavailable`)
//! runs `split(c.call(..))?` inside the closure: `Ok(Reply::Ok(v))` is the result,
//! `Ok(Reply::Remote(body))` an engine error (match on `body.code`, never on the message; show
//! `remote_message(&body)`), and `Err(_)` stays a transport failure. Decode results with
//! `.map_err(json_err)`.
//!
//! **Single instance**: [`run`] holds the named mutex `Local\BorderlessShatter.Gui` (suffix
//! `.<s>` when `BORDERLESS_SHATTER_INSTANCE=<s>`); a second `gui` raises the first window
//! (found by class + title "Borderless Shatter" / `Borderless Shatter (<s>)`) and exits.

mod diagnose_view;
pub mod editor_model;
mod editor_view;
mod islands_view;
pub mod monitor_preview;
mod profiles_view;
pub mod theme;
mod windows_view;

use std::borrow::Cow;
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use gpui_kit::component::{ActiveTheme as _, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App as GpuiApp, AppContext as _, AsyncApp, Bounds, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, size,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, DETACHED_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{HSTRING, w};

use crate::i18n::{self, Locale};
use crate::ipc::pipe::{Client, instance_suffix};
use crate::ipc::{CallError, ErrorBody, ErrorCode, IpcError};
use crate::settings::{self, SaveError};

use theme::{
    BadgeKind, R_LG, R_SM, SP_1, SP_2, SP_3, SP_6, SP_8, TEXT_MD, TEXT_SM, badge, bg, nav_item,
    primary_button, raised, secondary_button, shadow_soft, surface, text, text_muted,
};
use windows_view::Msg;

const TITLE: &str = "Borderless Shatter";

/// Named-mutex name of the single-instance guard; `suffix` isolates test instances.
fn mutex_name(suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("Local\\BorderlessShatter.Gui.{s}"),
        None => "Local\\BorderlessShatter.Gui".to_owned(),
    }
}

/// Window title (also what a second launch searches to raise the first panel).
fn window_title(suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{TITLE} ({s})"),
        None => TITLE.to_owned(),
    }
}

/// Result of a background IPC call as seen by a view.
pub enum Load<T> {
    Loading,
    Ready(T),
    /// Message already phrased for the user.
    Error(String),
}

/// Run `f` with a connected IPC client on a background thread (never the UI thread).
pub async fn ipc_call<T: Send + 'static>(
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Client) -> Result<T, CallError> + Send + 'static,
) -> Result<T, String> {
    cx.background_executor()
        .spawn(async move {
            let mut c = Client::connect(Duration::from_millis(500))
                .map_err(|_| i18n::t("common.error.resident_unreachable").into_owned())?;
            f(&mut c).map_err(|e| call_error_text(&e))
        })
        .await
}

/// Plain sentence for a failed call: the engine code picks it, raw transport text never shows.
fn call_error_text(e: &CallError) -> String {
    match e {
        CallError::Remote(b) => i18n::error_sentence(b.code).into_owned(),
        CallError::Ipc(_) => i18n::t("common.error.transport_error").into_owned(),
    }
}

/// Like [`ipc_call`], but the engine error code survives as [`Failure::Engine`].
pub async fn call<T: Send + 'static>(
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Client) -> Result<Reply<T>, CallError> + Send + 'static,
) -> Result<T, Failure> {
    cx.background_executor()
        .spawn(async move {
            let mut c = Client::connect(Duration::from_millis(500)).map_err(|_| Failure::Down)?;
            match f(&mut c) {
                Ok(Reply::Ok(v)) => Ok(v),
                Ok(Reply::Remote(b)) => Err(Failure::Engine(b.code)),
                Err(_) => Err(Failure::Transport),
            }
        })
        .await
}

/// One IPC request decoded into `T`; an engine error stays a [`Reply::Remote`].
pub fn get<T: DeserializeOwned>(
    c: &mut Client,
    method: &str,
    params: Value,
) -> Result<Reply<T>, CallError> {
    Ok(match split(c.call(method, params))? {
        Reply::Ok(v) => Reply::Ok(serde_json::from_value(v).map_err(json_err)?),
        Reply::Remote(b) => Reply::Remote(b),
    })
}

pub fn lift<T, U>(r: Reply<T>, f: impl FnOnce(T) -> U) -> Reply<U> {
    match r {
        Reply::Ok(v) => Reply::Ok(f(v)),
        Reply::Remote(b) => Reply::Remote(b),
    }
}

/// Snake-case wire name of an error code.
pub fn code_str(c: ErrorCode) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".into())
}

/// Why a call failed, without the raw OS or transport text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Down,
    Transport,
    Engine(ErrorCode),
}

/// Plain sentence plus the small stable `code`; never raw OS or transport text.
pub fn failure_text(f: &Failure) -> (String, String) {
    match f {
        Failure::Down => (
            i18n::t("common.error.resident_unreachable").into_owned(),
            "resident_unreachable".to_owned(),
        ),
        Failure::Transport => (
            i18n::t("common.error.transport_error").into_owned(),
            "transport_error".to_owned(),
        ),
        Failure::Engine(c) => (i18n::error_sentence(*c).into_owned(), code_str(*c)),
    }
}

/// A failed action as a notice: sentence plus the small stable code.
pub fn failure_msg(f: &Failure) -> Msg {
    let (text, code) = failure_text(f);
    Msg {
        kind: BadgeKind::Error,
        text,
        detail: vec![i18n::tf("gui.error.code", &[("code", &code)])],
    }
}

/// A fresh load failure is the one current error: an old error notice (say, "busy") goes.
pub fn without_stale_error(notice: Option<Msg>) -> Option<Msg> {
    notice.filter(|m| m.kind != BadgeKind::Error)
}

/// Display name of a layout: the catalog name for a built-in preset, else its id.
pub fn layout_label(id: &str) -> String {
    let key = format!("common.preset.{id}");
    if i18n::has(&key) {
        i18n::t(&key).into_owned()
    } else {
        id.to_owned()
    }
}

/// Notice card of a view: status badge, sentence and small detail lines.
pub fn notice_view(m: &Msg) -> gpui_kit::Div {
    let label = match m.kind {
        BadgeKind::Ok => i18n::t("gui.notice.done"),
        BadgeKind::Error => i18n::t("gui.notice.error"),
        BadgeKind::Warn => i18n::t("gui.notice.warning"),
        _ => i18n::t("gui.notice.info"),
    };
    div()
        .flex()
        .flex_col()
        .gap(SP_1)
        .p(SP_3)
        .rounded(R_SM)
        .bg(raised())
        .child(
            div()
                .flex()
                .items_center()
                .gap(SP_2)
                .child(badge(label.into_owned(), m.kind))
                .child(div().child(m.text.clone())),
        )
        .children(m.detail.iter().map(|d| {
            div()
                .pl(SP_2)
                .text_size(TEXT_SM)
                .text_color(text_muted())
                .child(d.clone())
        }))
}

/// An IPC call split into result and engine error (see the module doc).
pub enum Reply<T> {
    Ok(T),
    Remote(ErrorBody),
}

/// Keep engine errors (they carry `code`) apart from transport failures.
pub fn split<T>(r: Result<T, CallError>) -> Result<Reply<T>, CallError> {
    match r {
        Ok(v) => Ok(Reply::Ok(v)),
        Err(CallError::Remote(b)) => Ok(Reply::Remote(b)),
        Err(e) => Err(e),
    }
}

pub fn json_err(e: serde_json::Error) -> CallError {
    CallError::Ipc(IpcError::from(e))
}

/// User-facing text of an engine error: the rejected warnings if it carries them, else the
/// sentence for its code.
pub fn remote_message(e: &ErrorBody) -> String {
    let rejected: Vec<String> = e
        .data
        .as_ref()
        .and_then(|d| d.get("warnings"))
        .and_then(|w| serde_json::from_value::<Vec<crate::profiles::Warning>>(w.clone()).ok())
        .map(|ws| ws.iter().map(profiles_view::warning_text).collect())
        .unwrap_or_default();
    if rejected.is_empty() {
        i18n::error_sentence(e.code).into_owned()
    } else {
        rejected.join(" ")
    }
}

/// Error placeholder shared by all views.
pub fn error_view(msg: &str, cx: &GpuiApp) -> impl IntoElement {
    theme::card()
        .text_color(cx.theme().danger)
        .child(SharedString::from(msg.to_owned()))
}

/// Launch this executable again with `args`, detached from our console and lifetime.
/// `&["gui"]` opens the panel, `&[]` starts the resident process.
pub fn spawn_detached(args: &[&str]) -> std::io::Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .args(args)
        .creation_flags(DETACHED_PROCESS.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Windows,
    Islands,
    IslandEditor,
    Profiles,
    Diagnose,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Windows,
        Section::Islands,
        Section::IslandEditor,
        Section::Profiles,
        Section::Diagnose,
    ];

    fn label(self) -> Cow<'static, str> {
        match self {
            Section::Windows => i18n::t("gui.section.windows"),
            Section::Islands => i18n::t("gui.section.islands"),
            Section::IslandEditor => i18n::t("gui.section.island_editor"),
            Section::Profiles => i18n::t("gui.section.profiles"),
            Section::Diagnose => i18n::t("gui.section.diagnose"),
        }
    }

    /// Stable element id (the label changes with the language).
    fn slug(self) -> &'static str {
        match self {
            Section::Windows => "nav-windows",
            Section::Islands => "nav-islands",
            Section::IslandEditor => "nav-island-editor",
            Section::Profiles => "nav-profiles",
            Section::Diagnose => "nav-diagnose",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::Windows => IconName::Frame,
            Section::Islands => IconName::LayoutDashboard,
            Section::IslandEditor => IconName::Settings2,
            Section::Profiles => IconName::User,
            Section::Diagnose => IconName::Inspector,
        }
    }
}

/// How long a probe waits for the pipe: a busy resident must not read as "not running".
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Is the resident process answering?
fn probe() -> bool {
    Client::connect(PROBE_TIMEOUT).is_ok_and(|mut c| c.call("hello", json!({})).is_ok())
}

/// Debounce: a connected panel needs two failed probes in a row before it says "down" (one
/// slow answer must not flicker the sidebar). Returns the connection to show and the new count.
fn settle(connected: Option<bool>, misses: u8, up: bool) -> (bool, u8) {
    if up {
        return (true, 0);
    }
    let misses = misses.saturating_add(1);
    (connected == Some(true) && misses < 2, misses)
}

/// What the sidebar status shows (dot colour and label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Starting,
    StartFailed,
    Up,
    Down,
    Checking,
}

impl Status {
    fn of(connected: Option<bool>, starting: bool, start_failed: bool) -> Self {
        match (connected, starting, start_failed) {
            (_, true, _) => Status::Starting,
            (_, _, true) => Status::StartFailed,
            (Some(true), ..) => Status::Up,
            (Some(false), ..) => Status::Down,
            (None, ..) => Status::Checking,
        }
    }

    fn dot(self) -> gpui_kit::Hsla {
        match self {
            Status::Up => theme::ok(),
            Status::Down => theme::warn(),
            Status::StartFailed => theme::danger(),
            Status::Starting | Status::Checking => text_muted(),
        }
    }

    fn label(self) -> Cow<'static, str> {
        i18n::t(match self {
            Status::Starting => "gui.status.starting",
            Status::StartFailed => "gui.status.start_failed",
            Status::Up => "gui.status.connected",
            Status::Down => "gui.status.down",
            Status::Checking => "gui.status.checking",
        })
    }
}

/// How saving the language choice went (shown under the selector).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LangNote {
    Saved,
    /// The settings file is unreadable or from a newer version: never overwritten.
    Refused,
    Failed,
}

impl LangNote {
    fn text(self) -> Cow<'static, str> {
        match self {
            LangNote::Saved => i18n::t("gui.language.tray_note"),
            LangNote::Refused => i18n::t("gui.language.save_refused"),
            LangNote::Failed => i18n::t("gui.language.save_failed"),
        }
    }
}

/// Store the choice (`None` = follow the system) in the settings file at `path`.
fn persist_language(path: Option<&Path>, choice: Option<Locale>) -> LangNote {
    let Some(path) = path else {
        return LangNote::Failed;
    };
    match settings::save(path, &settings::Settings { language: choice }) {
        Ok(()) => LangNote::Saved,
        Err(SaveError::Io(_)) => LangNote::Failed,
        Err(_) => LangNote::Refused,
    }
}

/// Locale a choice stands for right now: the pick itself, or the OS language for "System".
fn effective_locale(choice: Option<Locale>) -> Locale {
    i18n::resolve(None, choice, i18n::os_ui_language())
}

struct App {
    section: Section,
    focus: FocusHandle,
    /// `None` until the first probe answers.
    connected: Option<bool>,
    /// Consecutive failed probes (see [`settle`]).
    misses: u8,
    starting: bool,
    start_failed: bool,
    /// Language picked in the selector (`None` = system).
    language: Option<Locale>,
    lang_note: Option<LangNote>,
    windows: Entity<windows_view::WindowsView>,
    islands: Entity<islands_view::IslandsView>,
    editor: Entity<editor_view::EditorView>,
    profiles: Entity<profiles_view::ProfilesView>,
    diagnose: Entity<diagnose_view::DiagnoseView>,
    _subs: Vec<Subscription>,
}

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Probe the resident every 2 s (off the UI thread) for the sidebar indicator.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let up = cx.background_executor().spawn(async { probe() }).await;
                if this
                    .update_in(cx, |t, window, cx| t.on_probe(up, window, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
        let windows = cx.new(|cx| windows_view::WindowsView::new(window, cx));
        let islands = cx.new(|cx| islands_view::IslandsView::new(window, cx));
        let editor = cx.new(|cx| editor_view::EditorView::new(window, cx));
        let profiles = cx.new(|cx| profiles_view::ProfilesView::new(window, cx));
        let diagnose = cx.new(|cx| diagnose_view::DiagnoseView::new(window, cx));
        // Cross-view jumps; the subscriptions live as long as the app.
        let subs = vec![
            cx.subscribe_in(
                &windows,
                window,
                |t, _, ev: &windows_view::DiagnoseWindow, _, cx| {
                    t.section = Section::Diagnose;
                    t.diagnose.update(cx, |v, cx| v.select_window(ev.hwnd, cx));
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &diagnose,
                window,
                |t, _, ev: &diagnose_view::UseGameResolution, window, cx| {
                    t.section = Section::Profiles;
                    t.profiles.update(cx, |v, cx| {
                        v.prefill_resolution(&ev.process, ev.width, ev.height, window, cx)
                    });
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &islands,
                window,
                |t, _, ev: &islands_view::OpenEditor, window, cx| {
                    t.section = Section::IslandEditor;
                    t.editor.update(cx, |v, cx| {
                        v.select_layout(ev.layout.as_deref(), window, cx)
                    });
                    cx.notify();
                },
            ),
        ];
        Self {
            section: Section::Windows,
            focus: cx.focus_handle(),
            connected: None,
            misses: 0,
            starting: false,
            start_failed: false,
            language: settings::default_path().and_then(|p| settings::load(&p).settings.language),
            lang_note: None,
            windows,
            islands,
            editor,
            profiles,
            diagnose,
            _subs: subs,
        }
    }

    /// Apply a debounced probe result (see [`settle`]).
    fn on_probe(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (up, misses) = settle(self.connected, self.misses, up);
        self.misses = misses;
        self.set_connected(up, window, cx);
    }

    fn set_connected(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if up {
            self.misses = 0;
            self.starting = false;
            self.start_failed = false;
        }
        if self.connected != Some(up) {
            self.connected = Some(up);
            if up {
                self.refresh_visible(window, cx);
            }
            cx.notify();
        }
    }

    /// After a reconnect only the view on screen reloads; the others load when polled or retried.
    fn refresh_visible(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.section {
            Section::Windows => self.windows.update(cx, |v, cx| v.refresh(cx)),
            Section::Islands => self.islands.update(cx, |v, cx| v.refresh(cx)),
            Section::IslandEditor => self.editor.update(cx, |v, cx| v.refresh(window, cx)),
            Section::Profiles => self.profiles.update(cx, |v, cx| v.refresh(cx)),
            Section::Diagnose => self.diagnose.update(cx, |v, cx| v.refresh(cx)),
        }
    }

    /// Switch the language now and remember it; every view repaints from the catalog.
    fn set_language(&mut self, choice: Option<Locale>, cx: &mut Context<Self>) {
        self.language = choice;
        i18n::set_locale(effective_locale(choice));
        self.lang_note = Some(persist_language(
            settings::default_path().as_deref(),
            choice,
        ));
        self.windows.update(cx, |_, cx| cx.notify());
        self.islands.update(cx, |_, cx| cx.notify());
        self.editor.update(cx, |_, cx| cx.notify());
        self.profiles.update(cx, |_, cx| cx.notify());
        self.diagnose.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// "Start": launch the resident and wait up to 5 s for it to answer.
    fn start_resident(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_failed = spawn_detached(&[]).is_err();
        self.starting = !self.start_failed;
        cx.notify();
        if self.start_failed {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let mut up = false;
            for _ in 0..20 {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                up = cx.background_executor().spawn(async { probe() }).await;
                if up {
                    break;
                }
            }
            this.update_in(cx, |t, window, cx| {
                t.starting = false;
                t.start_failed = !up;
                t.set_connected(up, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ctrl+1..5 pick a section (bubbles up from whatever has focus).
    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if !k.modifiers.control || k.modifiers.alt || k.modifiers.shift {
            return;
        }
        if let Some(i) = k.key.parse::<usize>().ok().filter(|i| (1..=5).contains(i)) {
            self.section = Section::ALL[i - 1];
            cx.notify();
        }
    }

    fn connection_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = Status::of(self.connected, self.starting, self.start_failed);
        let (dot, label) = (status.dot(), status.label());
        let down = self.connected == Some(false);
        div()
            .flex()
            .flex_col()
            .gap(SP_2)
            .p(SP_3)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(SP_2)
                    .text_size(TEXT_MD)
                    .child(
                        div()
                            .mt(px(6.))
                            .size(px(8.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(dot),
                    )
                    .child(div().flex_1().min_w_0().child(label.into_owned())),
            )
            .when(down, |d| {
                d.child(
                    primary_button(
                        "start-resident",
                        if self.starting {
                            i18n::t("gui.status.starting").into_owned()
                        } else {
                            i18n::t("gui.status.start").into_owned()
                        },
                        cx.listener(|t, _, window, cx| t.start_resident(window, cx)),
                    )
                    .disabled(self.starting),
                )
            })
    }

    /// Language selector: System / English / Spanish, each language under its own name.
    fn language_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = [
            ("lang-system", i18n::t("gui.language.system"), None),
            ("lang-en", i18n::t("gui.language.english"), Some(Locale::En)),
            ("lang-es", i18n::t("gui.language.spanish"), Some(Locale::Es)),
        ];
        div()
            .flex()
            .flex_col()
            .gap(SP_2)
            .p(SP_3)
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(i18n::t("gui.language.label").into_owned()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_2)
                    .children(options.into_iter().map(|(id, label, choice)| {
                        let b = secondary_button(
                            id,
                            label.into_owned(),
                            cx.listener(move |t, _, _, cx| t.set_language(choice, cx)),
                        );
                        if self.language == choice {
                            b.icon(IconName::Check)
                        } else {
                            b
                        }
                    })),
            )
            .children(self.lang_note.map(|n| {
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(n.text().into_owned())
            }))
    }
}

impl Render for App {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (font, size) = (theme.font_family.clone(), theme.font_size);
        let status = Status::of(self.connected, self.starting, self.start_failed);
        let nav = Section::ALL
            .iter()
            .fold(div().flex().flex_col().gap(SP_1), |col, &s| {
                col.child(nav_item(
                    s.slug(),
                    s.icon(),
                    s.label().into_owned(),
                    s == self.section,
                    cx.listener(move |t, _, _, cx| {
                        t.section = s;
                        cx.notify();
                    }),
                ))
            });
        let sidebar = div()
            .w(px(232.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .p(SP_2)
            .rounded(R_LG) // = R_SM nav items + SP_2 padding
            .bg(surface())
            .shadow(shadow_soft())
            .child(
                div()
                    .h(px(56.))
                    .px(SP_3)
                    .flex()
                    .items_center()
                    .gap(SP_3)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(div().size(px(12.)).rounded(R_SM).bg(status.dot()))
                    .child(TITLE),
            )
            .child(nav)
            .child(div().flex_1())
            .child(self.language_panel(cx))
            .child(self.connection_panel(cx));

        let view = match self.section {
            Section::Windows => self.windows.clone().into_any_element(),
            Section::Islands => self.islands.clone().into_any_element(),
            Section::IslandEditor => self.editor.clone().into_any_element(),
            Section::Profiles => self.profiles.clone().into_any_element(),
            Section::Diagnose => self.diagnose.clone().into_any_element(),
        };
        div()
            .id("app")
            .size_full()
            .flex()
            .gap(SP_3)
            .p(SP_3)
            .bg(bg())
            .text_color(text())
            .font_family(font)
            .text_size(size)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|t, ev, _, cx| t.on_key(ev, cx)))
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px(SP_8)
                    .py(SP_6)
                    .overflow_hidden()
                    .child(view),
            )
    }
}

/// Take the per-user named mutex of instance `suffix`. `Ok(None)` = another panel already holds it.
fn acquire_single_instance(suffix: Option<&str>) -> Result<Option<HANDLE>> {
    // SAFETY: plain Win32 calls; the handle is kept for the process lifetime (never closed on
    // success) so the mutex disappears only when this process exits.
    unsafe {
        let name = HSTRING::from(mutex_name(suffix));
        let h = CreateMutexW(None, false, &name)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h);
            return Ok(None);
        }
        Ok(Some(h))
    }
}

/// Bring the already-open panel to the front (restoring it if minimised). Retries ~2 s because
/// the other process may still be creating its window.
fn raise_existing_panel(title: &str) {
    let title = HSTRING::from(title);
    for _ in 0..20 {
        // SAFETY: FindWindowW/IsIconic/ShowWindow/SetForegroundWindow are plain calls on a
        // window handle we just looked up; a stale handle only makes them fail harmlessly.
        unsafe {
            if let Ok(hwnd) = FindWindowW(w!("Zed::Window"), &title) {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Entry point of the `gui` subcommand. Blocks until the window closes.
pub fn run() -> Result<()> {
    // A set-but-invalid instance suffix is fatal: never fall back to the real panel's mutex.
    let suffix = instance_suffix()?;
    let title = window_title(suffix.as_deref());
    i18n::init();
    let Some(_single_instance) = acquire_single_instance(suffix.as_deref())? else {
        raise_existing_panel(&title);
        return Ok(());
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            theme::install(cx);
            let opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(760.)),
                    cx,
                ))),
                window_min_size: Some(size(px(960.), px(600.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let opened = gpui_kit::open_window(opts, cx, |window, cx| {
                let app = cx.new(|cx| App::new(window, cx));
                let focus = app.read(cx).focus.clone();
                window.focus(&focus, cx);
                app
            });
            if let Err(e) = opened {
                eprintln!("borderless-shatter: could not open the panel window: {e:#}");
                cx.quit();
                return;
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::ErrorCode;

    #[test]
    fn unset_instance_keeps_the_original_names() {
        assert_eq!(mutex_name(None), "Local\\BorderlessShatter.Gui");
        assert_eq!(window_title(None), "Borderless Shatter");
    }

    #[test]
    fn instance_suffix_isolates_mutex_and_title() {
        assert_eq!(
            mutex_name(Some("gui-a")),
            "Local\\BorderlessShatter.Gui.gui-a"
        );
        assert_eq!(window_title(Some("gui-a")), "Borderless Shatter (gui-a)");
        assert_ne!(mutex_name(Some("gui-a")), mutex_name(Some("gui-b")));
    }

    #[test]
    fn split_keeps_engine_code_apart_from_transport_errors() {
        let remote = CallError::Remote(ErrorBody::new(ErrorCode::Busy, "ocupado"));
        match split::<()>(Err(remote)) {
            Ok(Reply::Remote(b)) => assert_eq!(b.code, ErrorCode::Busy),
            _ => panic!("engine error must become Reply::Remote"),
        }
        assert!(matches!(split(Ok(1)), Ok(Reply::Ok(1))));
        let transport =
            CallError::Ipc(IpcError::from(serde_json::from_str::<u8>("x").unwrap_err()));
        assert!(split::<()>(Err(transport)).is_err());
    }

    /// Fresh empty directory per test (pid + name), removed by the test.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bs-gui-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn language_choice_is_written_to_the_settings_file() {
        let dir = temp_dir("write");
        let path = dir.join("settings.json");
        assert_eq!(
            persist_language(Some(&path), Some(Locale::Es)),
            LangNote::Saved
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"language\": \"es\""), "{text}");
        assert_eq!(persist_language(Some(&path), None), LangNote::Saved);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"system\"")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_or_broken_settings_file_is_refused_and_left_alone() {
        let dir = temp_dir("refuse");
        let path = dir.join("settings.json");
        let future = r#"{"version": 9, "language": "en"}"#;
        std::fs::write(&path, future).unwrap();
        assert_eq!(
            persist_language(Some(&path), Some(Locale::Es)),
            LangNote::Refused
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), future);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            persist_language(Some(&path), Some(Locale::Es)),
            LangNote::Refused
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_without_a_settings_path_reports_a_failure() {
        assert_eq!(persist_language(None, Some(Locale::En)), LangNote::Failed);
    }

    #[test]
    fn an_explicit_language_wins_over_the_os_language() {
        assert_eq!(effective_locale(Some(Locale::Es)), Locale::Es);
        assert_eq!(effective_locale(Some(Locale::En)), Locale::En);
    }

    #[test]
    fn every_save_note_has_text_in_both_languages() {
        for l in [Locale::En, Locale::Es] {
            let _g = i18n::scoped_locale(l);
            for n in [LangNote::Saved, LangNote::Refused, LangNote::Failed] {
                assert!(n.text().contains(' '), "{l:?} {n:?}: {}", n.text());
            }
        }
    }

    #[test]
    fn section_names_follow_the_locale() {
        let names = |l| {
            let _g = i18n::scoped_locale(l);
            Section::ALL.map(|s| s.label().into_owned())
        };
        assert_eq!(
            names(Locale::En),
            [
                "Windows",
                "Islands",
                "Island editor",
                "Profiles",
                "Diagnostics"
            ]
        );
        assert_ne!(names(Locale::Es), names(Locale::En));
    }

    #[test]
    fn presets_get_their_catalog_name_and_user_layouts_keep_their_id() {
        let _g = i18n::scoped_locale(Locale::En);
        assert_eq!(layout_label("halves-16-9"), "Halves 16:9");
        assert_eq!(layout_label("my-layout"), "my-layout");
        i18n::set_locale(Locale::Es);
        assert_eq!(layout_label("halves-16-9"), "Mitades 16:9");
    }

    #[test]
    fn every_error_code_reads_as_a_plain_sentence_with_its_code() {
        use ErrorCode::*;
        let _g = i18n::scoped_locale(Locale::En);
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
        for c in all {
            let (s, code) = failure_text(&Failure::Engine(c));
            assert!(s.ends_with('.') && s.chars().count() > 12, "{c:?}: {s}");
            assert!(!s.contains("0x") && !s.contains("os error"), "{c:?}: {s}");
            assert_eq!(code, serde_json::to_value(c).unwrap().as_str().unwrap());
        }
    }

    #[test]
    fn resident_down_and_transport_failures_have_plain_sentences() {
        let _g = i18n::scoped_locale(Locale::En);
        let (down, code) = failure_text(&Failure::Down);
        assert!(down.contains("not running"), "{down}");
        assert_eq!(code, "resident_unreachable");
        assert_eq!(failure_text(&Failure::Transport).1, "transport_error");
        i18n::set_locale(Locale::Es);
        assert_ne!(failure_text(&Failure::Down).0, down);
        let m = failure_msg(&Failure::Engine(ErrorCode::WindowGone));
        assert_eq!(m.text, i18n::t("common.error.window_gone"));
        assert_eq!(
            m.detail,
            [i18n::tf("gui.error.code", &[("code", &"window_gone")])]
        );
    }

    #[test]
    fn call_errors_never_show_raw_os_text() {
        let _g = i18n::scoped_locale(Locale::En);
        // OS-localized text as Windows puts it into io::Error and the engine into `message`.
        let os = "El sistema no puede encontrar el archivo especificado. (os error 2)";
        let ipc = CallError::Ipc(IpcError::Connect(os.to_owned()));
        let io = CallError::Ipc(IpcError::Io(std::io::Error::other(os)));
        let remote = CallError::Remote(ErrorBody {
            code: ErrorCode::Win32Error,
            message: os.to_owned(),
            data: None,
        });
        for e in [ipc, io, remote] {
            let text = call_error_text(&e);
            assert!(
                !text.contains("sistema") && !text.contains("os error"),
                "{text}"
            );
        }
    }

    #[test]
    fn one_missed_probe_keeps_a_connected_panel_up_and_two_bring_it_down() {
        assert_eq!(settle(Some(true), 0, false), (true, 1));
        assert_eq!(settle(Some(true), 1, false), (false, 2));
        assert_eq!(settle(Some(true), 1, true), (true, 0));
        // Never connected: no grace, the first failure shows "down".
        assert_eq!(settle(None, 0, false), (false, 1));
        assert_eq!(settle(Some(false), 0, false), (false, 1));
    }

    #[test]
    fn status_dot_follows_the_connection_not_the_accent() {
        assert_eq!(Status::of(Some(true), false, false).dot(), theme::ok());
        assert_eq!(Status::of(Some(false), false, false).dot(), theme::warn());
        assert_ne!(Status::of(Some(false), false, false).dot(), theme::accent());
        assert_eq!(Status::of(Some(true), true, false), Status::Starting);
        assert_eq!(Status::of(None, false, true), Status::StartFailed);
        assert_eq!(Status::of(None, false, false), Status::Checking);
    }

    #[test]
    fn a_new_load_failure_clears_old_error_notices_but_keeps_good_news() {
        let msg = |kind| Msg {
            kind,
            text: String::new(),
            detail: Vec::new(),
        };
        assert!(without_stale_error(Some(msg(BadgeKind::Error))).is_none());
        assert!(without_stale_error(Some(msg(BadgeKind::Ok))).is_some());
        assert!(without_stale_error(None).is_none());
    }

    #[test]
    fn code_str_is_snake_case() {
        assert_eq!(code_str(ErrorCode::RevisionConflict), "revision_conflict");
    }
}
