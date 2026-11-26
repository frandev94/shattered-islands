//! Island editor: edits the user's island layouts on a to-scale preview of the desktop.
//! The pure half (drag / keyboard / undo state machine, conflict outcomes, save gate, error
//! texts) is at the top and unit-tested; geometry, validation and the `set_profiles` payload
//! live in `editor_model`; the GPUI half below renders and talks IPC.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use gpui_kit::component::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, CursorStyle, DispatchPhase, Div, ElementId, Entity,
    FocusHandle, Focusable as _, FontWeight, HitboxBehavior, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, px,
};
use serde_json::json;

use super::editor_model::{
    Field, FieldError, Guides, PxRect, SnapTargets, aspect_warnings, build_payload, duplicate_id,
    field_edit, fit_16_9, is_16_9, island_px, move_rect, overlaps, profile_refs, rebase,
    rect_from_px, resize_rect, round6, snap_targets, validate,
};
use super::monitor_preview::{
    DesktopLayout, Handle, Hit, MonitorPreview, PRect, PreviewIsland, PreviewMonitor,
    desktop_layout, format_size, fraction_label, pick, preview_max_width, snap_tolerance_phys,
};
use super::profiles_view::warning_text;
use super::theme::{
    BadgeKind, Btn, HIT, R_SM, SP_1, SP_2, SP_3, SP_4, TEXT_MD, TEXT_SM, accent, badge, bg, card,
    danger, empty_state, primary_button, raised, secondary_button, section_header, surface,
    tabular, text, text_muted, warn,
};
use super::{Reply, ipc_call, json_err, remote_message, split};
use crate::i18n::{self, Locale, fmt_decimal, t, tf};
use crate::ipc::pipe::Client;
use crate::ipc::{
    CallError, ErrorBody, ErrorCode, GetProfilesResult, ListIslandsResult, SetProfilesParams,
    SetProfilesResult,
};
use crate::islands::{
    Island, IslandRect, Layout, MonitorInfo, MonitorSel, Unit, island_rect_on, presets,
    select_monitor,
};
use crate::profiles::{Profile, Warning};
use crate::window_state::Rect;

// =================================================================== pure logic

/// Undo depth (each step is a snapshot of the whole draft).
pub const UNDO_LIMIT: usize = 100;

/// Undo / redo of the draft. Every edit stores the draft as it was just before it.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Vec<Layout>>,
    redo: Vec<Vec<Layout>>,
}

impl History {
    /// Remember `before` (the draft just before an edit); a new edit drops the redo branch.
    pub fn record(&mut self, before: Vec<Layout>) {
        self.undo.push(before);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Closes an edit that already touched the draft (a drag, a field): records `before` only
    /// when the draft really changed. Returns whether it did.
    pub fn finish(&mut self, before: Vec<Layout>, draft: &[Layout]) -> bool {
        let changed = before != draft;
        if changed {
            self.record(before);
        }
        changed
    }

    pub fn undo(&mut self, draft: &mut Vec<Layout>) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(draft, prev));
        true
    }

    pub fn redo(&mut self, draft: &mut Vec<Layout>) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(draft, next));
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Rect of an island being dragged: the pointer moved from `origin` to `cur` (monitor-relative
/// physical px); the body moves the island, a handle resizes it. `snap: None` = Alt held or
/// snapping off.
pub fn drag_target(
    hit: Hit,
    start: PxRect,
    origin: (f64, f64),
    cur: (f64, f64),
    mw: i32,
    mh: i32,
    snap: Option<(&SnapTargets, f64)>,
) -> (PxRect, Guides) {
    // `as` saturates, so a pointer far outside the window cannot overflow.
    let (dx, dy) = (
        (cur.0 - origin.0).round() as i32,
        (cur.1 - origin.1).round() as i32,
    );
    match hit {
        Hit::Body => move_rect(start, dx, dy, mw, mh, snap),
        Hit::Handle(h) => resize_rect(start, h, dx, dy, mw, mh, snap),
    }
}

/// What a key does on the focused preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Move(i32, i32),
    /// Width / height change (the right and bottom edges move).
    Resize(i32, i32),
    Delete,
    Undo,
    Redo,
}

/// Arrows move 1 px (Shift: 10), Alt+arrows resize, Delete/Backspace delete, Ctrl+Z undo,
/// Ctrl+Shift+Z / Ctrl+Y redo. Anything else (Ctrl+1..5 included) is left to the shell.
pub fn key_action(key: &str, ctrl: bool, shift: bool, alt: bool) -> Option<KeyAction> {
    let step = if shift { 10 } else { 1 };
    let dir = match key {
        "left" => Some((-step, 0)),
        "right" => Some((step, 0)),
        "up" => Some((0, -step)),
        "down" => Some((0, step)),
        _ => None,
    };
    if let Some((x, y)) = dir {
        return (!ctrl).then_some(if alt {
            KeyAction::Resize(x, y)
        } else {
            KeyAction::Move(x, y)
        });
    }
    match (key, ctrl, shift) {
        ("delete" | "backspace", false, _) => Some(KeyAction::Delete),
        ("z", true, false) => Some(KeyAction::Undo),
        ("z", true, true) | ("y", true, false) => Some(KeyAction::Redo),
        _ => None,
    }
}

/// Applies a move / resize key to an island (no snapping; kept inside the monitor, min 64).
pub fn nudge(r: PxRect, a: KeyAction, mw: i32, mh: i32) -> PxRect {
    match a {
        KeyAction::Move(dx, dy) => move_rect(r, dx, dy, mw, mh, None).0,
        KeyAction::Resize(dw, dh) => resize_rect(r, Handle::SE, dw, dh, mw, mh, None).0,
        _ => r,
    }
}

/// How a fresh `get_profiles` meets the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reload {
    /// No draft to protect, or "Discard my changes": the draft becomes the file.
    Replace,
    /// "Reload and review" after a `revision_conflict`: the user's edits survive.
    Rebase,
}

/// New draft and the layout ids to flag (kept user version over an outside change).
pub fn reload_draft(
    mode: Reload,
    old_saved: &[Layout],
    new_saved: &[Layout],
    draft: &[Layout],
) -> (Vec<Layout>, Vec<String>) {
    match mode {
        Reload::Replace => (new_saved.to_vec(), Vec::new()),
        Reload::Rebase => rebase(old_saved, new_saved, draft),
    }
}

/// `busy` after a `set_profiles` answer: a successful save stays busy until the reload it
/// starts lands, so nothing typed in between can be dropped by `Reload::Replace`.
pub fn busy_after_save(saved_ok: bool) -> bool {
    saved_ok
}

/// `busy` after a `get_profiles` answer: the post-save reload (`replace`) ends the save.
pub fn busy_after_load(busy: bool, replace: bool) -> bool {
    busy && !replace
}

/// Help line under the preview. A conflict pauses the user's own draft: that is not "read-only".
pub fn help_key(editable: bool, conflict: bool, busy: bool) -> &'static str {
    if editable || busy {
        "editor.help_edit"
    } else if conflict {
        "editor.help_paused"
    } else {
        "editor.help_readonly"
    }
}

/// Why "Save layout" is disabled (first matching reason), or `None` when it can save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveGate {
    pub dirty: bool,
    pub errors: bool,
    pub readonly: bool,
    pub busy: bool,
    pub conflict: bool,
}

impl SaveGate {
    pub fn blocked(&self) -> Option<Cow<'static, str>> {
        let key = if self.readonly {
            "editor.gate.readonly"
        } else if self.conflict {
            "editor.gate.conflict"
        } else if self.busy {
            "editor.gate.busy"
        } else if self.errors {
            "editor.gate.errors"
        } else if !self.dirty {
            "editor.gate.clean"
        } else {
            return None;
        };
        Some(t(key))
    }
}

/// Layout shown in the editor: a draft layout (editable) or a preset not shadowed by a draft
/// layout with its id (read-only; index into `presets()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Draft(usize),
    Preset(usize),
}

/// Presets still visible next to `draft` (a user layout with the same id replaces it).
pub fn visible_presets(draft: &[Layout]) -> Vec<usize> {
    presets()
        .iter()
        .enumerate()
        .filter(|(_, p)| !draft.iter().any(|d| d.id == p.id))
        .map(|(i, _)| i)
        .collect()
}

/// First draft layout, else the first preset ("Halves 16:9" on the primary monitor).
pub fn default_pick(draft: &[Layout]) -> Option<Pick> {
    if draft.is_empty() {
        visible_presets(draft).first().map(|&i| Pick::Preset(i))
    } else {
        Some(Pick::Draft(0))
    }
}

pub fn pick_by_id(draft: &[Layout], id: &str) -> Option<Pick> {
    draft
        .iter()
        .position(|l| l.id == id)
        .map(Pick::Draft)
        .or_else(|| {
            let all = presets();
            visible_presets(draft)
                .into_iter()
                .find(|&i| all[i].id == id)
                .map(Pick::Preset)
        })
}

/// `<prefix>-<n>` with the lowest `n ≥ 1` not in `taken` (ignoring case).
pub fn next_id(prefix: &str, taken: &[&str]) -> String {
    let free = |c: &str| !taken.iter().any(|t| t.to_lowercase() == c.to_lowercase());
    // Among taken.len() + 1 candidates at least one is free.
    (1..=taken.len() + 1)
        .map(|n| format!("{prefix}-{n}"))
        .find(|c| free(c))
        .unwrap_or_else(|| format!("{prefix}-{}", taken.len() + 1))
}

/// Island created by a double-click: 25 % × 50 % of the monitor centred on the click, kept
/// inside the monitor.
pub fn new_island_rect(click: (i32, i32), mw: i32, mh: i32) -> PxRect {
    let (w, h) = (
        (f64::from(mw) / 4.0).round() as i32,
        (f64::from(mh) / 2.0).round() as i32,
    );
    let r = PxRect {
        x: click.0 - w / 2,
        y: click.1 - h / 2,
        w,
        h,
    };
    move_rect(r, 0, 0, mw, mh, None).0
}

/// Pointer over the preview: what is under it (or being dragged).
pub fn cursor_for(hit: Option<Hit>, dragging: bool) -> CursorStyle {
    use Handle::*;
    match hit {
        None => CursorStyle::Arrow,
        Some(Hit::Body) if dragging => CursorStyle::ClosedHand,
        Some(Hit::Body) => CursorStyle::OpenHand,
        Some(Hit::Handle(N | S)) => CursorStyle::ResizeUpDown,
        Some(Hit::Handle(E | W)) => CursorStyle::ResizeLeftRight,
        Some(Hit::Handle(NW | SE)) => CursorStyle::ResizeUpLeftDownRight,
        Some(Hit::Handle(NE | SW)) => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// A failure as shown to the user: one plain sentence plus the small `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub text: String,
    pub code: String,
}

fn code_str(c: ErrorCode) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The engine refuses to write this file (invalid JSON or a newer version): the editor turns read-only.
pub fn is_readonly(b: &ErrorBody) -> bool {
    matches!(
        b.code,
        ErrorCode::ProfileError | ErrorCode::UnsupportedVersion
    )
}

/// Engine error → text. File errors get our own sentence (their message carries raw parser
/// or I/O detail); rejected warnings are shown as such; the rest use the sentence for the code.
pub fn remote_problem(b: &ErrorBody) -> Problem {
    let has_warnings = b.data.as_ref().is_some_and(|d| d.get("warnings").is_some());
    let text = match b.code {
        ErrorCode::ProfileError => t("editor.problem.profile_error").into_owned(),
        ErrorCode::UnsupportedVersion => t("editor.problem.unsupported_version").into_owned(),
        _ if has_warnings => remote_message(b),
        c => i18n::error_sentence(c).into_owned(),
    };
    Problem {
        text,
        code: code_str(b.code),
    }
}

/// Any transport failure (resident not running, broken pipe): never the raw OS error.
pub fn transport_problem() -> Problem {
    Problem {
        text: t("editor.problem.transport").into_owned(),
        code: "resident_unreachable".to_owned(),
    }
}

// ======================================================================== IPC glue

struct Loaded {
    monitors: Vec<MonitorInfo>,
    file: Reply<GetProfilesResult>,
}

fn load(c: &mut Client) -> Result<Loaded, CallError> {
    let islands = c.call("list_islands", json!({}))?;
    let monitors = serde_json::from_value::<ListIslandsResult>(islands)
        .map_err(json_err)?
        .monitors;
    let file = match split(c.call("get_profiles", json!({})))? {
        Reply::Ok(v) => Reply::Ok(serde_json::from_value(v).map_err(json_err)?),
        Reply::Remote(b) => Reply::Remote(b),
    };
    Ok(Loaded { monitors, file })
}

fn save_call(
    c: &mut Client,
    params: SetProfilesParams,
) -> Result<Reply<SetProfilesResult>, CallError> {
    let v = serde_json::to_value(params).map_err(json_err)?;
    Ok(match split(c.call("set_profiles", v))? {
        Reply::Ok(v) => Reply::Ok(serde_json::from_value(v).map_err(json_err)?),
        Reply::Remote(b) => Reply::Remote(b),
    })
}

// ===================================================================== the view

/// Half side of a resize handle in preview px (16 px squares).
const HANDLE_HALF: f32 = 8.0;
/// Snap distance: 8 physical px of the panel's screen ("8 px of screen").
const SNAP_SCREEN_PX: f64 = 8.0;
const PREVIEW_MAX_H: f32 = 380.0;
/// The poll runs every 5 s while the view rendered recently: a view off screen is not
/// rendered, a visible one re-renders after each poll, so 7 s means "visible".
const POLL: Duration = Duration::from_secs(5);
const VISIBLE: Duration = Duration::from_secs(7);

const ZERO_RECT: IslandRect = IslandRect {
    x: 0.0,
    y: 0.0,
    w: 0.0,
    h: 0.0,
    unit: Unit::Fraction,
};

/// The 10 synced text fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inp {
    LayoutId,
    IslandId,
    Num(Field),
}

const INPUTS: [Inp; 10] = [
    Inp::LayoutId,
    Inp::IslandId,
    Inp::Num(Field::PxX),
    Inp::Num(Field::PxY),
    Inp::Num(Field::PxW),
    Inp::Num(Field::PxH),
    Inp::Num(Field::FrX),
    Inp::Num(Field::FrY),
    Inp::Num(Field::FrW),
    Inp::Num(Field::FrH),
];

fn slot(inp: Inp) -> usize {
    INPUTS.iter().position(|i| *i == inp).unwrap_or(0)
}

fn placeholder(inp: Inp) -> String {
    match inp {
        Inp::LayoutId => t("editor.ph.layout_id").into_owned(),
        Inp::IslandId => t("editor.ph.island_id").into_owned(),
        Inp::Num(Field::FrX | Field::FrY | Field::FrW | Field::FrH) => fmt_decimal(0.5, 1),
        Inp::Num(_) => "0".to_owned(),
    }
}

fn tr(key: &str) -> SharedString {
    SharedString::from(t(key).into_owned())
}

struct FileData {
    path: String,
    revision: String,
    profiles: Vec<Profile>,
    saved: Vec<Layout>,
}

struct Drag {
    island: usize,
    hit: Hit,
    start_px: PxRect,
    start_rect: IslandRect,
    /// Pointer at mouse-down, monitor-relative physical px.
    origin: (f64, f64),
    before: Vec<Layout>,
}

/// Preview geometry of the last render, read by the mouse listeners.
#[derive(Clone)]
struct Geom {
    layout: DesktopLayout,
    /// Desktop rect of the picked layout's monitor.
    monitor: Option<Rect>,
    islands: Vec<PRect>,
    editable: bool,
}

pub struct EditorView {
    presets: Vec<Layout>,
    monitors: Vec<MonitorInfo>,
    file: Option<FileData>,
    draft: Vec<Layout>,
    pick: Option<Pick>,
    sel: Option<usize>,
    hist: History,
    snap: bool,
    grid: bool,
    drag: Option<Drag>,
    /// Monitor-relative snap guides of the running drag.
    guides: Guides,
    hover: Option<Hit>,
    geom: Option<Geom>,
    loading: bool,
    busy: bool,
    load_error: Option<Problem>,
    /// `profile_error` / `unsupported_version`: nothing can be saved, the file on disk is kept as is.
    readonly: Option<Problem>,
    save_error: Option<Problem>,
    notice: Option<String>,
    save_warnings: Vec<Warning>,
    conflict: bool,
    /// Layouts whose user version was kept over an outside change (after a rebase).
    outside: Vec<String>,
    field_error: Option<(Inp, FieldError)>,
    /// Field with focus and the pick/island it edits.
    editing: Option<(Inp, Option<Pick>, Option<usize>)>,
    /// `select_layout` before the first load.
    pending: Option<Option<String>>,
    focus: FocusHandle,
    inputs: [Entity<InputState>; 10],
    _subs: Vec<Subscription>,
    last_render: Instant,
    locale: Locale,
}

impl EditorView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        let inputs = INPUTS.map(|inp| {
            let st = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder(inp)));
            subs.push(cx.subscribe_in(
                &st,
                window,
                move |t: &mut Self, _, e: &InputEvent, window, cx| t.on_input(inp, e, window, cx),
            ));
            st
        });
        let mut v = Self {
            presets: presets(),
            monitors: Vec::new(),
            file: None,
            draft: Vec::new(),
            pick: None,
            sel: None,
            hist: History::default(),
            snap: true,
            grid: false,
            drag: None,
            guides: (None, None),
            hover: None,
            geom: None,
            loading: false,
            busy: false,
            load_error: None,
            readonly: None,
            save_error: None,
            notice: None,
            save_warnings: Vec::new(),
            conflict: false,
            outside: Vec::new(),
            field_error: None,
            editing: None,
            pending: None,
            focus: cx.focus_handle(),
            inputs,
            _subs: subs,
            last_render: Instant::now(),
            locale: i18n::locale(),
        };
        v.reload(false, window, cx);
        // Poll only with no draft to protect, nothing in flight and the window active.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(due) = this.update_in(cx, |t, window, _| t.poll_due(window)) else {
                    break;
                };
                if due {
                    this.update_in(cx, |t, window, cx| t.reload(false, window, cx))
                        .ok();
                }
            }
        })
        .detach();
        v
    }

    /// Reload from the file (after a reconnect); a dirty draft is rebased, never dropped.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload(false, window, cx);
    }

    /// Hook for the Islas view: open layout `id`, or start a new design with `None`.
    pub fn select_layout(&mut self, id: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        if self.file.is_none() {
            self.pending = Some(id.map(str::to_owned));
            return;
        }
        match id {
            None => self.new_design(window, cx),
            Some(id) => {
                if let Some(p) = pick_by_id(&self.draft, id) {
                    self.pick = Some(p);
                    self.sel = None;
                }
                self.refresh_ui(window, cx);
            }
        }
    }

    // ------------------------------------------------------------ state queries

    fn dirty(&self) -> bool {
        self.file.as_ref().is_some_and(|f| f.saved != self.draft)
    }

    fn layout(&self) -> Option<&Layout> {
        match self.pick? {
            Pick::Draft(i) => self.draft.get(i),
            Pick::Preset(j) => self.presets.get(j),
        }
    }

    fn draft_index(&self) -> Option<usize> {
        match self.pick {
            Some(Pick::Draft(i)) if i < self.draft.len() => Some(i),
            _ => None,
        }
    }

    fn size_of(&self, l: &Layout) -> Option<(i32, i32)> {
        select_monitor(&l.monitor, &self.monitors)
            .ok()
            .map(|m| (m.rect.width(), m.rect.height()))
    }

    /// The file may be changed now (layout ids, monitor, add/delete).
    fn can_edit_file(&self) -> bool {
        self.readonly.is_none() && !self.conflict && !self.busy && self.file.is_some()
    }

    /// Island geometry of the picked layout may be edited (draft layout, monitor connected).
    fn editable(&self) -> bool {
        self.can_edit_file()
            && self
                .draft_index()
                .and_then(|i| self.size_of(&self.draft[i]))
                .is_some()
    }

    fn poll_due(&self, window: &Window) -> bool {
        !self.dirty()
            && !self.busy
            && !self.loading
            && !self.conflict
            && self.drag.is_none()
            && window.is_window_active()
            && self.last_render.elapsed() < VISIBLE
    }

    fn all_ids(&self) -> Vec<String> {
        self.draft
            .iter()
            .chain(&self.presets)
            .map(|l| l.id.clone())
            .collect()
    }

    /// Current text of a field for the picked layout / island.
    fn text_for(&self, inp: Inp) -> String {
        let Some(l) = self.layout() else {
            return String::new();
        };
        let isl = self.sel.and_then(|s| l.islands.get(s));
        match inp {
            Inp::LayoutId => l.id.clone(),
            Inp::IslandId => isl.map(|i| i.id.clone()).unwrap_or_default(),
            Inp::Num(f) => {
                let (Some(isl), Some((mw, mh))) = (isl, self.size_of(l)) else {
                    return String::new();
                };
                let p = island_px(&isl.rect, mw, mh);
                // Same rect for fractions; px-unit islands shown as their fractions.
                let r = rect_from_px(&isl.rect, p, mw, mh);
                let sep = t("common.decimal_separator");
                let fr = |v: f64| round6(v).to_string().replace('.', &sep);
                match f {
                    Field::PxX => p.x.to_string(),
                    Field::PxY => p.y.to_string(),
                    Field::PxW => p.w.to_string(),
                    Field::PxH => p.h.to_string(),
                    Field::FrX => fr(r.x),
                    Field::FrY => fr(r.y),
                    Field::FrW => fr(r.w),
                    Field::FrH => fr(r.h),
                }
            }
        }
    }

    // ------------------------------------------------------------ edits

    /// Applies `f` to the picked draft layout (with its monitor size); records undo when it
    /// changed something.
    fn edit_layout(&mut self, f: impl FnOnce(&mut Layout, Option<(i32, i32)>)) -> bool {
        if !self.can_edit_file() {
            return false;
        }
        let Some(i) = self.draft_index() else {
            return false;
        };
        let size = self.size_of(&self.draft[i]);
        let before = self.draft.clone();
        f(&mut self.draft[i], size);
        self.hist.finish(before, &self.draft)
    }

    /// Moves / resizes the selected island in px (its untouched fractions are kept).
    fn edit_rect(&mut self, f: impl FnOnce(PxRect, i32, i32) -> PxRect) -> bool {
        let Some(s) = self.sel else {
            return false;
        };
        self.edit_layout(|l, size| {
            if let (Some((mw, mh)), Some(isl)) = (size, l.islands.get_mut(s)) {
                let target = f(island_px(&isl.rect, mw, mh), mw, mh);
                isl.rect = rect_from_px(&isl.rect, target, mw, mh);
            }
        })
    }

    fn fix_pick(&mut self) {
        let valid = match self.pick {
            Some(Pick::Draft(i)) => i < self.draft.len(),
            Some(Pick::Preset(j)) => visible_presets(&self.draft).contains(&j),
            None => false,
        };
        if !valid {
            self.pick = default_pick(&self.draft);
            self.sel = None;
        }
        let n = self.layout().map_or(0, |l| l.islands.len());
        if self.sel.is_some_and(|s| s >= n) {
            self.sel = None;
        }
    }

    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let texts = INPUTS.map(|inp| self.text_for(inp));
        for (st, text) in self.inputs.iter().zip(texts) {
            st.update(cx, |s, cx| {
                // Never overwrite what the user is typing.
                if !s.focus_handle(cx).is_focused(window) && s.value().as_ref() != text {
                    s.set_value(text, window, cx);
                }
            });
        }
    }

    /// Selection / pick changed: fields follow, nothing else.
    fn refresh_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fix_pick();
        self.sync_inputs(window, cx);
        cx.notify();
    }

    /// The draft changed.
    fn after_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.field_error = None;
        self.notice = None;
        self.refresh_ui(window, cx);
    }

    fn on_input(&mut self, inp: Inp, e: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e {
            InputEvent::Focus => self.editing = Some((inp, self.pick, self.sel)),
            InputEvent::PressEnter { .. } => self.commit(inp, window, cx),
            InputEvent::Blur => self.flush_editing(window, cx),
            InputEvent::Change => {}
        }
    }

    /// Commits the focused field if it still edits the same island (a click elsewhere first
    /// commits, then selects).
    fn flush_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((inp, p, s)) = self.editing.take()
            && (p, s) == (self.pick, self.sel)
        {
            self.commit(inp, window, cx);
        }
    }

    fn commit(&mut self, inp: Inp, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.inputs[slot(inp)].read(cx).value().to_string();
        if text == self.text_for(inp) {
            return;
        }
        let changed = match inp {
            Inp::LayoutId => {
                let t = text.trim().to_owned();
                self.edit_layout(|l, _| l.id = t)
            }
            Inp::IslandId => {
                let (Some(s), t) = (self.sel, text.trim().to_owned()) else {
                    return;
                };
                self.edit_layout(|l, _| {
                    if let Some(i) = l.islands.get_mut(s) {
                        i.id = t;
                    }
                })
            }
            Inp::Num(f) => {
                let Some(s) = self.sel else {
                    return;
                };
                let mut result = None;
                self.edit_layout(|l, size| {
                    if let (Some((mw, mh)), Some(isl)) = (size, l.islands.get_mut(s)) {
                        match field_edit(&isl.rect, f, &text, mw, mh) {
                            Ok(r) => isl.rect = r,
                            Err(e) => result = Some(e),
                        }
                    }
                });
                if let Some(m) = result {
                    self.field_error = Some((inp, m));
                    cx.notify();
                    return;
                }
                true
            }
        };
        if changed || self.field_error.is_some() {
            self.after_change(window, cx);
        }
    }

    fn new_design(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit_file() {
            return;
        }
        let taken = self.all_ids();
        let id = next_id(
            &t("editor.prefix.layout"),
            &taken.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        self.hist.record(self.draft.clone());
        self.draft.push(Layout {
            id,
            monitor: MonitorSel::Primary,
            islands: Vec::new(),
            extra: Default::default(),
        });
        self.pick = Some(Pick::Draft(self.draft.len() - 1));
        self.sel = None;
        self.after_change(window, cx);
    }

    /// "Duplicate to edit": a preset becomes an editable `<id>-copy`.
    fn duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(src) = self.layout().cloned() else {
            return;
        };
        if !self.can_edit_file() {
            return;
        }
        let taken = self.all_ids();
        let id = duplicate_id(
            &src.id,
            &taken.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        self.hist.record(self.draft.clone());
        self.draft.push(Layout { id, ..src });
        self.pick = Some(Pick::Draft(self.draft.len() - 1));
        self.sel = None;
        self.after_change(window, cx);
    }

    /// "Delete layout": gone from the draft now, from the file on save (`remove_layouts`).
    fn delete_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.draft_index().filter(|_| self.can_edit_file()) else {
            return;
        };
        self.hist.record(self.draft.clone());
        let gone = self.draft.remove(i);
        self.pick = default_pick(&self.draft);
        self.sel = None;
        self.after_change(window, cx);
        self.notice = Some(tf("editor.notice.layout_removed", &[("id", &gone.id)]));
    }

    fn add_island(&mut self, at: Option<(i32, i32)>) -> bool {
        let mut ids: Vec<String> = Vec::new();
        if let Some(l) = self.layout() {
            ids = l.islands.iter().map(|i| i.id.clone()).collect();
        }
        let id = next_id(
            &t("editor.prefix.island"),
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let added = self.edit_layout(|l, size| {
            if let Some((mw, mh)) = size {
                let target = new_island_rect(at.unwrap_or((mw / 2, mh / 2)), mw, mh);
                l.islands.push(Island {
                    id,
                    rect: rect_from_px(&ZERO_RECT, target, mw, mh),
                    extra: Default::default(),
                });
            }
        });
        if added {
            self.sel = self.layout().and_then(|l| l.islands.len().checked_sub(1));
        }
        added
    }

    fn delete_island(&mut self) -> bool {
        let Some(s) = self.sel else {
            return false;
        };
        let done = self.edit_layout(|l, _| {
            if s < l.islands.len() {
                l.islands.remove(s);
            }
        });
        if done {
            self.sel = None;
        }
        done
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.file.as_ref().map(|f| f.saved.clone()) else {
            return;
        };
        if self.draft != saved {
            // Undoable: "Discard changes" is one more step in the history.
            self.hist.record(std::mem::replace(&mut self.draft, saved));
            self.outside.clear();
            self.after_change(window, cx);
        }
    }

    fn undo_redo(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let done = self.can_edit_file()
            && if redo {
                self.hist.redo(&mut self.draft)
            } else {
                self.hist.undo(&mut self.draft)
            };
        if done {
            self.after_change(window, cx);
        }
    }

    // ------------------------------------------------------------ mouse and keys

    /// Pointer in preview px and in physical px relative to the picked layout's monitor.
    fn points(g: &Geom, pos: Point<Pixels>, origin: Point<Pixels>) -> ((f32, f32), (f64, f64)) {
        let p = (f32::from(pos.x - origin.x), f32::from(pos.y - origin.y));
        let (x, y) = g.layout.point_to_phys(p.0, p.1);
        let m = g.monitor.unwrap_or_default();
        (p, (x - f64::from(m.left), y - f64::from(m.top)))
    }

    fn mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flush_editing(window, cx);
        window.focus(&self.focus, cx);
        let Some(g) = self.geom.clone().filter(|g| g.editable) else {
            return;
        };
        let Some(m) = g.monitor else {
            return;
        };
        let (mw, mh) = (m.width(), m.height());
        let (p, rel) = Self::points(&g, ev.position, origin);
        match pick(&g.islands, self.sel, p, HANDLE_HALF) {
            Some((i, hit)) if ev.click_count < 2 => {
                let Some(isl) = self.layout().and_then(|l| l.islands.get(i)) else {
                    return;
                };
                self.drag = Some(Drag {
                    island: i,
                    hit,
                    start_px: island_px(&isl.rect, mw, mh),
                    start_rect: isl.rect,
                    origin: rel,
                    before: self.draft.clone(),
                });
                self.sel = Some(i);
            }
            Some((i, _)) => self.sel = Some(i),
            None if ev.click_count >= 2
                && rel.0 >= 0.0
                && rel.1 >= 0.0
                && rel.0 < f64::from(mw)
                && rel.1 < f64::from(mh) =>
            {
                if self.add_island(Some((rel.0 as i32, rel.1 as i32))) {
                    self.after_change(window, cx);
                    return;
                }
            }
            None => self.sel = None,
        }
        self.refresh_ui(window, cx);
    }

    fn mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        origin: Point<Pixels>,
        hovered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(g) = self.geom.clone() else {
            return;
        };
        let (p, rel) = Self::points(&g, ev.position, origin);
        let Some(d) = &self.drag else {
            let hover = (hovered && g.editable)
                .then(|| pick(&g.islands, self.sel, p, HANDLE_HALF).map(|(_, h)| h))
                .flatten();
            if hover != self.hover {
                self.hover = hover;
                cx.notify();
            }
            return;
        };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag(window, cx);
            return;
        }
        let (Some(m), Some(li)) = (g.monitor, self.draft_index()) else {
            return;
        };
        let (mw, mh) = (m.width(), m.height());
        let (island, hit, start_px, start_rect, from) =
            (d.island, d.hit, d.start_px, d.start_rect, d.origin);
        let others: Vec<PxRect> = self.draft[li]
            .islands
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != island)
            .map(|(_, x)| island_px(&x.rect, mw, mh))
            .collect();
        let targets = snap_targets(mw, mh, &others);
        let tol = snap_tolerance_phys(
            SNAP_SCREEN_PX,
            f64::from(window.scale_factor()),
            g.layout.scale,
        );
        let snap = (self.snap && !ev.modifiers.alt).then_some((&targets, tol));
        let (r, guides) = drag_target(hit, start_px, from, rel, mw, mh, snap);
        if let Some(isl) = self.draft[li].islands.get_mut(island) {
            isl.rect = rect_from_px(&start_rect, r, mw, mh);
        }
        self.guides = guides;
        self.sync_inputs(window, cx);
        cx.notify();
    }

    fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else {
            return;
        };
        self.guides = (None, None);
        if self.hist.finish(d.before, &self.draft) {
            self.after_change(window, cx);
        } else {
            self.refresh_ui(window, cx);
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        let Some(a) = key_action(&k.key, m.control, m.shift, m.alt) else {
            return;
        };
        cx.stop_propagation();
        if self.drag.is_some() {
            return;
        }
        match a {
            KeyAction::Undo => self.undo_redo(false, window, cx),
            KeyAction::Redo => self.undo_redo(true, window, cx),
            KeyAction::Delete => {
                if self.delete_island() {
                    self.after_change(window, cx);
                }
            }
            _ => {
                if self.editable() && self.edit_rect(|r, mw, mh| nudge(r, a, mw, mh)) {
                    self.after_change(window, cx);
                }
            }
        }
    }

    // ------------------------------------------------------------ IPC

    /// `replace`: the draft becomes the file (after a save, "Discard my changes").
    /// Otherwise a dirty draft is rebased onto the file, never dropped.
    fn reload(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let r = ipc_call(cx, load).await;
            this.update_in(cx, |t, window, cx| t.loaded(r, replace, window, cx))
                .ok();
        })
        .detach();
    }

    fn loaded(
        &mut self,
        r: Result<Loaded, String>,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        self.busy = busy_after_load(self.busy, replace);
        match r {
            Err(_) => self.load_error = Some(transport_problem()),
            Ok(l) => {
                self.load_error = None;
                self.monitors = l.monitors;
                match l.file {
                    Reply::Ok(gp) => self.adopt(gp, replace),
                    Reply::Remote(b) if is_readonly(&b) => {
                        self.readonly = Some(remote_problem(&b));
                    }
                    Reply::Remote(b) => self.load_error = Some(remote_problem(&b)),
                }
            }
        }
        self.refresh_ui(window, cx);
        if self.file.is_some()
            && let Some(target) = self.pending.take()
        {
            self.select_layout(target.as_deref(), window, cx);
        }
    }

    fn adopt(&mut self, gp: GetProfilesResult, replace: bool) {
        let keep = self.layout().map(|l| l.id.clone());
        let mode = if replace || !self.dirty() {
            Reload::Replace
        } else {
            Reload::Rebase
        };
        let (old_saved, old_rev) = self
            .file
            .as_ref()
            .map(|f| (f.saved.clone(), Some(f.revision.clone())))
            .unwrap_or_default();
        let (draft, flagged) = reload_draft(mode, &old_saved, &gp.layouts, &self.draft);
        match mode {
            Reload::Replace => {
                self.hist.clear();
                self.outside.clear();
                if !replace && old_rev.is_some_and(|r| r != gp.revision) {
                    self.notice = Some(t("editor.notice.reloaded_outside").into_owned());
                }
            }
            Reload::Rebase => {
                if old_rev.as_ref() != Some(&gp.revision) {
                    self.notice = Some(t("editor.notice.rebased").into_owned());
                }
                self.outside = flagged;
            }
        }
        self.draft = draft;
        self.readonly = None;
        self.file = Some(FileData {
            path: gp.path,
            revision: gp.revision,
            profiles: gp.profiles,
            saved: gp.layouts,
        });
        self.pick = keep
            .and_then(|id| pick_by_id(&self.draft, &id))
            .or(self.pick);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &self.file else {
            return;
        };
        let Some(params) = build_payload(&f.saved, &self.draft, &f.revision, &self.monitors) else {
            return;
        };
        self.busy = true;
        self.save_error = None;
        self.notice = None;
        self.save_warnings.clear();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let r = ipc_call(cx, move |c| save_call(c, params)).await;
            this.update_in(cx, |t, window, cx| t.saved(r, window, cx))
                .ok();
        })
        .detach();
    }

    fn saved(
        &mut self,
        r: Result<Reply<SetProfilesResult>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = busy_after_save(matches!(r, Ok(Reply::Ok(_))));
        match r {
            Ok(Reply::Ok(res)) => {
                self.save_warnings = res.warnings;
                self.notice = Some(t("editor.notice.saved").into_owned());
                self.reload(true, window, cx);
            }
            // Never overwrite: the user picks in the conflict dialog; the draft stays.
            Ok(Reply::Remote(b)) if b.code == ErrorCode::RevisionConflict => self.conflict = true,
            Ok(Reply::Remote(b)) if is_readonly(&b) => self.readonly = Some(remote_problem(&b)),
            Ok(Reply::Remote(b)) => self.save_error = Some(remote_problem(&b)),
            Err(_) => self.save_error = Some(transport_problem()),
        }
        cx.notify();
    }

    fn resolve_conflict(
        &mut self,
        choice: Option<Reload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conflict = false;
        match choice {
            Some(mode) => self.reload(mode == Reload::Replace, window, cx),
            None => cx.notify(),
        }
    }
}

// ---------------------------------------------------------------------- rendering

fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> Stateful<Div> {
    let d = div()
        .id(id)
        .min_h(HIT)
        .px(SP_3)
        .flex()
        .items_center()
        .rounded(R_SM)
        .text_size(TEXT_MD)
        .bg(if selected {
            accent().opacity(0.18)
        } else {
            raised()
        })
        .text_color(if selected { accent() } else { text() })
        .font_weight(if selected {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .child(label.into());
    if disabled {
        d.opacity(0.4).cursor_default()
    } else {
        d.focusable()
            .tab_stop(true)
            .cursor_pointer()
            .hover(|s| s.bg(text().opacity(0.08)))
            .focus_visible(|s| s.border_1().border_color(accent()))
            .on_click(on_click)
    }
}

fn labeled(label: &str, child: impl IntoElement, error: Option<String>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_1)
        .child(
            div()
                .text_size(TEXT_SM)
                .text_color(text_muted())
                .child(label.to_owned()),
        )
        .child(child)
        .when_some(error, |d, m| {
            d.child(div().text_size(TEXT_SM).text_color(danger()).child(m))
        })
}

fn title(s: impl Into<SharedString>) -> Div {
    div().font_weight(FontWeight::SEMIBOLD).child(s.into())
}

fn muted(s: impl Into<SharedString>) -> Div {
    div()
        .text_size(TEXT_SM)
        .text_color(text_muted())
        .child(s.into())
}

/// Error card: what happened in one sentence, an optional detail line and the small code.
fn problem_banner(p: &Problem, detail: Option<String>, action: Option<Btn>) -> Div {
    card()
        .flex_row()
        .items_start()
        .gap(SP_3)
        .child(
            div()
                .text_color(danger())
                .font_weight(FontWeight::SEMIBOLD)
                .child(tr("editor.banner.error")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(SP_1)
                .child(p.text.clone())
                .when_some(detail, |d, x| d.child(muted(x)))
                .child(
                    muted(tf("editor.banner.code", &[("code", &p.code)])).font_features(tabular()),
                ),
        )
        .when_some(action, |d, a| d.child(a))
}

fn kind_label(k: BadgeKind) -> SharedString {
    tr(match k {
        BadgeKind::Error => "editor.kind.error",
        BadgeKind::Warn => "editor.kind.warn",
        _ => "editor.kind.info",
    })
}

fn name_or_blank(id: &str) -> String {
    if id.trim().is_empty() {
        t("editor.unnamed").into_owned()
    } else {
        tf("editor.quoted", &[("id", &id)])
    }
}

fn monitor_label(m: &MonitorInfo) -> String {
    tf(
        "editor.monitor_label",
        &[
            ("n", &(m.index + 1)),
            ("size", &format_size(m.rect.width(), m.rect.height())),
        ],
    )
}

fn monitor_is(sel: &MonitorSel, m: &MonitorInfo) -> bool {
    match sel {
        MonitorSel::Index(i) => *i == m.index,
        MonitorSel::Device(d) => d.eq_ignore_ascii_case(&m.device),
        MonitorSel::Primary => false,
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.last_render = Instant::now();
        if i18n::locale() != self.locale {
            self.locale = i18n::locale();
            for (inp, st) in INPUTS.iter().zip(&self.inputs) {
                let ph = placeholder(*inp);
                st.update(cx, |s, cx| s.set_placeholder(ph, window, cx));
            }
        }
        let issues = validate(&self.draft, &self.monitors);
        let gate = SaveGate {
            dirty: self.dirty(),
            errors: !issues.is_empty(),
            readonly: self.readonly.is_some(),
            busy: self.busy || self.loading || self.file.is_none(),
            conflict: self.conflict,
        };
        let blocked = gate.blocked();

        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(SP_2)
            .when(gate.dirty, |d| {
                d.child(badge(tr("editor.badge.unsaved"), BadgeKind::Warn))
            })
            .when(self.readonly.is_some(), |d| {
                d.child(badge(tr("editor.badge.readonly"), BadgeKind::Error))
            })
            .child(
                secondary_button(
                    "discard",
                    tr("editor.btn.discard"),
                    cx.listener(|t, _, w, cx| t.discard(w, cx)),
                )
                .disabled(!gate.dirty || self.busy || gate.readonly || gate.conflict),
            )
            .child(
                primary_button(
                    "save",
                    if self.busy {
                        tr("editor.btn.saving")
                    } else {
                        tr("editor.btn.save")
                    },
                    cx.listener(|t, _, w, cx| t.save(w, cx)),
                )
                .icon(IconName::Check)
                .disabled(blocked.is_some()),
            );
        let head = div()
            .flex()
            .flex_wrap()
            .items_start()
            .justify_between()
            .gap(SP_3)
            .child(
                section_header(tr("editor.title"), tr("editor.subtitle"))
                    .flex_1()
                    .min_w(px(280.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(SP_1)
                    .child(actions)
                    .when_some(blocked.filter(|_| gate.dirty || gate.errors), |d, why| {
                        d.child(muted(why.into_owned()))
                    }),
            );

        let mut top = div().flex().flex_col().gap(SP_3);
        let mut has_top = false;
        if let Some(p) = &self.load_error {
            has_top = true;
            let retry = secondary_button(
                "retry",
                tr("editor.btn.retry"),
                cx.listener(|t, _, w, cx| t.reload(false, w, cx)),
            )
            .icon(IconName::RefreshCw)
            .disabled(self.loading);
            top = top.child(problem_banner(p, None, Some(retry)));
        }
        if let Some(p) = &self.readonly {
            has_top = true;
            // Before a first good read the engine gave no path; the panel runs as the same user.
            let path = self
                .file
                .as_ref()
                .map(|f| f.path.clone())
                .or_else(|| {
                    crate::profiles::default_path()
                        .ok()
                        .map(|p| p.display().to_string())
                })
                .map(|p| tf("editor.file_label", &[("path", &p)]));
            // The poll never runs over a draft, so re-reading after fixing the file is manual.
            let reread = secondary_button(
                "reread",
                tr("editor.btn.reread"),
                cx.listener(|t, _, w, cx| t.reload(false, w, cx)),
            )
            .icon(IconName::RefreshCw)
            .disabled(self.loading);
            top = top.child(problem_banner(p, path, Some(reread)));
        }
        if let Some(p) = &self.save_error {
            has_top = true;
            top = top.child(problem_banner(
                p,
                Some(t("editor.save_error_hint").into_owned()),
                None,
            ));
        }
        if self.conflict {
            has_top = true;
            top = top.child(self.conflict_card(cx));
        }
        if let Some(n) = &self.notice {
            has_top = true;
            top = top.child(
                card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(SP_3)
                            .child(badge(tr("editor.badge.ready"), BadgeKind::Ok))
                            .child(div().flex_1().min_w_0().child(n.clone())),
                    )
                    .children(self.save_warnings.iter().map(|w| {
                        div()
                            .flex()
                            .flex_col()
                            .child(warning_text(w))
                            .child(muted(tf("editor.banner.code", &[("code", &w.code)])))
                    })),
            );
        }

        let mut body = div()
            .id("editor-scroll")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(SP_4)
            .overflow_y_scroll();
        if self.monitors.is_empty() {
            if self.load_error.is_none() {
                body = body.child(if self.loading || self.file.is_none() {
                    div().text_color(text_muted()).child(tr("editor.loading"))
                } else {
                    empty_state(
                        tr("editor.no_monitors.title"),
                        tr("editor.no_monitors.body"),
                    )
                });
            }
        } else {
            body = body
                .child(self.design_card(cx))
                .child(self.preview_card(window, cx));
            if self.sel.is_some() {
                body = body.child(self.island_card(cx));
            }
            body = body.child(self.issues_card(&issues));
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(head)
            .when(has_top, |d| d.child(top.pb(SP_4)))
            .child(body)
    }
}

impl EditorView {
    fn conflict_card(&self, cx: &mut Context<Self>) -> Div {
        card()
            .child(title(tr("editor.conflict.title")).text_color(warn()))
            .child(
                div()
                    .text_color(text_muted())
                    .child(tr("editor.conflict.body")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_2)
                    .child(primary_button(
                        "conflict-rebase",
                        tr("editor.btn.rebase"),
                        cx.listener(|t, _, w, cx| t.resolve_conflict(Some(Reload::Rebase), w, cx)),
                    ))
                    .child(secondary_button(
                        "conflict-discard",
                        tr("editor.btn.discard_mine"),
                        cx.listener(|t, _, w, cx| t.resolve_conflict(Some(Reload::Replace), w, cx)),
                    ))
                    .child(secondary_button(
                        "conflict-keep",
                        tr("editor.btn.keep_editing"),
                        cx.listener(|t, _, w, cx| t.resolve_conflict(None, w, cx)),
                    )),
            )
    }

    fn design_card(&self, cx: &mut Context<Self>) -> Div {
        let can = self.can_edit_file();
        let is_preset = matches!(self.pick, Some(Pick::Preset(_)));
        let mut chips = div().flex().flex_wrap().gap(SP_2);
        for (i, l) in self.draft.iter().enumerate() {
            chips = chips.child(chip(
                ("layout", i),
                name_or_blank(&l.id),
                self.pick == Some(Pick::Draft(i)),
                false,
                cx.listener(move |t, _, w, cx| {
                    t.pick = Some(Pick::Draft(i));
                    t.sel = None;
                    t.refresh_ui(w, cx);
                }),
            ));
        }
        for j in visible_presets(&self.draft) {
            chips = chips.child(chip(
                ("preset", j),
                tf(
                    "editor.chip.preset",
                    &[("name", &name_or_blank(&self.presets[j].id))],
                ),
                self.pick == Some(Pick::Preset(j)),
                false,
                cx.listener(move |t, _, w, cx| {
                    t.pick = Some(Pick::Preset(j));
                    t.sel = None;
                    t.refresh_ui(w, cx);
                }),
            ));
        }
        let buttons = div()
            .flex()
            .flex_wrap()
            .gap(SP_2)
            .child(
                secondary_button(
                    "new-layout",
                    tr("editor.btn.new_layout"),
                    cx.listener(|t, _, w, cx| t.new_design(w, cx)),
                )
                .icon(IconName::Plus)
                .disabled(!can),
            )
            .when(is_preset, |d| {
                d.child(
                    primary_button(
                        "duplicate",
                        tr("editor.btn.duplicate"),
                        cx.listener(|t, _, w, cx| t.duplicate(w, cx)),
                    )
                    .icon(IconName::Copy)
                    .disabled(!can),
                )
            })
            .when(self.draft_index().is_some(), |d| {
                d.child(
                    secondary_button(
                        "delete-layout",
                        tr("editor.btn.delete_layout"),
                        cx.listener(|t, _, w, cx| t.delete_layout(w, cx)),
                    )
                    .icon(IconName::Delete)
                    .disabled(!can),
                )
            });
        let Some(l) = self.layout() else {
            return card()
                .child(title(tr("editor.design")))
                .child(chips)
                .child(buttons);
        };
        let editable_layout = can && !is_preset;
        let primary = div().child(chip(
            "monitor-primary",
            tr("editor.monitor.primary"),
            l.monitor == MonitorSel::Primary,
            !editable_layout,
            cx.listener(|t, _, w, cx| {
                if t.edit_layout(|l, _| l.monitor = MonitorSel::Primary) {
                    t.after_change(w, cx);
                }
            }),
        ));
        let monitors =
            self.monitors
                .iter()
                .fold(div().flex().flex_wrap().gap(SP_2).child(primary), |d, m| {
                    let sel = MonitorSel::Device(m.device.clone());
                    d.child(chip(
                        ("monitor", m.index),
                        monitor_label(m),
                        monitor_is(&l.monitor, m),
                        !editable_layout,
                        cx.listener(move |t, _, w, cx| {
                            let sel = sel.clone();
                            if t.edit_layout(|l, _| l.monitor = sel) {
                                t.after_change(w, cx);
                            }
                        }),
                    ))
                });
        let missing = self.size_of(l).is_none();
        card()
            .child(title(tr("editor.design")))
            .child(chips.child(buttons))
            .when(is_preset, |c| {
                c.child(div().text_color(text_muted()).child(tf(
                    "editor.preset_readonly",
                    &[("name", &name_or_blank(&l.id))],
                )))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_4)
                    .child(
                        labeled(
                            &t("editor.layout_id_label"),
                            Input::new(&self.inputs[slot(Inp::LayoutId)])
                                .disabled(!editable_layout),
                            self.field_error
                                .filter(|(i, _)| *i == Inp::LayoutId)
                                .map(|(_, e)| e.message().into_owned()),
                        )
                        .w(px(260.)),
                    )
                    .child(
                        labeled(&t("editor.monitor"), monitors, None)
                            .flex_1()
                            .min_w(px(240.)),
                    ),
            )
            .when(missing, |c| {
                c.child(div().text_color(warn()).child(tr("editor.monitor_missing")))
            })
    }

    fn preview_card(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let editable = self.editable();
        let can = self.can_edit_file();
        let Some(l) = self.layout() else {
            self.geom = None;
            return card().child(title(tr("editor.preview")));
        };
        let mon = select_monitor(&l.monitor, &self.monitors).ok();
        let rects: Vec<Rect> = self.monitors.iter().map(|m| m.rect).collect();
        let max_w = preview_max_width(f32::from(window.viewport_size().width));
        let layout = desktop_layout(&rects, max_w, PREVIEW_MAX_H);
        let pm = self
            .monitors
            .iter()
            .map(|m| PreviewMonitor {
                rect: m.rect,
                // The edited monitor is named by the "Monitor" chips; its label would sit
                // under the islands.
                label: if mon.is_some_and(|x| x.index == m.index) && !l.islands.is_empty() {
                    SharedString::default()
                } else if m.primary {
                    tf(
                        "editor.monitor.primary_label",
                        &[("label", &monitor_label(m))],
                    )
                    .into()
                } else {
                    monitor_label(m).into()
                },
            })
            .collect();
        let islands: Vec<PreviewIsland> = mon
            .map(|m| {
                l.islands
                    .iter()
                    .enumerate()
                    .map(|(i, isl)| PreviewIsland {
                        rect: island_rect_on(m.rect, &isl.rect),
                        name: if isl.id.trim().is_empty() {
                            tr("editor.unnamed")
                        } else {
                            isl.id.clone().into()
                        },
                        occupants: Vec::new(),
                        color_idx: i,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sel_px = mon
            .zip(self.sel.and_then(|s| l.islands.get(s)))
            .map(|(m, isl)| island_px(&isl.rect, m.rect.width(), m.rect.height()));
        let island_chips = l.islands.iter().enumerate().fold(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(SP_2)
                .child(muted(tr("editor.islands_label"))),
            |d, (i, isl)| {
                d.child(chip(
                    ("island-chip", i),
                    name_or_blank(&isl.id),
                    self.sel == Some(i),
                    false,
                    cx.listener(move |t, _, w, cx| {
                        t.sel = Some(i);
                        t.refresh_ui(w, cx);
                    }),
                ))
            },
        );
        let n_islands = l.islands.len();
        // Snap guides: 2 px light lines across the whole preview, past its edges, so they
        // stay visible on top of the accent border of the dragged island.
        let guide = || div().absolute().bg(text().opacity(0.9));
        let (ox, oy, s) = (layout.origin.0, layout.origin.1, layout.scale);
        let (pw, ph) = layout.size;
        let mut guides: Vec<Div> = Vec::new();
        if let (Some(m), Some(x)) = (mon, self.guides.0) {
            let gx = (f64::from(m.rect.left + x - ox) * s) as f32;
            guides.push(
                guide()
                    .left(px(gx - 1.0))
                    .top(-SP_1)
                    .w(px(2.))
                    .h(px(ph) + SP_1 * 2.),
            );
            // Coordinate tag next to the line (flipped left near the right edge).
            let tag_x = if gx > pw - 110.0 {
                gx - 110.0
            } else {
                gx + 6.0
            };
            guides.push(
                div()
                    .absolute()
                    .left(px(tag_x))
                    .top(px(ph / 2.0))
                    .px(SP_1)
                    .rounded(R_SM)
                    .bg(bg())
                    .text_size(TEXT_SM)
                    .text_color(text())
                    .font_features(tabular())
                    .child(format!("x = {x} px")),
            );
        }
        if let (Some(m), Some(y)) = (mon, self.guides.1) {
            let gy = (f64::from(m.rect.top + y - oy) * s) as f32;
            guides.push(
                guide()
                    .top(px(gy - 1.0))
                    .left(-SP_1)
                    .h(px(2.))
                    .w(px(pw) + SP_1 * 2.),
            );
        }
        let prects: Vec<PRect> = islands
            .iter()
            .map(|i| layout.rect_to_preview(i.rect))
            .collect();
        // The 8 resize handles of the selected island (drawn; hit-testing is `pick`).
        let handles: Vec<Div> = self
            .sel
            .and_then(|s| prects.get(s))
            .filter(|_| editable)
            .map(|r| {
                let (xs, ys) = (
                    [r.x, r.x + r.w / 2.0, r.right()],
                    [r.y, r.y + r.h / 2.0, r.bottom()],
                );
                let side = HANDLE_HALF * 1.25;
                (0..9)
                    .filter(|k| *k != 4)
                    .map(|k| {
                        div()
                            .absolute()
                            .left(px(xs[k % 3] - side / 2.0))
                            .top(px(ys[k / 3] - side / 2.0))
                            .size(px(side))
                            .bg(accent())
                            .border_1()
                            .border_color(surface())
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.geom = Some(Geom {
            layout: layout.clone(),
            monitor: mon.map(|m| m.rect),
            islands: prects,
            editable,
        });

        let this = cx.entity().downgrade();
        let dragging = self.drag.is_some();
        let cursor = cursor_for(self.drag.as_ref().map(|d| d.hit).or(self.hover), dragging);
        // Window-level listeners (registered while painting) so a drag keeps going outside
        // the preview; element listeners would stop at its hitbox.
        let overlay = canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                if dragging {
                    window.set_window_cursor_style(cursor);
                } else {
                    window.set_cursor_style(cursor, &hitbox);
                }
                let origin = bounds.origin;
                let (h, t) = (hitbox.clone(), this.clone());
                window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble
                        && ev.button == MouseButton::Left
                        && h.is_hovered(window)
                    {
                        t.update(cx, |v, cx| v.mouse_down(ev, origin, window, cx))
                            .ok();
                    }
                });
                let (h, t) = (hitbox, this.clone());
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let hovered = h.is_hovered(window);
                        t.update(cx, |v, cx| v.mouse_move(ev, origin, hovered, window, cx))
                            .ok();
                    }
                });
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                        this.update(cx, |v, cx| v.end_drag(window, cx)).ok();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let overlay = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .children(handles)
            .children(guides)
            .child(overlay);

        let preview = MonitorPreview::new(layout, pm, islands)
            .selected(self.sel)
            .grid(self.grid)
            .overlay(overlay);
        let frame = div()
            .id("editor-preview")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|t, ev: &KeyDownEvent, w, cx| t.on_key(ev, w, cx)))
            .self_start()
            .p(SP_1)
            .rounded(R_SM)
            .border_2()
            .border_color(surface())
            .focus(|s| s.border_color(accent()))
            .child(preview);

        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(SP_2)
            .child(chip(
                "snap",
                if self.snap {
                    tr("editor.snap_on")
                } else {
                    tr("editor.snap_off")
                },
                self.snap,
                false,
                cx.listener(|t, _, _, cx| {
                    t.snap = !t.snap;
                    cx.notify();
                }),
            ))
            .child(chip(
                "grid",
                if self.grid {
                    tr("editor.grid_on")
                } else {
                    tr("editor.grid_off")
                },
                self.grid,
                false,
                cx.listener(|t, _, _, cx| {
                    t.grid = !t.grid;
                    cx.notify();
                }),
            ))
            .child(
                secondary_button(
                    "new-island",
                    tr("editor.btn.new_island"),
                    cx.listener(|t, _, w, cx| {
                        if t.add_island(None) {
                            t.after_change(w, cx);
                        }
                    }),
                )
                .icon(IconName::Plus)
                .disabled(!editable),
            )
            .child(
                secondary_button(
                    "fit-16-9",
                    tr("editor.btn.fit"),
                    cx.listener(|t, _, w, cx| {
                        if t.edit_rect(fit_16_9) {
                            t.after_change(w, cx);
                        }
                    }),
                )
                .icon(IconName::Maximize)
                .disabled(!editable || sel_px.is_none()),
            )
            .child(
                secondary_button(
                    "undo",
                    tr("editor.btn.undo"),
                    cx.listener(|t, _, w, cx| t.undo_redo(false, w, cx)),
                )
                .icon(IconName::Undo)
                .disabled(!can || !self.hist.can_undo()),
            )
            .child(
                secondary_button(
                    "redo",
                    tr("editor.btn.redo"),
                    cx.listener(|t, _, w, cx| t.undo_redo(true, w, cx)),
                )
                .icon(IconName::Redo)
                .disabled(!can || !self.hist.can_redo()),
            )
            .when(sel_px.is_some_and(|p| is_16_9(p.w, p.h)), |d| {
                d.child(badge("16:9", BadgeKind::Ok))
            });

        card()
            .child(title(tr("editor.preview")))
            .child(toolbar)
            .child(frame)
            .child(island_chips.when(n_islands == 0, |d| d.child(muted(tr("editor.none_yet")))))
            .child(muted(tr(help_key(editable, self.conflict, self.busy))))
    }

    fn island_card(&self, cx: &mut Context<Self>) -> Div {
        let (Some(l), Some(s)) = (self.layout(), self.sel) else {
            return div();
        };
        let Some(isl) = l.islands.get(s) else {
            return div();
        };
        let editable = self.editable();
        let can_id = self.can_edit_file() && self.draft_index().is_some();
        let err = |inp: Inp| {
            self.field_error
                .filter(|(i, _)| *i == inp)
                .map(|(_, e)| e.message().into_owned())
        };
        let num = |label: &str, f: Field| {
            let inp = Inp::Num(f);
            labeled(
                label,
                Input::new(&self.inputs[slot(inp)]).disabled(!editable),
                err(inp),
            )
            .w(px(132.))
        };
        let mon = select_monitor(&l.monitor, &self.monitors).ok();
        let size_line = mon.map(|m| {
            let r = island_rect_on(m.rect, &isl.rect);
            tf(
                "editor.size_line",
                &[
                    ("fraction", &fraction_label(r, m.rect)),
                    ("size", &format_size(r.width(), r.height())),
                ],
            )
        });
        let overlap_names: Vec<String> = mon
            .map(|m| {
                let rects: Vec<PxRect> = l
                    .islands
                    .iter()
                    .map(|i| island_px(&i.rect, m.rect.width(), m.rect.height()))
                    .collect();
                overlaps(&rects)
                    .into_iter()
                    .filter_map(|(a, b)| match (a == s, b == s) {
                        (true, _) => Some(b),
                        (_, true) => Some(a),
                        _ => None,
                    })
                    .map(|o| name_or_blank(&l.islands[o].id))
                    .collect()
            })
            .unwrap_or_default();
        card()
            .child(title(tr("editor.selected")))
            .child(
                labeled(
                    &t("editor.island_id_label"),
                    Input::new(&self.inputs[slot(Inp::IslandId)]).disabled(!can_id),
                    err(Inp::IslandId),
                )
                .w(px(260.)),
            )
            .when_some(size_line, |c, t| c.child(muted(t).font_features(tabular())))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_3)
                    .child(num(&t("editor.lbl.x_px"), Field::PxX))
                    .child(num(&t("editor.lbl.y_px"), Field::PxY))
                    .child(num(&t("editor.lbl.w_px"), Field::PxW))
                    .child(num(&t("editor.lbl.h_px"), Field::PxH)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_3)
                    .child(num(&t("editor.lbl.x_frac"), Field::FrX))
                    .child(num(&t("editor.lbl.y_frac"), Field::FrY))
                    .child(num(&t("editor.lbl.w_frac"), Field::FrW))
                    .child(num(&t("editor.lbl.h_frac"), Field::FrH)),
            )
            .child(muted(tr("editor.fields_help")))
            .when(!overlap_names.is_empty(), |c| {
                c.child(div().text_color(warn()).child(tf(
                    "editor.overlaps_with",
                    &[("names", &overlap_names.join(", "))],
                )))
            })
            .child(
                div().flex().child(
                    secondary_button(
                        "delete-island",
                        tr("editor.btn.delete_island"),
                        cx.listener(|t, _, w, cx| {
                            if t.delete_island() {
                                t.after_change(w, cx);
                            }
                        }),
                    )
                    .icon(IconName::Delete)
                    .disabled(!can_id),
                ),
            )
    }

    fn issues_card(&self, issues: &[super::editor_model::Issue]) -> Div {
        let mut rows: Vec<(BadgeKind, String)> = Vec::new();
        for i in issues {
            let Some(l) = self.draft.get(i.layout) else {
                continue;
            };
            let place = match i.island.and_then(|ii| l.islands.get(ii)) {
                Some(isl) => format!("{} › {}", name_or_blank(&l.id), name_or_blank(&isl.id)),
                None => name_or_blank(&l.id),
            };
            rows.push((BadgeKind::Error, format!("{place}: {}", i.kind.message())));
        }
        for l in &self.draft {
            let Some((mw, mh)) = self.size_of(l) else {
                continue;
            };
            let rects: Vec<PxRect> = l
                .islands
                .iter()
                .map(|i| island_px(&i.rect, mw, mh))
                .collect();
            for (a, b) in overlaps(&rects) {
                rows.push((
                    BadgeKind::Warn,
                    tf(
                        "editor.issue.overlap",
                        &[
                            ("layout", &name_or_blank(&l.id)),
                            ("a", &name_or_blank(&l.islands[a].id)),
                            ("b", &name_or_blank(&l.islands[b].id)),
                        ],
                    ),
                ));
            }
        }
        if let Some(f) = &self.file {
            for (process, r) in profile_refs(&f.profiles, &f.saved, &self.draft) {
                rows.push((
                    BadgeKind::Warn,
                    tf(
                        "editor.issue.profile_ref",
                        &[("process", &process), ("island", &r)],
                    ),
                ));
            }
            for w in aspect_warnings(&f.profiles, &self.draft, &self.monitors) {
                rows.push((
                    BadgeKind::Info,
                    tf(
                        "editor.issue.aspect",
                        &[
                            ("process", &w.process),
                            ("game", &format_size(w.game.width, w.game.height)),
                            ("island", &w.island),
                            (
                                "size",
                                &format_size(w.island_size.width, w.island_size.height),
                            ),
                        ],
                    ),
                ));
            }
        }
        for id in &self.outside {
            rows.push((
                BadgeKind::Warn,
                tf("editor.issue.outside", &[("name", &name_or_blank(id))]),
            ));
        }
        card()
            .child(title(tr("editor.issues")))
            .when(rows.is_empty(), |c| {
                c.child(muted(tr("editor.issues_none")))
            })
            .children(rows.into_iter().map(|(k, t)| {
                div()
                    .flex()
                    .items_start()
                    .gap(SP_2)
                    .child(badge(kind_label(k), k).flex_shrink_0())
                    .child(div().flex_1().min_w_0().child(t))
            }))
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::editor_model::snap_targets;
    use crate::gui::monitor_preview::Handle;
    use crate::i18n::{Locale, scoped_locale};
    use crate::ipc::ErrorCode;
    use crate::islands::{IslandRect, MonitorSel, Unit, presets};

    fn px(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    #[test]
    fn a_save_stays_busy_until_its_reload_lands() {
        let busy = busy_after_save(true);
        assert!(
            busy,
            "edits must stay blocked until the post-save reload replaces the draft"
        );
        // An unrelated reload (poll, reconnect) does not end it; the post-save one does.
        assert!(busy_after_load(busy, false));
        assert!(!busy_after_load(busy, true));
        // A failed save leaves nothing in flight.
        assert!(!busy_after_save(false));
    }

    fn layout(id: &str, islands: &[&str]) -> Layout {
        Layout {
            id: id.into(),
            monitor: MonitorSel::Primary,
            islands: islands
                .iter()
                .map(|i| Island {
                    id: (*i).into(),
                    rect: IslandRect {
                        x: 0.0,
                        y: 0.0,
                        w: 0.5,
                        h: 0.5,
                        unit: Unit::Fraction,
                    },
                    extra: Default::default(),
                })
                .collect(),
            extra: Default::default(),
        }
    }

    // ---- drag

    #[test]
    fn body_drag_moves_by_the_rounded_physical_delta() {
        let (r, g) = drag_target(
            Hit::Body,
            px(100, 100, 400, 300),
            (500.0, 500.0),
            (550.4, 489.6),
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(150, 90, 400, 300));
        assert_eq!(g, (None, None));
    }

    #[test]
    fn handle_drag_resizes_only_its_edges() {
        let (r, _) = drag_target(
            Hit::Handle(Handle::SE),
            px(100, 100, 400, 300),
            (0.0, 0.0),
            (50.0, -20.0),
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(100, 100, 450, 280));
        let (r, _) = drag_target(
            Hit::Handle(Handle::W),
            px(100, 100, 400, 300),
            (0.0, 0.0),
            (30.0, 999.0),
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(130, 100, 370, 300));
    }

    #[test]
    fn drag_snaps_to_the_half_and_reports_the_guide() {
        let t = snap_targets(1920, 1080, &[]);
        let (r, g) = drag_target(
            Hit::Body,
            px(0, 0, 400, 300),
            (0.0, 0.0),
            (955.0, 0.0),
            1920,
            1080,
            Some((&t, 8.0)),
        );
        assert_eq!(r.x, 960);
        assert_eq!(g.0, Some(960));
    }

    #[test]
    fn drag_outside_the_monitor_stays_clamped() {
        let (r, _) = drag_target(
            Hit::Body,
            px(100, 100, 400, 300),
            (0.0, 0.0),
            (-5000.0, 9000.0),
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(0, 780, 400, 300));
    }

    #[test]
    fn finished_drag_records_undo_only_when_something_changed() {
        let before = vec![layout("a", &["x"])];
        let mut h = History::default();
        assert!(!h.finish(before.clone(), &before));
        assert!(!h.can_undo());
        let mut after = before.clone();
        after[0].islands[0].rect.x = 0.25;
        assert!(h.finish(before.clone(), &after));
        let mut draft = after;
        assert!(h.undo(&mut draft));
        assert_eq!(draft, before);
    }

    // ---- history

    #[test]
    fn undo_redo_roundtrip_and_new_edit_clears_redo() {
        let s0 = vec![layout("a", &[])];
        let s1 = vec![layout("a", &["x"])];
        let s2 = vec![layout("a", &["x", "y"])];
        let mut h = History::default();
        let mut draft = s0.clone();
        h.record(draft.clone());
        draft = s1.clone();
        h.record(draft.clone());
        draft = s2.clone();
        assert!(h.undo(&mut draft));
        assert_eq!(draft, s1);
        assert!(h.undo(&mut draft));
        assert_eq!(draft, s0);
        assert!(!h.undo(&mut draft));
        assert!(h.redo(&mut draft));
        assert_eq!(draft, s1);
        h.record(draft.clone());
        draft = vec![];
        assert!(!h.can_redo());
        assert!(!h.redo(&mut draft));
    }

    #[test]
    fn history_keeps_at_most_100_steps() {
        let mut h = History::default();
        for i in 0..150 {
            h.record(vec![layout(&format!("l{i}"), &[])]);
        }
        let mut draft = vec![];
        let mut n = 0;
        while h.undo(&mut draft) {
            n += 1;
        }
        assert_eq!(n, UNDO_LIMIT);
        assert_eq!(draft[0].id, "l50");
    }

    // ---- keyboard

    #[test]
    fn keys_map_to_moves_resizes_and_edits() {
        use KeyAction::*;
        assert_eq!(key_action("left", false, false, false), Some(Move(-1, 0)));
        assert_eq!(key_action("down", false, true, false), Some(Move(0, 10)));
        assert_eq!(key_action("right", false, false, true), Some(Resize(1, 0)));
        assert_eq!(key_action("up", false, true, true), Some(Resize(0, -10)));
        assert_eq!(key_action("delete", false, false, false), Some(Delete));
        assert_eq!(key_action("backspace", false, false, false), Some(Delete));
        assert_eq!(key_action("z", true, false, false), Some(Undo));
        assert_eq!(key_action("z", true, true, false), Some(Redo));
        assert_eq!(key_action("y", true, false, false), Some(Redo));
        assert_eq!(key_action("z", false, false, false), None);
        assert_eq!(key_action("left", true, false, false), None);
        assert_eq!(key_action("1", true, false, false), None);
    }

    #[test]
    fn nudges_move_and_resize_inside_the_monitor() {
        let r = px(0, 0, 400, 300);
        assert_eq!(
            nudge(r, KeyAction::Move(10, 1), 1920, 1080),
            px(10, 1, 400, 300)
        );
        assert_eq!(nudge(r, KeyAction::Move(-1, -10), 1920, 1080), r);
        assert_eq!(
            nudge(r, KeyAction::Resize(10, -1), 1920, 1080),
            px(0, 0, 410, 299)
        );
        assert_eq!(
            nudge(px(0, 0, 64, 64), KeyAction::Resize(-10, -10), 1920, 1080),
            px(0, 0, 64, 64)
        );
        assert_eq!(
            nudge(px(1520, 0, 400, 300), KeyAction::Resize(10, 0), 1920, 1080),
            px(1520, 0, 400, 300)
        );
    }

    // ---- conflict outcomes

    fn edited(l: &Layout) -> Layout {
        let mut l = l.clone();
        l.islands[0].rect.x = 0.125;
        l
    }

    #[test]
    fn rebase_after_conflict_keeps_the_users_edit_and_flags_it() {
        let a = layout("a", &["x"]);
        let b = layout("b", &["y"]);
        let old = vec![a.clone(), b.clone()];
        let mut outside_a = a.clone();
        outside_a.islands[0].id = "fuera".into();
        let mut outside_b = b.clone();
        outside_b.islands[0].id = "fuera".into();
        let new = vec![outside_a, outside_b.clone(), layout("c", &[])];
        let draft = vec![edited(&a), b];
        let (d, flagged) = reload_draft(Reload::Rebase, &old, &new, &draft);
        assert_eq!(d, vec![edited(&a), outside_b, layout("c", &[])]);
        assert_eq!(flagged, vec!["a".to_owned()]);
    }

    #[test]
    fn discard_after_conflict_adopts_the_file() {
        let a = layout("a", &["x"]);
        let new = vec![layout("z", &[])];
        let (d, flagged) = reload_draft(
            Reload::Replace,
            std::slice::from_ref(&a),
            &new,
            &[edited(&a)],
        );
        assert_eq!(d, new);
        assert!(flagged.is_empty());
    }

    // ---- save gate

    #[test]
    fn save_is_blocked_with_a_reason() {
        let ok = SaveGate {
            dirty: true,
            errors: false,
            readonly: false,
            busy: false,
            conflict: false,
        };
        assert_eq!(ok.blocked(), None);
        assert!(SaveGate { dirty: false, ..ok }.blocked().is_some());
        assert!(SaveGate { errors: true, ..ok }.blocked().is_some());
        assert!(
            SaveGate {
                readonly: true,
                ..ok
            }
            .blocked()
            .is_some()
        );
        assert!(SaveGate { busy: true, ..ok }.blocked().is_some());
        assert!(
            SaveGate {
                conflict: true,
                ..ok
            }
            .blocked()
            .is_some()
        );
    }

    // ---- picks and ids

    #[test]
    fn presets_shadowed_by_a_draft_layout_are_hidden() {
        let pid = presets()[0].id.clone();
        assert_eq!(visible_presets(&[]), vec![0]);
        assert!(visible_presets(&[layout(&pid, &[])]).is_empty());
        assert_eq!(default_pick(&[]), Some(Pick::Preset(0)));
        assert_eq!(default_pick(&[layout("a", &[])]), Some(Pick::Draft(0)));
        assert_eq!(pick_by_id(&[layout("a", &[])], &pid), Some(Pick::Preset(0)));
        assert_eq!(pick_by_id(&[layout(&pid, &[])], &pid), Some(Pick::Draft(0)));
        assert_eq!(pick_by_id(&[], "nada"), None);
    }

    #[test]
    fn next_id_takes_the_lowest_free_number_ignoring_case() {
        assert_eq!(next_id("island", &[]), "island-1");
        assert_eq!(next_id("island", &["ISLAND-1", "island-3"]), "island-2");
    }

    #[test]
    fn double_click_island_is_centred_quarter_by_half_and_clamped() {
        assert_eq!(
            new_island_rect((960, 540), 1920, 1080),
            px(720, 270, 480, 540)
        );
        assert_eq!(new_island_rect((5, 1075), 1920, 1080), px(0, 540, 480, 540));
    }

    // ---- cursor and errors

    #[test]
    fn cursor_follows_the_hover_target() {
        assert_eq!(cursor_for(None, false), CursorStyle::Arrow);
        assert_eq!(cursor_for(Some(Hit::Body), false), CursorStyle::OpenHand);
        assert_eq!(cursor_for(Some(Hit::Body), true), CursorStyle::ClosedHand);
        assert_eq!(
            cursor_for(Some(Hit::Handle(Handle::N)), false),
            CursorStyle::ResizeUpDown
        );
        assert_eq!(
            cursor_for(Some(Hit::Handle(Handle::W)), false),
            CursorStyle::ResizeLeftRight
        );
        assert_eq!(
            cursor_for(Some(Hit::Handle(Handle::SE)), false),
            CursorStyle::ResizeUpLeftDownRight
        );
        assert_eq!(
            cursor_for(Some(Hit::Handle(Handle::NE)), false),
            CursorStyle::ResizeUpRightDownLeft
        );
    }

    #[test]
    fn a_conflict_pauses_the_draft_instead_of_calling_it_read_only() {
        assert_eq!(help_key(true, false, false), "editor.help_edit");
        assert_eq!(help_key(false, true, false), "editor.help_paused");
        assert_eq!(help_key(false, false, false), "editor.help_readonly");
        assert_eq!(help_key(false, false, true), "editor.help_edit");
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            assert_ne!(t("editor.help_paused"), t("editor.help_readonly"));
            assert!(!t("editor.help_paused").to_lowercase().contains("read-only"));
        }
    }

    #[test]
    fn engine_errors_become_plain_sentences_with_their_code() {
        let _g = scoped_locale(Locale::En);
        let invalid = ErrorBody::new(ErrorCode::ProfileError, "invalid JSON: expected value");
        let p = remote_problem(&invalid);
        assert_eq!(p.code, "profile_error");
        assert!(!p.text.contains("expected value"));
        assert!(is_readonly(&invalid));
        let newer = ErrorBody::new(ErrorCode::UnsupportedVersion, "version 9 not supported");
        assert_eq!(remote_problem(&newer).code, "unsupported_version");
        assert!(is_readonly(&newer));
        let other = ErrorBody::new(ErrorCode::InvalidParams, "duplicate island");
        assert_eq!(
            remote_problem(&other).text,
            "Borderless Shatter did not understand the request."
        );
        assert!(!is_readonly(&other));
        let down = transport_problem();
        assert_eq!(down.code, "resident_unreachable");
        assert!(!down.text.contains("os error"));
    }

    #[test]
    fn save_gate_reasons_exist_in_every_language() {
        let gate = SaveGate {
            dirty: false,
            errors: false,
            readonly: false,
            busy: false,
            conflict: false,
        };
        let cases = [
            SaveGate {
                readonly: true,
                ..gate
            },
            SaveGate {
                conflict: true,
                ..gate
            },
            SaveGate { busy: true, ..gate },
            SaveGate {
                errors: true,
                ..gate
            },
            gate,
        ];
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            let msgs: Vec<_> = cases.iter().filter_map(|g| g.blocked()).collect();
            assert_eq!(msgs.len(), cases.len());
            assert!(msgs.iter().all(|m| !m.starts_with("editor.")), "{msgs:?}");
        }
    }
}
