//! Diagnostics: moves a window for real for a few seconds and explains how it reacts.
//!
//! Pure logic (texts, markers, axes, zoom) on top, with tests; the GPUI view below only
//! paints it. The chart has no canvas: marks and lines are divs positioned with `relative(f)`.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, BoxShadow, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, point, px,
    relative,
};

use serde_json::{Value, json};

use super::theme::{
    BadgeKind, R_SM, SP_1, SP_2, SP_3, SP_4, SP_6, TEXT_MD, TEXT_SM, TEXT_XL, accent, badge, card,
    danger, empty_state, primary_button, raised, secondary_button, section_header, stat, tabular,
    text, text_muted, warn,
};
use super::windows_view::{sort_windows, truncate_chars, visible_windows};
use super::{Failure, Reply, call, failure_text, get, lift};
use crate::diagnose::{Latencies, Recommendation, Verdict};
use crate::i18n::{self, fmt_decimal, placeholders, t, tf};
use crate::ipc::pipe::Client;
use crate::ipc::{
    CallError, DiagnoseResult, ListIslandsResult, ListWindowsResult, MAX_OBSERVE_MS, TimelineEntry,
    TimelineSource, WindowInfo,
};
use crate::win32::DpiAwareness;
use crate::window_state::Rect;

const MIN_OBSERVE_MS: u64 = 500;
const OBSERVE_STEP_MS: u64 = 500;
const DEFAULT_OBSERVE_MS: u64 = 3_000;
const REFRESH_AFTER: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);
const DEFAULT_ISLAND: &str = "halves-16-9/right";
const NO_ISLAND: &str = "";
const TITLE_MAX: usize = 70;
const MIN_SPAN_MS: f64 = 20.0;
const MAX_AXIS_TICKS: usize = 8;
const MAX_TABLE_ROWS: usize = 300;

const ROW_H: f32 = 44.0;
const AXIS_H: f32 = 28.0;
const GUTTER_W: f32 = 148.0;
const MARKER_W: f32 = 28.0;
const TABLE_MAX_H: f32 = 240.0;

// ------------------------------------------------------------------ pure logic

/// Headline, badge text and badge colour of a verdict.
pub fn verdict_text(v: Verdict) -> (Cow<'static, str>, Cow<'static, str>, BadgeKind) {
    let (name, kind) = match v {
        Verdict::Accepted => ("accepted", BadgeKind::Ok),
        Verdict::StyleReverted => ("style_reverted", BadgeKind::Warn),
        Verdict::SizeForced => ("size_forced", BadgeKind::Warn),
        Verdict::PositionForced => ("position_forced", BadgeKind::Warn),
        Verdict::StyleAndSizeReverted => ("style_and_size_reverted", BadgeKind::Error),
        Verdict::LateRevert => ("late_revert", BadgeKind::Warn),
    };
    (
        t(&format!("diagnose.verdict.{name}.headline")),
        t(&format!("diagnose.verdict.{name}.badge")),
        kind,
    )
}

/// "4.6 ms" / "60 ms" / "1.25 s" (decimal comma in Spanish). Non-finite or non-positive values
/// read "0 ms".
pub fn format_ms(ms: f64) -> String {
    if !ms.is_finite() || ms <= 0.0 {
        return "0 ms".into();
    }
    if ms < 10.0 {
        let r = (ms * 10.0).round() / 10.0;
        if r < 10.0 {
            return format!("{} ms", fmt_decimal(r, 1));
        }
    }
    if ms.round() < 1000.0 {
        return format!("{} ms", ms.round());
    }
    let s = fmt_decimal(ms / 1000.0, 2);
    let sep = t("common.decimal_separator");
    let sep = sep.chars().next().unwrap_or('.');
    let s = s.trim_end_matches('0').trim_end_matches(sep);
    format!("{s} s")
}

pub fn latency_sentences(l: &Latencies) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ms) = l.style {
        out.push(tf("diagnose.latency.style", &[("ms", &format_ms(ms))]));
    }
    if let Some(ms) = l.rect {
        out.push(tf("diagnose.latency.rect", &[("ms", &format_ms(ms))]));
    }
    if out.is_empty() {
        out.push(t("diagnose.latency.none").into_owned());
    }
    out
}

fn tr(key: &str) -> SharedString {
    SharedString::from(t(key).into_owned())
}

pub fn format_size(w: i32, h: i32) -> String {
    format!("{w}x{h}")
}

pub fn format_rect(r: &Rect) -> String {
    tf(
        "diagnose.rect_at",
        &[
            ("size", &format_size(r.width(), r.height())),
            ("x", &r.left),
            ("y", &r.top),
        ],
    )
}

pub fn dpi_percent(dpi: u32) -> String {
    tf(
        "diagnose.percent",
        &[("n", &((f64::from(dpi) * 100.0 / 96.0).round() as u32))],
    )
}

pub fn awareness_text(a: DpiAwareness) -> Cow<'static, str> {
    t(match a {
        DpiAwareness::Unaware => "diagnose.dpi.unaware",
        DpiAwareness::System => "diagnose.dpi.system",
        DpiAwareness::PerMonitor => "diagnose.dpi.per_monitor",
        DpiAwareness::PerMonitorV2 => "diagnose.dpi.per_monitor_v2",
        DpiAwareness::Unknown => "diagnose.dpi.unknown",
    })
}

/// Progress bar fraction; never reaches 1 because the engine needs a moment to answer.
pub fn observe_progress(elapsed_ms: f64, observe_ms: u64) -> f32 {
    let total = observe_ms as f64 + 1500.0;
    (elapsed_ms / total).clamp(0.0, 0.97) as f32
}

/// Slider value → allowed observation window (500..=15000, step 500).
pub fn clamp_observe(ms: u64) -> u64 {
    let stepped = (ms + OBSERVE_STEP_MS / 2) / OBSERVE_STEP_MS * OBSERVE_STEP_MS;
    stepped.clamp(MIN_OBSERVE_MS, MAX_OBSERVE_MS)
}

/// The sentence for a recommendation: `diagnose.rec.<code>` filled from `data` and the process
/// name; the engine's English `text` when the code is unknown or its numbers are missing.
pub fn recommendation_text(rec: &Recommendation, process: &str) -> String {
    let key = format!("diagnose.rec.{}", rec.code);
    if !i18n::has(&key) {
        return rec.text.clone();
    }
    let mut args: Vec<(String, String)> = vec![("process".to_owned(), process.to_owned())];
    if let Some(Value::Object(m)) = &rec.data {
        for (k, v) in m {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            args.push((k.clone(), v));
        }
    }
    let get = |k: &str| args.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    if rec.code == "warscape_resolution"
        && let Some(unaware) = get("dpi_unaware")
    {
        let w = get("island_width").unwrap_or_default();
        let h = get("island_height").unwrap_or_default();
        let how = if unaware == "true" {
            let dpi = get("dpi").unwrap_or_default();
            let pct = dpi.parse().map(dpi_percent).unwrap_or_default();
            tf(
                "diagnose.rec.warscape_how_unaware",
                &[
                    ("w", &w),
                    ("h", &h),
                    ("pct", &pct),
                    ("dpi", &dpi),
                    ("x_res", &get("x_res").unwrap_or_default()),
                    ("y_res", &get("y_res").unwrap_or_default()),
                ],
            )
        } else {
            tf("diagnose.rec.warscape_how_plain", &[("w", &w), ("h", &h)])
        };
        args.push(("how".to_owned(), how));
    }
    let template = t(&key);
    if !placeholders(&template)
        .iter()
        .all(|p| args.iter().any(|(k, _)| k == p))
    {
        return rec.text.clone();
    }
    let refs: Vec<(&str, &dyn std::fmt::Display)> = args
        .iter()
        .map(|(k, v)| (k.as_str(), v as &dyn std::fmt::Display))
        .collect();
    tf(&key, &refs)
}

/// All recommendation sentences, separated by blank lines (what "Copy" puts on the clipboard).
pub fn recommendations_text(recs: &[Recommendation], process: &str) -> String {
    recs.iter()
        .map(|r| recommendation_text(r, process))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Rows of the timeline chart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Style,
    Rect,
    Writes,
}

impl Row {
    const ALL: [Row; 3] = [Row::Style, Row::Rect, Row::Writes];

    fn label(self) -> String {
        t(match self {
            Row::Style => "diagnose.row.style",
            Row::Rect => "diagnose.row.rect",
            Row::Writes => "diagnose.row.writes",
        })
        .into_owned()
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub row: Row,
    pub t_ms: f64,
    pub source: TimelineSource,
    pub label: String,
    pub tip: String,
}

fn origin_text(s: TimelineSource) -> Cow<'static, str> {
    t(match s {
        TimelineSource::Us => "diagnose.origin.us",
        TimelineSource::Window => "diagnose.origin.window",
    })
}

/// Text of the detail box for one timeline entry.
pub fn entry_tip(e: &TimelineEntry) -> String {
    let mut s = tf(
        "diagnose.tip.head",
        &[
            ("t", &format_ms(e.t_ms)),
            ("origin", &origin_text(e.source)),
        ],
    );
    if let Some(op) = &e.op {
        s.push_str(&format!(" · {op}"));
    }
    s.push_str(&tf(
        "diagnose.tip.tail",
        &[
            ("style", &e.style),
            ("exstyle", &e.exstyle),
            ("rect", &format_rect(&e.rect)),
        ],
    ));
    s
}

/// Chart marks. Our writes go to their own row; a window entry marks Style and/or Rect only
/// where it differs from the previous *window* entry (the first one is the baseline). Our
/// entries never update that previous state: theirs is a later sample.
pub fn build_markers(timeline: &[TimelineEntry]) -> Vec<Marker> {
    let mut out = Vec::new();
    let mut prev: Option<&TimelineEntry> = None;
    for e in timeline {
        let tip = entry_tip(e);
        let mut push = |row, label: String, tip: String| {
            out.push(Marker {
                row,
                t_ms: e.t_ms,
                source: e.source,
                label,
                tip,
            });
        };
        match e.source {
            TimelineSource::Us => {
                push(Row::Writes, e.op.clone().unwrap_or_default(), tip);
            }
            TimelineSource::Window => {
                let tip = match prev {
                    None => tf("diagnose.initial_state", &[("tip", &tip)]),
                    Some(_) => tip,
                };
                let style_changed = prev.is_none_or(|p| p.style != e.style);
                let exstyle_changed = prev.is_none_or(|p| p.exstyle != e.exstyle);
                if style_changed || exstyle_changed {
                    let label = if style_changed {
                        e.style.clone()
                    } else {
                        format!("ex {}", e.exstyle)
                    };
                    push(Row::Style, label, tip.clone());
                }
                if prev.is_none_or(|p| p.rect != e.rect) {
                    push(Row::Rect, format_rect(&e.rect), tip);
                }
                prev = Some(e);
            }
        }
    }
    out.sort_by(|a, b| a.t_ms.total_cmp(&b.t_ms));
    out
}

/// True when the window never answered our writes: the engine verdict is `Accepted` with no
/// revert latency. Timeline states can't tell: our entries carry the first sample after the
/// write, which may already be the window's revert. Without any write of ours there is nothing
/// to say.
pub fn only_our_writes(r: &DiagnoseResult) -> bool {
    r.verdict == Verdict::Accepted
        && r.revert_latency_ms == Latencies::default()
        && r.timeline.iter().any(|e| e.source == TimelineSource::Us)
}

/// X-axis end in ms: the later of the last entry and the observation window.
pub fn axis_end(timeline: &[TimelineEntry], observe_ms: u64) -> f64 {
    timeline
        .iter()
        .map(|e| e.t_ms)
        .filter(|t| t.is_finite())
        .fold(observe_ms as f64, f64::max)
}

/// Position of `t` inside the view as a fraction 0..=1; `None` when it is outside.
pub fn frac_of(t: f64, view: (f64, f64)) -> Option<f32> {
    let span = view.1 - view.0;
    if span.is_nan() || span <= 0.0 {
        return None;
    }
    let eps = span * 1e-3;
    (t >= view.0 - eps && t <= view.1 + eps).then(|| ((t - view.0) / span).clamp(0.0, 1.0) as f32)
}

/// Round tick positions (1/2/5 × 10^k) inside `[vs, ve]`; never empty, at most `max_ticks + 1`.
pub fn axis_ticks(vs: f64, ve: f64, max_ticks: usize) -> Vec<f64> {
    let max_ticks = max_ticks.max(1);
    let span = ve - vs;
    if !span.is_finite() || span <= 0.0 {
        return vec![vs];
    }
    let raw = span / max_ticks as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    let first = (vs / step).ceil() as i64;
    let last = (ve / step).floor() as i64;
    let ticks: Vec<f64> = (first..=last)
        .map(|k| k as f64 * step)
        .filter(|t| *t >= vs && *t <= ve)
        .collect();
    if ticks.is_empty() {
        vec![vs, ve]
    } else {
        ticks
    }
}

/// Vertical latency lines: first style write + style latency, first rect write + rect latency.
pub fn latency_lines(timeline: &[TimelineEntry], l: &Latencies) -> Vec<(Row, f64)> {
    let first = |op: &str| {
        timeline
            .iter()
            .find(|e| e.source == TimelineSource::Us && e.op.as_deref() == Some(op))
            .map(|e| e.t_ms)
    };
    let mut out = Vec::new();
    if let (Some(t), Some(ms)) = (first("set_styles"), l.style) {
        out.push((Row::Style, t + ms));
    }
    if let (Some(t), Some(ms)) = (first("set_pos"), l.rect) {
        out.push((Row::Rect, t + ms));
    }
    out
}

/// Which labels fit: the first always, then greedy with a minimum gap; past `max_frac` hidden.
pub fn visible_labels(fracs: &[f32], min_gap: f32, max_frac: f32) -> Vec<bool> {
    let mut last: Option<f32> = None;
    fracs
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            let show = i == 0 || (f <= max_frac && last.is_none_or(|l| f - l >= min_gap));
            if show {
                last = Some(f);
            }
            show
        })
        .collect()
}

fn finite_or(x: f64, d: f64) -> f64 {
    if x.is_finite() { x } else { d }
}

/// Bring any view into `0 <= vs < ve <= total` with span `>= min(20, total)` (`total > 0`).
fn normalize(view: (f64, f64), total: f64) -> (f64, f64) {
    let total = finite_or(total, 1.0).max(f64::MIN_POSITIVE);
    let min_span = MIN_SPAN_MS.min(total);
    let span = finite_or(view.1 - view.0, total).clamp(min_span, total);
    let vs = finite_or(view.0, 0.0).clamp(0.0, total - span);
    (vs, vs + span)
}

/// Zoom by `factor` (> 1 zooms in) keeping the relative position of `center`.
pub fn zoom_view(view: (f64, f64), factor: f64, center: f64, total: f64) -> (f64, f64) {
    let (vs, ve) = normalize(view, total);
    let total = finite_or(total, 1.0).max(f64::MIN_POSITIVE);
    let factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };
    let span = (ve - vs) / factor;
    let span = span.clamp(MIN_SPAN_MS.min(total), total);
    let rel = ((finite_or(center, vs) - vs) / (ve - vs)).clamp(0.0, 1.0);
    normalize((center - rel * span, center - rel * span + span), total)
}

pub fn pan_view(view: (f64, f64), delta: f64, total: f64) -> (f64, f64) {
    let (vs, ve) = normalize(view, total);
    normalize(
        (vs + finite_or(delta, 0.0), ve + finite_or(delta, 0.0)),
        total,
    )
}

/// Scroll the view, if needed, so `t` is visible (centred).
pub fn reveal(view: (f64, f64), t: f64, total: f64) -> (f64, f64) {
    let (vs, ve) = normalize(view, total);
    if t >= vs && t <= ve {
        return (vs, ve);
    }
    let half = (ve - vs) / 2.0;
    normalize((t - half, t + half), total)
}

/// Next/previous marker index (`dir` > 0 forward). No wrap; nothing selected → first/last.
pub fn step_marker(cur: Option<usize>, n: usize, dir: i32) -> Option<usize> {
    if n == 0 {
        return None;
    }
    Some(match cur {
        None if dir > 0 => 0,
        None => n - 1,
        Some(c) if dir > 0 => (c + 1).min(n - 1),
        Some(c) => c.saturating_sub(1).min(n - 1),
    })
}

/// Everything the result cards and the chart need, derived once per run.
#[derive(Debug)]
pub struct Report {
    pub result: DiagnoseResult,
    pub dpi: Option<u32>,
    pub markers: Vec<Marker>,
    pub lines: Vec<(Row, f64)>,
    pub end_ms: f64,
    pub only_ours: bool,
}

impl Report {
    pub fn new(result: DiagnoseResult, observe_ms: u64, dpi: Option<u32>) -> Self {
        Self {
            markers: build_markers(&result.timeline),
            lines: latency_lines(&result.timeline, &result.revert_latency_ms),
            end_ms: axis_end(&result.timeline, observe_ms),
            only_ours: only_our_writes(&result),
            result,
            dpi,
        }
    }
}

// ------------------------------------------------------------------ IPC

type Lists = (Vec<WindowInfo>, ListIslandsResult);

fn load_lists(c: &mut Client) -> Result<Reply<Lists>, CallError> {
    let w = match get::<ListWindowsResult>(c, "list_windows", json!({}))? {
        Reply::Ok(w) => w.windows,
        Reply::Remote(b) => return Ok(Reply::Remote(b)),
    };
    Ok(lift(
        get::<ListIslandsResult>(c, "list_islands", json!({}))?,
        |i| (w, i),
    ))
}

// ------------------------------------------------------------------------ view

#[derive(Clone)]
struct Choice {
    title: SharedString,
    value: String,
    disabled: bool,
}

impl SearchableListItem for Choice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.title.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
    fn disabled(&self) -> bool {
        self.disabled
    }
}

type Choices = SearchableVec<Choice>;

fn window_choices(all: &[WindowInfo]) -> Vec<Choice> {
    let mut v: Vec<WindowInfo> = visible_windows(all, false, "", false)
        .into_iter()
        .cloned()
        .collect();
    sort_windows(&mut v);
    v.iter()
        .map(|w| {
            let elevated = w.elevated == Some(true);
            let title = format!(
                "{} — {}{}",
                w.process,
                truncate_chars(&w.title, TITLE_MAX),
                if elevated {
                    t("diagnose.elevated")
                } else {
                    Cow::Borrowed("")
                }
            );
            Choice {
                title: title.into(),
                value: w.hwnd.to_string(),
                disabled: elevated,
            }
        })
        .collect()
}

fn island_choices(isl: &ListIslandsResult) -> Vec<Choice> {
    let mut v = vec![Choice {
        title: t("diagnose.island_none").into_owned().into(),
        value: NO_ISLAND.into(),
        disabled: false,
    }];
    for l in &isl.layouts {
        for i in &l.islands {
            v.push(Choice {
                title: format!(
                    "{}/{} — {}",
                    l.id,
                    i.id,
                    format_size(i.rect.width(), i.rect.height())
                )
                .into(),
                value: format!("{}/{}", l.id, i.id),
                disabled: false,
            });
        }
    }
    v
}

enum Run {
    Idle,
    Running { started: Instant, observe_ms: u64 },
    Done(Box<Report>),
    Failed(Failure),
}

/// Emitted by "Use as the game's resolution"; the shell opens Profiles with it. Saves nothing.
pub struct UseGameResolution {
    pub process: String,
    pub width: i32,
    pub height: i32,
}

pub struct DiagnoseView {
    handle: AnyWindowHandle,
    win_infos: Vec<WindowInfo>,
    list_error: Option<Failure>,
    loading: bool,
    defaulted_island: bool,
    window_sel: Entity<SelectState<Choices>>,
    island_sel: Entity<SelectState<Choices>>,
    observe_sel: Entity<SliderState>,
    window_hwnd: Option<isize>,
    island: String,
    observe_ms: u64,
    keep: bool,
    run: Run,
    pending_hwnd: Option<isize>,
    view: (f64, f64),
    selected_marker: Option<usize>,
    hovered_marker: Option<usize>,
    chart_focus: FocusHandle,
    notice: Option<String>,
    last_render: Option<Instant>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<UseGameResolution> for DiagnoseView {}

impl DiagnoseView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let window_sel = cx.new(|cx| {
            SelectState::new(Choices::new(Vec::<Choice>::new()), None, window, cx).searchable(true)
        });
        let island_sel =
            cx.new(|cx| SelectState::new(Choices::new(Vec::<Choice>::new()), None, window, cx));
        let observe_sel = cx.new(|_| {
            SliderState::new()
                .min(MIN_OBSERVE_MS as f32)
                .max(MAX_OBSERVE_MS as f32)
                .step(OBSERVE_STEP_MS as f32)
                .default_value(DEFAULT_OBSERVE_MS as f32)
        });
        let subs = vec![
            cx.subscribe(&window_sel, |t, _, ev: &SelectEvent<Choices>, cx| {
                let SelectEvent::Confirm(v) = ev;
                t.window_hwnd = v.as_ref().and_then(|v| v.parse().ok());
                cx.notify();
            }),
            cx.subscribe(&island_sel, |t, _, ev: &SelectEvent<Choices>, cx| {
                let SelectEvent::Confirm(v) = ev;
                t.island = v.clone().unwrap_or_default();
                cx.notify();
            }),
            cx.subscribe(&observe_sel, |t, _, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                t.observe_ms = clamp_observe(v.start().max(0.0) as u64);
                cx.notify();
            }),
        ];
        Self {
            handle: window.window_handle(),
            win_infos: Vec::new(),
            list_error: None,
            loading: false,
            defaulted_island: false,
            window_sel,
            island_sel,
            observe_sel,
            window_hwnd: None,
            island: NO_ISLAND.into(),
            observe_ms: DEFAULT_OBSERVE_MS,
            keep: false,
            run: Run::Idle,
            pending_hwnd: None,
            view: (0.0, 1.0),
            selected_marker: None,
            hovered_marker: None,
            chart_focus: cx.focus_handle(),
            notice: None,
            last_render: None,
            _subs: subs,
        }
    }

    /// Hook for Windows "Diagnose…": preselect `hwnd` once the list is fresh.
    pub fn select_window(&mut self, hwnd: isize, cx: &mut Context<Self>) {
        self.pending_hwnd = Some(hwnd);
        self.refresh(cx);
    }

    fn running(&self) -> bool {
        matches!(self.run, Run::Running { .. })
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let handle = self.handle;
        cx.spawn(async move |this, cx| {
            let r = call(cx, load_lists).await;
            cx.update(|app| {
                handle
                    .update(app, |_, window, app| {
                        this.update(app, |t, cx| t.apply_lists(r, window, cx)).ok();
                    })
                    .ok();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_lists(
        &mut self,
        r: Result<Lists, Failure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match r {
            Err(f) => self.list_error = Some(f),
            Ok((wins, isl)) => {
                self.list_error = None;
                self.win_infos = wins;
                let wc = window_choices(&self.win_infos);
                self.window_sel
                    .update(cx, |s, cx| s.set_items(Choices::new(wc), window, cx));
                let ic = island_choices(&isl);
                let has_default = ic.iter().any(|c| c.value == DEFAULT_ISLAND);
                self.island_sel
                    .update(cx, |s, cx| s.set_items(Choices::new(ic), window, cx));
                if !self.defaulted_island {
                    self.defaulted_island = true;
                    self.island = if has_default {
                        DEFAULT_ISLAND
                    } else {
                        NO_ISLAND
                    }
                    .into();
                }
                let island = self.island.clone();
                self.island_sel
                    .update(cx, |s, cx| s.set_selected_value(&island, window, cx));
                if let Some(h) = self.pending_hwnd.take() {
                    self.window_sel
                        .update(cx, |s, cx| s.set_selected_value(&h.to_string(), window, cx));
                }
                self.window_hwnd = self
                    .window_sel
                    .read(cx)
                    .selected_value()
                    .and_then(|v| v.parse().ok());
            }
        }
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(hwnd) = self.window_hwnd.filter(|_| !self.running()) else {
            return;
        };
        let observe_ms = self.observe_ms;
        let mut params =
            json!({ "window": { "hwnd": hwnd }, "observe_ms": observe_ms, "keep": self.keep });
        if !self.island.is_empty() {
            params["island"] = json!(self.island);
        }
        let dpi = self
            .win_infos
            .iter()
            .find(|w| w.hwnd == hwnd)
            .map(|w| w.dpi);
        self.run = Run::Running {
            started: Instant::now(),
            observe_ms,
        };
        self.notice = None;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let Ok(running) = this.update(cx, |t, cx| {
                    let r = t.running();
                    if r {
                        cx.notify();
                    }
                    r
                }) else {
                    break;
                };
                if !running {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let r = call(cx, move |c| get::<DiagnoseResult>(c, "diagnose", params)).await;
            this.update(cx, |t, cx| {
                match r {
                    Ok(res) => {
                        let rep = Report::new(res, observe_ms, dpi);
                        t.view = (0.0, rep.end_ms);
                        t.selected_marker = None;
                        t.hovered_marker = None;
                        t.run = Run::Done(Box::new(rep));
                    }
                    Err(f) => t.run = Run::Failed(f),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn report(&self) -> Option<&Report> {
        match &self.run {
            Run::Done(r) => Some(r),
            _ => None,
        }
    }

    fn total(&self) -> f64 {
        self.report().map_or(1.0, |r| r.end_ms)
    }

    fn zoom(&mut self, factor: f64, cx: &mut Context<Self>) {
        let total = self.total();
        let center = self
            .selected_t()
            .unwrap_or((self.view.0 + self.view.1) / 2.0);
        self.view = zoom_view(self.view, factor, center, total);
        cx.notify();
    }

    fn pan(&mut self, dir: f64, cx: &mut Context<Self>) {
        let delta = dir * (self.view.1 - self.view.0) / 4.0;
        self.view = pan_view(self.view, delta, self.total());
        cx.notify();
    }

    fn selected_t(&self) -> Option<f64> {
        let r = self.report()?;
        r.markers.get(self.selected_marker?).map(|m| m.t_ms)
    }

    fn step(&mut self, dir: i32, cx: &mut Context<Self>) {
        let Some(n) = self.report().map(|r| r.markers.len()) else {
            return;
        };
        self.selected_marker = step_marker(self.selected_marker, n, dir);
        if let Some(t) = self.selected_t() {
            self.view = reveal(self.view, t, self.total());
        }
        cx.notify();
    }

    fn on_chart_key(&mut self, ev: &gpui_kit::KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let ch = k.key_char.as_deref().unwrap_or(&k.key);
        match (k.key.as_str(), ch) {
            ("left", _) => self.step(-1, cx),
            ("right", _) => self.step(1, cx),
            (_, "+" | "=") => self.zoom(2.0, cx),
            (_, "-") => self.zoom(0.5, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // -------------------------------------------------------------- rendering

    fn error_card(
        &self,
        f: &Failure,
        retry_id: &'static str,
        cx: &mut Context<Self>,
        retry: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let (sentence, code) = failure_text(f);
        card()
            .flex()
            .flex_col()
            .gap(SP_2)
            .child(div().text_color(danger()).child(sentence))
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(tf("diagnose.code_line", &[("code", &code)])),
            )
            .child(div().flex().child(secondary_button(
                retry_id,
                tr("diagnose.btn.retry"),
                cx.listener(move |t, _, _, cx| retry(t, cx)),
            )))
    }

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running();
        let field = |label: SharedString, body: gpui_kit::AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(SP_1)
                .child(
                    div()
                        .text_size(TEXT_SM)
                        .text_color(text_muted())
                        .child(label),
                )
                .child(body)
        };
        let window_row = div()
            .flex()
            .items_center()
            .gap(SP_2)
            .child(
                div().flex_1().min_w_0().child(
                    Select::new(&self.window_sel)
                        .placeholder(tr("diagnose.window_placeholder"))
                        .search_placeholder(tr("diagnose.window_search"))
                        .disabled(running),
                ),
            )
            .child(
                secondary_button(
                    "refresh",
                    tr("diagnose.btn.refresh"),
                    cx.listener(|t, _, _, cx| t.refresh(cx)),
                )
                .disabled(self.loading || running),
            );
        card()
            .flex()
            .flex_col()
            .gap(SP_3)
            .when_some(self.list_error.clone(), |d, f| {
                d.child(self.error_card(&f, "retry-lists", cx, |t, cx| t.refresh(cx)))
            })
            .child(field(tr("diagnose.window"), window_row.into_any_element()))
            .child(field(
                tr("diagnose.island"),
                Select::new(&self.island_sel)
                    .disabled(running)
                    .into_any_element(),
            ))
            .child(field(
                tr("diagnose.observe_for"),
                div()
                    .flex()
                    .items_center()
                    .gap(SP_4)
                    .child(
                        div().flex_1().child(
                            Slider::new(&self.observe_sel)
                                .horizontal()
                                .disabled(running),
                        ),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .text_right()
                            .font_features(tabular())
                            .child(format_ms(self.observe_ms as f64)),
                    )
                    .into_any_element(),
            ))
            .child(
                Checkbox::new("keep")
                    .label(tr("diagnose.keep"))
                    .checked(self.keep)
                    .on_click(cx.listener(|t, v: &bool, _, cx| {
                        t.keep = *v;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(SP_2)
                    .text_size(TEXT_SM)
                    .text_color(warn())
                    .child("⚠")
                    .child(div().flex_1().min_w_0().child(tr("diagnose.warning"))),
            )
            .child(
                div().child(
                    primary_button(
                        "diagnose",
                        if running {
                            tr("diagnose.btn.running")
                        } else {
                            tr("diagnose.btn.run")
                        },
                        cx.listener(|t, _, _, cx| t.start(cx)),
                    )
                    .disabled(self.window_hwnd.is_none() || running),
                ),
            )
    }

    fn progress(&self, started: Instant, observe_ms: u64) -> impl IntoElement {
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        card()
            .flex()
            .flex_col()
            .gap(SP_2)
            .child(div().font_features(tabular()).child(tf(
                "diagnose.observing",
                &[
                    ("elapsed", &format_ms(elapsed.min(observe_ms as f64))),
                    ("total", &format_ms(observe_ms as f64)),
                ],
            )))
            .child(
                div().h(SP_2).rounded(R_SM).bg(raised()).child(
                    div()
                        .h_full()
                        .rounded(R_SM)
                        .bg(accent())
                        .w(relative(observe_progress(elapsed, observe_ms))),
                ),
            )
    }

    fn verdict_card(&self, r: &Report) -> impl IntoElement {
        let res = &r.result;
        let (headline, badge_text, kind) = verdict_text(res.verdict);
        let dpi = match r.dpi {
            Some(d) => format!("{} · {}", dpi_percent(d), awareness_text(res.dpi_awareness)),
            None => awareness_text(res.dpi_awareness).into_owned(),
        };
        card()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_3)
                    .child(
                        div()
                            .min_w_0()
                            .text_size(TEXT_XL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(headline.into_owned()),
                    )
                    .child(badge(badge_text.into_owned(), kind)),
            )
            .child(div().text_color(text_muted()).child(tr(if res.restored {
                "diagnose.restored"
            } else {
                "diagnose.left_placed"
            })))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x(SP_6)
                    .gap_y(SP_3)
                    .child(stat(tr("diagnose.stat.target"), format_rect(&res.target)))
                    .child(stat(tr("diagnose.stat.final"), format_rect(&res.r#final)))
                    .child(stat(
                        tr("diagnose.stat.forced"),
                        res.forced_size
                            .map_or("—".to_owned(), |s| format_size(s.width, s.height)),
                    ))
                    .child(stat(tr("diagnose.stat.dpi"), dpi)),
            )
            .child(
                div().flex().flex_col().gap(SP_1).children(
                    latency_sentences(&res.revert_latency_ms)
                        .into_iter()
                        .map(|s| div().child(s)),
                ),
            )
    }

    fn recommendations_card(&self, r: &Report, cx: &mut Context<Self>) -> impl IntoElement {
        let res = &r.result;
        let all = recommendations_text(&res.recommendations, &res.process);
        let forced = res.forced_size;
        let process = res.process.clone();
        card()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("diagnose.recommendations")),
            )
            .when(res.recommendations.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(text_muted())
                        .child(tr("diagnose.no_recommendations")),
                )
            })
            .children(res.recommendations.iter().map(|rec| {
                div()
                    .flex()
                    .flex_col()
                    .gap(SP_1)
                    .child(div().child(recommendation_text(rec, &res.process)))
                    .child(
                        div()
                            .text_size(TEXT_SM)
                            .text_color(text_muted())
                            .child(rec.code.clone()),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_2)
                    .child(
                        secondary_button(
                            "copy",
                            tr("diagnose.btn.copy"),
                            cx.listener(move |t, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(all.clone()));
                                t.notice = Some(i18n::t("diagnose.copied").into_owned());
                                cx.notify();
                            }),
                        )
                        .disabled(res.recommendations.is_empty()),
                    )
                    .when_some(forced, |d, s| {
                        d.child(secondary_button(
                            "use-resolution",
                            tr("diagnose.btn.use_resolution"),
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(UseGameResolution {
                                    process: process.clone(),
                                    width: s.width,
                                    height: s.height,
                                });
                            }),
                        ))
                    }),
            )
            .when_some(self.notice.clone(), |d, n| {
                d.child(div().text_size(TEXT_SM).text_color(text_muted()).child(n))
            })
    }

    fn chart(&self, r: &Report, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.view;
        let plot_h = px(ROW_H * 3.0);
        let mut plot = div()
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(ROW_H * 3.0 + AXIS_H));
        for row in Row::ALL {
            plot = plot.child(
                div()
                    .absolute()
                    .top(px(row.index() as f32 * ROW_H + 2.0))
                    .left_0()
                    .w_full()
                    .h(px(ROW_H - 4.0))
                    .rounded(R_SM)
                    .bg(raised()),
            );
        }
        let ticks = axis_ticks(view.0, view.1, MAX_AXIS_TICKS);
        for t in ticks {
            let Some(f) = frac_of(t, view) else { continue };
            let label = if t.fract() == 0.0 {
                format!("{t:.0}")
            } else {
                fmt_decimal(t, 1)
            };
            plot = plot
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .h(plot_h)
                        .left(relative(f))
                        .w(px(1.))
                        .bg(text().opacity(0.06)),
                )
                .child(
                    div()
                        .absolute()
                        .top(plot_h)
                        .left(relative(f))
                        .ml(px(-24.))
                        .w(px(48.))
                        .text_center()
                        .text_size(TEXT_SM)
                        .text_color(text_muted())
                        .font_features(tabular())
                        .child(label),
                );
        }
        for &(_, t) in &r.lines {
            if let Some(f) = frac_of(t, view) {
                plot = plot.child(
                    div()
                        .absolute()
                        .top_0()
                        .h(plot_h)
                        .left(relative(f))
                        .w(px(2.))
                        .bg(warn()),
                );
            }
        }
        // Greedy label thinning per row, over the markers that are in view.
        let mut visible: Vec<(usize, f32)> = r
            .markers
            .iter()
            .enumerate()
            .filter_map(|(i, m)| Some((i, frac_of(m.t_ms, view)?)))
            .collect();
        visible.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut show = vec![false; r.markers.len()];
        for row in Row::ALL {
            let in_row: Vec<(usize, f32)> = visible
                .iter()
                .copied()
                .filter(|(i, _)| r.markers[*i].row == row)
                .collect();
            let fracs: Vec<f32> = in_row.iter().map(|p| p.1).collect();
            for ((i, _), s) in in_row.iter().zip(visible_labels(&fracs, 0.09, 0.88)) {
                show[*i] = s;
            }
        }
        for &(i, f) in &visible {
            let m = &r.markers[i];
            let (glyph, color) = match m.source {
                TimelineSource::Us => ("▲", accent()),
                TimelineSource::Window => ("●", warn()),
            };
            let active = self.selected_marker == Some(i);
            plot = plot.child(
                div()
                    .id(("marker", i))
                    .absolute()
                    .top(px(m.row.index() as f32 * ROW_H))
                    .left(relative(f))
                    .ml(px(-MARKER_W / 2.0))
                    .h(px(ROW_H))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(MARKER_W))
                            .h(px(ROW_H))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(R_SM)
                            .text_color(color)
                            .when(active, |d| d.bg(accent().opacity(0.18)))
                            .hover(|s| s.bg(text().opacity(0.06)))
                            .child(glyph),
                    )
                    .when(show[i] && !m.label.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(TEXT_SM)
                                .text_color(text_muted())
                                .whitespace_nowrap()
                                .font_features(tabular())
                                .child(m.label.clone()),
                        )
                    })
                    .on_hover(cx.listener(move |t, h: &bool, _, cx| {
                        if *h {
                            t.hovered_marker = Some(i);
                        } else if t.hovered_marker == Some(i) {
                            t.hovered_marker = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |t, _, window, cx| {
                        t.selected_marker = Some(i);
                        window.focus(&t.chart_focus, cx);
                        cx.notify();
                    })),
            );
        }
        let gutter = div()
            .w(px(GUTTER_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .children(Row::ALL.map(|row| {
                div()
                    .h(px(ROW_H))
                    .flex()
                    .items_center()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(row.label())
            }));
        let detail = self
            .hovered_marker
            .or(self.selected_marker)
            .and_then(|i| r.markers.get(i))
            .map(|m| m.tip.clone());
        let focus_ring = vec![BoxShadow {
            color: accent(),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(2.),
            inset: true,
        }];
        card()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_2)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("diagnose.timeline")),
                    )
                    .child(secondary_button(
                        "zoom-out",
                        tr("diagnose.btn.zoom_out"),
                        cx.listener(|t, _, _, cx| t.zoom(0.5, cx)),
                    ))
                    .child(secondary_button(
                        "zoom-in",
                        tr("diagnose.btn.zoom_in"),
                        cx.listener(|t, _, _, cx| t.zoom(2.0, cx)),
                    ))
                    .child(secondary_button(
                        "pan-left",
                        "◀",
                        cx.listener(|t, _, _, cx| t.pan(-1.0, cx)),
                    ))
                    .child(secondary_button(
                        "pan-right",
                        "▶",
                        cx.listener(|t, _, _, cx| t.pan(1.0, cx)),
                    ))
                    .child(secondary_button(
                        "zoom-reset",
                        tr("diagnose.btn.reset"),
                        cx.listener(|t, _, _, cx| {
                            t.view = (0.0, t.total());
                            cx.notify();
                        }),
                    )),
            )
            .when(r.only_ours, |d| {
                d.child(
                    div()
                        .flex()
                        .child(badge(tr("diagnose.no_reaction"), BadgeKind::Info)),
                )
            })
            .child(
                div()
                    .id("chart")
                    .track_focus(&self.chart_focus)
                    .tab_stop(true)
                    .rounded(R_SM)
                    .focus_visible(move |s| s.shadow(focus_ring))
                    .flex()
                    .on_key_down(cx.listener(|t, ev, _, cx| t.on_chart_key(ev, cx)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|t, _, window, cx| window.focus(&t.chart_focus, cx)),
                    )
                    .child(gutter)
                    .child(plot),
            )
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(tr("diagnose.legend")),
            )
            .child(
                div()
                    .p(SP_3)
                    .rounded(R_SM)
                    .bg(raised())
                    .text_size(TEXT_MD)
                    .when(detail.is_none(), |d| d.text_color(text_muted()))
                    .child(detail.unwrap_or_else(|| tr("diagnose.chart_hint").to_string())),
            )
    }

    fn table(&self, r: &Report) -> impl IntoElement {
        let tl = &r.result.timeline;
        let cell = |w: Option<f32>, s: String| {
            let d = div().min_w_0().font_features(tabular()).child(s);
            match w {
                Some(w) => d.w(px(w)).flex_shrink_0(),
                None => d.flex_1(),
            }
        };
        let head = |w: Option<f32>, s: SharedString| {
            cell(w, s.to_string())
                .text_size(TEXT_SM)
                .text_color(text_muted())
        };
        card()
            .flex()
            .flex_col()
            .gap(SP_2)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("diagnose.table.title")),
            )
            .child(
                div()
                    .flex()
                    .gap(SP_3)
                    .child(head(Some(80.), tr("diagnose.table.time")))
                    .child(head(Some(80.), tr("diagnose.table.origin")))
                    .child(head(Some(100.), tr("diagnose.table.op")))
                    .child(head(None, tr("diagnose.table.style")))
                    .child(head(Some(180.), tr("diagnose.table.rect"))),
            )
            .child(
                div()
                    .id("timeline-table")
                    .max_h(px(TABLE_MAX_H))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(SP_1)
                    .children(tl.iter().take(MAX_TABLE_ROWS).map(|e| {
                        div()
                            .flex()
                            .gap(SP_3)
                            .child(cell(Some(80.), format_ms(e.t_ms)))
                            .child(cell(Some(80.), origin_text(e.source).into()))
                            .child(cell(Some(100.), e.op.clone().unwrap_or_else(|| "—".into())))
                            .child(cell(None, format!("{} / {}", e.style, e.exstyle)))
                            .child(cell(Some(180.), format_rect(&e.rect)))
                    })),
            )
            .when(tl.len() > MAX_TABLE_ROWS, |d| {
                d.child(div().text_size(TEXT_SM).text_color(text_muted()).child(tf(
                    "diagnose.table.showing",
                    &[("max", &MAX_TABLE_ROWS), ("total", &tl.len())],
                )))
            })
    }
}

impl Render for DiagnoseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No visibility hook: a view that paints is visible, so refresh when it (re)appears.
        if !self.running()
            && !self.loading
            && self.last_render.is_none_or(|t| t.elapsed() > REFRESH_AFTER)
        {
            self.refresh(cx);
        }
        self.last_render = Some(Instant::now());

        let mut body = div()
            .flex()
            .flex_col()
            .gap(SP_4)
            .pb(SP_4)
            .child(self.controls(cx));
        // Move the run out so the card builders can borrow `self` freely; put it back below.
        let run = std::mem::replace(&mut self.run, Run::Idle);
        match &run {
            Run::Idle => {
                body = body.child(empty_state(
                    tr("diagnose.empty.title"),
                    tr("diagnose.empty.body"),
                ));
            }
            Run::Running {
                started,
                observe_ms,
            } => {
                body = body.child(self.progress(*started, *observe_ms));
            }
            Run::Failed(f) => {
                body = body.child(self.error_card(f, "retry-run", cx, |t, cx| t.start(cx)));
            }
            Run::Done(r) => {
                body = body
                    .child(self.verdict_card(r))
                    .child(self.recommendations_card(r, cx))
                    .child(self.chart(r, cx))
                    .child(self.table(r));
            }
        }
        self.run = run;
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(section_header(
                tr("diagnose.title"),
                tr("diagnose.subtitle"),
            ))
            .child(
                div()
                    .id("diagnose-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Locale, scoped_locale};
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;

    fn entry(
        t: f64,
        source: TimelineSource,
        op: Option<&str>,
        style: &str,
        ex: &str,
        r: [i32; 4],
    ) -> TimelineEntry {
        TimelineEntry {
            t_ms: t,
            source,
            op: op.map(str::to_owned),
            style: style.into(),
            exstyle: ex.into(),
            rect: Rect {
                left: r[0],
                top: r[1],
                right: r[2],
                bottom: r[3],
            },
        }
    }
    fn us(t: f64, op: &str) -> TimelineEntry {
        entry(t, TimelineSource::Us, Some(op), "0x0", "0x0", [0; 4])
    }
    fn win(t: f64, style: &str, ex: &str, r: [i32; 4]) -> TimelineEntry {
        entry(t, TimelineSource::Window, None, style, ex, r)
    }
    fn rows(m: &[Marker]) -> Vec<Row> {
        m.iter().map(|m| m.row).collect()
    }

    #[test]
    fn verdict_texts_are_distinct_and_only_accepted_is_ok() {
        let _g = scoped_locale(Locale::En);
        let all = [
            Verdict::Accepted,
            Verdict::StyleReverted,
            Verdict::SizeForced,
            Verdict::PositionForced,
            Verdict::StyleAndSizeReverted,
            Verdict::LateRevert,
        ];
        let texts: Vec<_> = all.iter().map(|v| verdict_text(*v)).collect();
        for (i, a) in texts.iter().enumerate() {
            for b in &texts[i + 1..] {
                assert_ne!(a.0, b.0);
                assert_ne!(a.1, b.1);
            }
        }
        assert_eq!(
            texts[4].0,
            "The game fights back: it reverts the frame and its size"
        );
        assert!(texts.iter().filter(|t| t.2 == BadgeKind::Ok).count() == 1);
        assert!(texts[0].2 == BadgeKind::Ok);
    }

    #[test]
    fn the_verdict_chip_never_repeats_the_headline() {
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            for v in [
                Verdict::Accepted,
                Verdict::StyleReverted,
                Verdict::SizeForced,
                Verdict::PositionForced,
                Verdict::StyleAndSizeReverted,
                Verdict::LateRevert,
            ] {
                let (headline, badge, _) = verdict_text(v);
                let (h, b) = (headline.to_lowercase(), badge.to_lowercase());
                assert!(!h.contains(&b), "{v:?} {l:?}: {badge} / {headline}");
            }
        }
    }

    #[test]
    fn verdict_texts_exist_in_spanish() {
        let en = {
            let _g = scoped_locale(Locale::En);
            verdict_text(Verdict::StyleAndSizeReverted).0
        };
        let _g = scoped_locale(Locale::Es);
        let es = verdict_text(Verdict::StyleAndSizeReverted).0;
        assert_ne!(es, en);
        assert!(!es.starts_with("diagnose."), "{es}");
    }

    #[test]
    fn format_ms_covers_ranges_and_never_panics() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(format_ms(0.0), "0 ms");
        assert_eq!(format_ms(4.6), "4.6 ms");
        assert_eq!(format_ms(60.0), "60 ms");
        assert_eq!(format_ms(59.6), "60 ms");
        assert_eq!(format_ms(1250.0), "1.25 s");
        assert_eq!(format_ms(2000.0), "2 s");
        assert_eq!(format_ms(1500.0), "1.5 s");
        assert_eq!(format_ms(999.7), "1 s");
        assert_eq!(format_ms(-3.0), "0 ms");
        assert_eq!(format_ms(f64::NAN), "0 ms");
        assert_eq!(format_ms(f64::INFINITY), "0 ms");
    }

    #[test]
    fn format_ms_uses_the_decimal_comma_in_spanish() {
        let _g = scoped_locale(Locale::Es);
        assert_eq!(format_ms(4.6), "4,6 ms");
        assert_eq!(format_ms(1250.0), "1,25 s");
        assert_eq!(format_ms(1500.0), "1,5 s");
        assert_eq!(format_ms(2000.0), "2 s");
    }

    #[test]
    fn axis_labels_follow_the_locale_separator() {
        let _g = scoped_locale(Locale::Es);
        assert_eq!(fmt_decimal(2.5, 1), "2,5");
        drop(_g);
        let _g = scoped_locale(Locale::En);
        assert_eq!(fmt_decimal(2.5, 1), "2.5");
    }

    #[test]
    fn latency_sentences_cases() {
        let _g = scoped_locale(Locale::En);
        let l = |style, rect| Latencies { style, rect };
        assert_eq!(
            latency_sentences(&l(Some(60.0), None)),
            ["Restores the frame after 60 ms"]
        );
        assert_eq!(
            latency_sentences(&l(None, Some(1250.0))),
            ["Changes size or position after 1.25 s"]
        );
        assert_eq!(latency_sentences(&l(Some(4.6), Some(80.0))).len(), 2);
        assert_eq!(latency_sentences(&l(None, None)), ["No revert"]);
    }

    #[test]
    fn size_rect_and_dpi_formats() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(format_size(1280, 720), "1280x720");
        let r = Rect {
            left: -1920,
            top: 0,
            right: -640,
            bottom: 720,
        };
        assert_eq!(format_rect(&r), "1280x720 at -1920,0");
        assert_eq!(dpi_percent(120), "125%");
        assert_eq!(dpi_percent(96), "100%");
        assert_eq!(awareness_text(DpiAwareness::PerMonitorV2), "Per-monitor v2");
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        assert_eq!(format_rect(&r), "1280x720 en -1920,0");
        assert_eq!(dpi_percent(120), "125 %");
        assert_eq!(awareness_text(DpiAwareness::PerMonitorV2), "Por monitor v2");
    }

    #[test]
    fn observe_progress_is_bounded() {
        assert_eq!(observe_progress(0.0, 3000), 0.0);
        assert!((observe_progress(2250.0, 3000) - 0.5).abs() < 1e-6);
        assert_eq!(observe_progress(1e9, 3000), 0.97);
        assert!(observe_progress(100.0, 0).is_finite());
    }

    #[test]
    fn clamp_observe_rounds_to_step_and_clamps() {
        assert_eq!(clamp_observe(0), 500);
        assert_eq!(clamp_observe(700), 500);
        assert_eq!(clamp_observe(750), 1000);
        assert_eq!(clamp_observe(3000), 3000);
        assert_eq!(clamp_observe(99_999), 15_000);
    }

    #[test]
    fn markers_only_our_writes_go_to_the_writes_row() {
        let m = build_markers(&[us(1.0, "set_styles"), us(2.0, "set_pos")]);
        assert_eq!(rows(&m), [Row::Writes, Row::Writes]);
        assert_eq!(m[0].label, "set_styles");
    }

    #[test]
    fn markers_first_window_entry_is_the_baseline_with_both_rows() {
        let _g = scoped_locale(Locale::En);
        let m = build_markers(&[win(0.0, "0x1", "0x2", [0, 0, 10, 10])]);
        assert_eq!(rows(&m), [Row::Style, Row::Rect]);
        assert!(m[0].tip.starts_with("Initial state"));
        assert_eq!(m[0].label, "0x1"); // D2: the baseline shows the style, not the exstyle
    }

    #[test]
    fn markers_identical_window_entry_collapses() {
        let w = win(0.0, "0x1", "0x2", [0, 0, 10, 10]);
        let again = win(5.0, "0x1", "0x2", [0, 0, 10, 10]);
        assert_eq!(build_markers(&[w, again]).len(), 2);
    }

    #[test]
    fn markers_rect_only_and_exstyle_only_changes() {
        let _g = scoped_locale(Locale::En);
        let a = win(0.0, "0x1", "0x2", [0, 0, 10, 10]);
        let rect = win(5.0, "0x1", "0x2", [0, 0, 20, 10]);
        let m = build_markers(&[a.clone(), rect]);
        assert_eq!(rows(&m)[2..], [Row::Rect]);
        assert_eq!(m[2].label, "20x10 at 0,0");
        let ex = win(5.0, "0x1", "0x4", [0, 0, 10, 10]);
        let m = build_markers(&[a, ex]);
        assert_eq!(rows(&m)[2..], [Row::Style]);
        assert_eq!(m[2].label, "ex 0x4");
    }

    #[test]
    fn markers_our_entries_do_not_pollute_the_previous_window_state() {
        let a = win(0.0, "0x1", "0x2", [0, 0, 10, 10]);
        let ours = us(2.0, "set_pos"); // default rect/style differ from the window's
        let same = win(5.0, "0x1", "0x2", [0, 0, 10, 10]);
        let m = build_markers(&[a, ours, same]);
        assert_eq!(rows(&m), [Row::Style, Row::Rect, Row::Writes]);
    }

    #[test]
    fn markers_are_sorted_by_time() {
        let m = build_markers(&[
            us(9.0, "set_pos"),
            win(1.0, "0x1", "0x2", [0; 4]),
            us(4.0, "set_styles"),
        ]);
        assert!(m.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
    }

    #[test]
    fn only_our_writes_cases() {
        let ok = Latencies::default();
        let base = win(0.0, "0x1", "0x0", [5; 4]);
        // nothing written by us: nothing to say.
        assert!(!only_our_writes(&result(Verdict::Accepted, ok, vec![])));
        assert!(!only_our_writes(&result(
            Verdict::Accepted,
            ok,
            vec![base.clone()]
        )));
        // the engine saw no reaction to our write.
        let tl = vec![base, us(1.0, "set_pos")];
        assert!(only_our_writes(&result(Verdict::Accepted, ok, tl.clone())));
        // any other verdict or a revert latency = the window reacted.
        for v in [
            Verdict::StyleReverted,
            Verdict::SizeForced,
            Verdict::PositionForced,
            Verdict::StyleAndSizeReverted,
            Verdict::LateRevert,
        ] {
            assert!(!only_our_writes(&result(v, ok, tl.clone())), "{v:?}");
        }
        let rect = Latencies {
            style: None,
            rect: Some(80.0),
        };
        assert!(!only_our_writes(&result(Verdict::Accepted, rect, tl)));
    }

    fn result(verdict: Verdict, l: Latencies, timeline: Vec<TimelineEntry>) -> DiagnoseResult {
        let r = Rect {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        DiagnoseResult {
            hwnd: 1,
            process: "game.exe".into(),
            target: r,
            verdict,
            timeline,
            r#final: r,
            forced_size: None,
            revert_latency_ms: l,
            dpi_awareness: DpiAwareness::PerMonitorV2,
            recommendations: vec![],
            restored: true,
        }
    }

    // D1: the game removed and restored the frame between two samples, so our entry carries
    // the reverted state; the banner must follow the engine verdict, not the inferred states.
    #[test]
    fn no_reaction_banner_follows_the_engine_verdict() {
        let r = [0, 0, 1280, 720];
        let ours = entry(
            10.0,
            TimelineSource::Us,
            Some("set_styles"),
            "0x14ca0000",
            "0x0",
            r,
        );
        let tl = vec![win(0.0, "0x14ca0000", "0x0", r), ours];
        let reverted = Latencies {
            style: Some(4.0),
            rect: None,
        };
        let rep = |v, l| Report::new(result(v, l, tl.clone()), 3000, None).only_ours;
        assert!(!rep(Verdict::StyleReverted, reverted));
        assert!(!rep(Verdict::Accepted, reverted));
        assert!(rep(Verdict::Accepted, Latencies::default()));
    }

    #[test]
    fn axis_end_is_the_later_of_last_entry_and_window() {
        assert_eq!(axis_end(&[], 3000), 3000.0);
        assert_eq!(axis_end(&[us(4200.0, "set_pos")], 3000), 4200.0);
        assert_eq!(axis_end(&[us(100.0, "set_pos")], 3000), 3000.0);
    }

    #[test]
    fn frac_of_cases() {
        assert_eq!(frac_of(0.0, (0.0, 100.0)), Some(0.0));
        assert_eq!(frac_of(100.0, (0.0, 100.0)), Some(1.0));
        assert_eq!(frac_of(50.0, (0.0, 100.0)), Some(0.5));
        assert_eq!(frac_of(150.0, (0.0, 100.0)), None);
        assert_eq!(frac_of(-5.0, (0.0, 100.0)), None);
        assert_eq!(frac_of(5.0, (5.0, 5.0)), None);
        // D3: a hair past either edge (float noise) snaps to the edge.
        assert_eq!(frac_of(-0.05, (0.0, 100.0)), Some(0.0));
        assert_eq!(frac_of(100.05, (0.0, 100.0)), Some(1.0));
    }

    #[test]
    fn axis_ticks_cases() {
        let t = axis_ticks(0.0, 3000.0, 8);
        assert_eq!(&t[..3], &[0.0, 500.0, 1000.0]);
        assert_eq!(axis_ticks(0.0, 20.0, 8), [0.0, 5.0, 10.0, 15.0, 20.0]);
        assert_eq!(axis_ticks(3.0, 4.0, 1), [3.0, 4.0]);
        assert!(!axis_ticks(7.0, 7.0, 8).is_empty());
    }

    #[test]
    fn latency_lines_add_to_the_first_write_of_the_right_kind() {
        let tl = [
            us(10.0, "set_styles"),
            us(12.0, "set_pos"),
            us(50.0, "set_pos"),
        ];
        let l = Latencies {
            style: Some(60.0),
            rect: Some(100.0),
        };
        assert_eq!(
            latency_lines(&tl, &l),
            [(Row::Style, 70.0), (Row::Rect, 112.0)]
        );
        let none = Latencies {
            style: None,
            rect: Some(1.0),
        };
        assert_eq!(latency_lines(&tl, &none), [(Row::Rect, 13.0)]);
        assert!(latency_lines(&[], &l).is_empty());
    }

    #[test]
    fn visible_labels_respect_gap_first_and_max_frac() {
        let v = visible_labels(&[0.0, 0.05, 0.2, 0.25, 0.9], 0.09, 0.88);
        assert_eq!(v, [true, false, true, false, false]);
        assert_eq!(visible_labels(&[0.95], 0.09, 0.88), [true]);
        assert!(visible_labels(&[], 0.09, 0.88).is_empty());
        // D4: a gap of exactly `min_gap` fits; D5: a label exactly at `max_frac` shows.
        assert_eq!(visible_labels(&[0.25, 0.5], 0.25, 1.0), [true, true]);
        assert_eq!(visible_labels(&[0.0, 0.5], 0.1, 0.5), [true, true]);
    }

    #[test]
    fn zoom_in_keeps_the_center_and_stays_in_range() {
        let (vs, ve) = zoom_view((0.0, 1000.0), 2.0, 500.0, 1000.0);
        assert!(((vs + ve) / 2.0 - 500.0).abs() < 1e-9);
        assert!((ve - vs - 500.0).abs() < 1e-9);
        assert_eq!(zoom_view((0.0, 1000.0), 0.5, 500.0, 1000.0), (0.0, 1000.0));
        let (vs, ve) = zoom_view((0.0, 1000.0), 1e9, 0.0, 1000.0);
        assert_eq!((vs, ve - vs), (0.0, 20.0));
        // D6: a center outside the view anchors the zoom to the nearest edge of the view.
        assert_eq!(
            zoom_view((100.0, 200.0), 2.0, 500.0, 1000.0),
            (450.0, 500.0)
        );
        assert_eq!(zoom_view((500.0, 600.0), 2.0, 0.0, 1000.0), (0.0, 50.0));
    }

    #[test]
    fn pan_and_reveal_clamp() {
        assert_eq!(pan_view((100.0, 200.0), -500.0, 1000.0), (0.0, 100.0));
        assert_eq!(pan_view((100.0, 200.0), 5000.0, 1000.0), (900.0, 1000.0));
        assert_eq!(reveal((0.0, 100.0), 50.0, 1000.0), (0.0, 100.0));
        let (vs, ve) = reveal((0.0, 100.0), 600.0, 1000.0);
        assert!(vs <= 600.0 && 600.0 <= ve);
        assert!(ve <= 1000.0);
        // D7: a time exactly on either edge is already visible: no recentre.
        assert_eq!(reveal((0.0, 100.0), 100.0, 1000.0), (0.0, 100.0));
        assert_eq!(reveal((200.0, 300.0), 200.0, 1000.0), (200.0, 300.0));
    }

    #[test]
    fn step_marker_cases() {
        assert_eq!(step_marker(None, 3, 1), Some(0));
        assert_eq!(step_marker(None, 3, -1), Some(2));
        assert_eq!(step_marker(Some(2), 3, 1), Some(2));
        assert_eq!(step_marker(Some(0), 3, -1), Some(0));
        assert_eq!(step_marker(Some(1), 3, 1), Some(2));
        assert_eq!(step_marker(Some(1), 0, 1), None);
        assert_eq!(step_marker(Some(9), 3, -1), Some(2)); // D8: stale index past the end
    }

    #[test]
    fn recommendations_text_joins_with_blank_lines() {
        let r = |t: &str| Recommendation {
            code: "c".into(),
            text: t.into(),
            data: None,
        };
        assert_eq!(recommendations_text(&[r("a"), r("b")], "g.exe"), "a\n\nb");
        assert_eq!(recommendations_text(&[], "g.exe"), "");
    }

    fn rec(code: &str, data: Option<Value>) -> Recommendation {
        Recommendation {
            code: code.into(),
            text: "engine english".into(),
            data,
        }
    }

    #[test]
    fn recommendations_render_from_code_and_data_in_english_and_spanish() {
        let cases: [(&str, Value, &[&str]); 5] = [
            (
                "style_reverted",
                json!({ "max_reasserts": 3 }),
                &["g.exe restores the window frame", "up to 3 times"],
            ),
            (
                "position_forced",
                json!({}),
                &["g.exe moves the window back"],
            ),
            (
                "warscape_resolution",
                json!({ "x_res": 2048, "y_res": 1152, "island_width": 2560, "island_height": 1440, "dpi_unaware": true, "dpi": 120 }),
                &[
                    "x_res 2048 and y_res 1152",
                    "DPI-unaware",
                    "(2560x1440)",
                    "(this monitor is at 125%, dpi 120: 2048x1152)",
                ],
            ),
            (
                "unity_args",
                json!({ "width": 2560, "height": 1440 }),
                &["-screen-width 2560 -screen-height 1440"],
            ),
            (
                "game_resolution",
                json!({ "width": 1280, "height": 720 }),
                &[
                    "{\"width\": 1280, \"height\": 720}",
                    "centred in the island",
                ],
            ),
        ];
        let render = |locale| {
            let _g = scoped_locale(locale);
            cases
                .iter()
                .map(|(code, data, _)| recommendation_text(&rec(code, Some(data.clone())), "g.exe"))
                .collect::<Vec<_>>()
        };
        let (en, es) = (render(Locale::En), render(Locale::Es));
        for (i, (code, _, parts)) in cases.iter().enumerate() {
            for part in *parts {
                assert!(en[i].contains(part), "en {code}: {}", en[i]);
            }
            // Spanish: same numbers and process, different sentence, no raw placeholders.
            assert_ne!(es[i], en[i], "{code}");
            assert_ne!(es[i], "engine english", "{code}");
            assert!(
                es[i].contains("g.exe") || !en[i].contains("g.exe"),
                "{}",
                es[i]
            );
            assert!(
                !es[i].contains("{x_res}") && !es[i].contains("{process}"),
                "{}",
                es[i]
            );
        }
        assert!(es[0].contains(" 3 "), "{}", es[0]);
        assert!(
            es[2].contains("x_res 2048") && es[2].contains("125 %, dpi 120: 2048x1152"),
            "{}",
            es[2]
        );
        assert!(
            es[3].contains("-screen-width 2560 -screen-height 1440"),
            "{}",
            es[3]
        );
        assert!(
            es[4].contains("{\"width\": 1280, \"height\": 720}"),
            "{}",
            es[4]
        );
    }

    #[test]
    fn warscape_without_dpi_awareness_uses_the_plain_island_size() {
        let _g = scoped_locale(Locale::En);
        let data = json!({ "x_res": 2560, "y_res": 1440, "island_width": 2560, "island_height": 1440, "dpi_unaware": false, "dpi": 96 });
        let text = recommendation_text(&rec("warscape_resolution", Some(data)), "g.exe");
        assert!(text.contains("(use the island size (2560x1440))"), "{text}");
    }

    #[test]
    fn unknown_code_or_missing_data_falls_back_to_the_engine_text() {
        let _g = scoped_locale(Locale::Es);
        assert_eq!(
            recommendation_text(&rec("brand_new_code", None), "g.exe"),
            "engine english"
        );
        assert_eq!(
            recommendation_text(&rec("unity_args", None), "g.exe"),
            "engine english"
        );
    }

    #[test]
    fn entry_tip_mentions_time_origin_styles_and_rect() {
        let _g = scoped_locale(Locale::En);
        let tip = entry_tip(&entry(
            12.0,
            TimelineSource::Us,
            Some("set_pos"),
            "0x94",
            "0x8",
            [1, 2, 11, 12],
        ));
        for part in ["12 ms", "Us", "set_pos", "0x94", "0x8", "10x10 at 1,2"] {
            assert!(tip.contains(part), "{tip}");
        }
    }

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0xB05EED),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(cfg())]

        // (1) zoom/pan always return a valid, sufficiently wide view.
        #[test]
        fn views_stay_valid(
            total in 1.0f64..60_000.0,
            a in -1e5f64..1e5,
            b in -1e5f64..1e5,
            factor in 0.01f64..100.0,
            center in -1e5f64..1e5,
            delta in -1e5f64..1e5,
        ) {
            let min = MIN_SPAN_MS.min(total);
            for (vs, ve) in [
                zoom_view((a, b), factor, center, total),
                pan_view((a, b), delta, total),
                reveal((a, b), center, total),
            ] {
                prop_assert!(0.0 <= vs && vs < ve && ve <= total, "{vs}..{ve} of {total}");
                prop_assert!(ve - vs >= min - 1e-9, "{vs}..{ve}");
            }
        }

        // (2) ticks are non-empty, sorted, inside the view and few.
        #[test]
        fn ticks_are_sorted_inside_and_bounded(
            vs in 0.0f64..50_000.0,
            span in 0.5f64..50_000.0,
            max_ticks in 1usize..16,
        ) {
            let ve = vs + span;
            let t = axis_ticks(vs, ve, max_ticks);
            prop_assert!(!t.is_empty());
            prop_assert!(t.len() <= max_ticks + 1, "{} ticks", t.len());
            prop_assert!(t.windows(2).all(|w| w[0] <= w[1]));
            prop_assert!(t.iter().all(|x| *x >= vs && *x <= ve));
        }
    }
}
