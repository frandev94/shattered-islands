//! "Windows": polled `list_windows` list with place / restore actions.
//!
//! Pure logic (filter, row model, warning texts, diff) on top, with tests; the GPUI view below
//! only paints it. Polling runs every 1.5 s and pauses while the view is not painted.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui_kit::component::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Div, Entity, EventEmitter, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Window,
    div, px,
};
use serde_json::json;

use super::Failure;
use super::monitor_preview::window_matches;
use super::theme::{
    BadgeKind, HIT, R_SM, SP_1, SP_2, SP_3, SP_4, TEXT_SM, badge, card, danger, empty_state,
    raised, secondary_button, section_header, tabular, text, text_muted,
};
use super::{Reply, call, failure_msg, failure_text, get, lift, notice_view};
use crate::i18n::{self, Locale};
use crate::ipc::pipe::Client;
use crate::ipc::{
    ApplyResult, CallError, ListIslandsResult, ListWindowsResult, RestoreAllResult, RestoreResult,
    WindowInfo,
};
use crate::profiles::Warning;
use crate::win32::DpiAwareness;

const POLL: Duration = Duration::from_millis(1500);
/// The view counts as visible if it painted within this long (polling pauses otherwise).
const VISIBLE_GRACE: Duration = Duration::from_secs(4);
const MESSAGE_TTL: Duration = Duration::from_secs(8);
/// Narrower than this the row's actions wrap under the text instead of squeezing it.
const INFO_MIN_W: gpui_kit::Pixels = px(260.);
const SEARCH_W: gpui_kit::Pixels = px(280.);

// ------------------------------------------------------------------ pure logic

/// Rows for the list: visible, titled, non-excluded windows (`show_all` keeps everything) that
/// match `query` (process or title) and, with `only_managed`, are managed.
pub fn visible_windows<'a>(
    all: &'a [WindowInfo],
    show_all: bool,
    query: &str,
    only_managed: bool,
) -> Vec<&'a WindowInfo> {
    all.iter()
        .filter(|w| {
            (show_all
                || (w.visible
                    && !w.title.trim().is_empty()
                    && !crate::profiles::is_excluded(&w.process)))
                && (!only_managed || w.managed)
                && window_matches(query, &w.process, &w.title)
        })
        .collect()
}

/// Title of the empty state: "Managed only" without a query says nothing is managed yet.
pub fn empty_title(query: &str, only_managed: bool) -> String {
    if only_managed && query.trim().is_empty() {
        i18n::t("windows.empty.none_managed.title").into_owned()
    } else {
        i18n::t("windows.empty.no_match.title").into_owned()
    }
}

fn empty_hint(query: &str, only_managed: bool) -> String {
    if only_managed && query.trim().is_empty() {
        i18n::t("windows.empty.none_managed.hint").into_owned()
    } else {
        i18n::t("windows.empty.no_match.hint").into_owned()
    }
}

/// Stable order (process, title, hwnd) so a poll never reshuffles rows.
pub fn sort_windows(v: &mut [WindowInfo]) {
    v.sort_by_cached_key(|w| (w.process.to_lowercase(), w.title.to_lowercase(), w.hwnd));
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WindowsDiff {
    pub added: Vec<isize>,
    pub removed: Vec<isize>,
    pub changed: Vec<isize>,
}

impl WindowsDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

pub fn diff_windows(old: &[WindowInfo], new: &[WindowInfo]) -> WindowsDiff {
    let mut d = WindowsDiff::default();
    for n in new {
        match old.iter().find(|o| o.hwnd == n.hwnd) {
            None => d.added.push(n.hwnd),
            Some(o) if o != n => d.changed.push(n.hwnd),
            Some(_) => {}
        }
    }
    d.removed = old
        .iter()
        .filter(|o| !new.iter().any(|n| n.hwnd == o.hwnd))
        .map(|o| o.hwnd)
        .collect();
    d
}

/// Cut to `max` chars, ending in an ellipsis when shortened.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn dpi_label(dpi: u32) -> String {
    let pct = (dpi as f64 / 96.0 * 100.0).round() as u32;
    i18n::tf("windows.dpi.percent", &[("pct", &pct)])
}

pub fn awareness_label(a: DpiAwareness) -> String {
    match a {
        DpiAwareness::Unaware => i18n::t("windows.dpi.unaware"),
        DpiAwareness::System => i18n::t("windows.dpi.system"),
        DpiAwareness::PerMonitor => i18n::t("windows.dpi.per_monitor"),
        DpiAwareness::PerMonitorV2 => i18n::t("windows.dpi.per_monitor_v2"),
        DpiAwareness::Unknown => i18n::t("windows.dpi.unknown"),
    }
    .into_owned()
}

/// Everything a row needs, already in the active language.
#[derive(Debug, PartialEq, Eq)]
pub struct RowModel {
    pub hwnd: isize,
    pub process: String,
    pub title: String,
    pub managed: bool,
    pub state: String,
    pub island: Option<String>,
    pub dpi: String,
    pub awareness: String,
    pub maximized: bool,
    pub minimized: bool,
    pub elevated: bool,
    pub fights_back: bool,
    pub can_apply: bool,
    pub can_place: bool,
    pub can_restore: bool,
    pub can_diagnose: bool,
}

pub fn row_model(w: &WindowInfo, fights_back: bool) -> RowModel {
    let elevated = w.elevated == Some(true);
    RowModel {
        hwnd: w.hwnd,
        process: w.process.clone(),
        title: w.title.clone(),
        managed: w.managed,
        state: if w.managed {
            i18n::t("windows.row.managed").into_owned()
        } else {
            i18n::t("windows.row.unmanaged").into_owned()
        },
        island: w.island.clone(),
        dpi: dpi_label(w.dpi),
        awareness: awareness_label(w.dpi_awareness),
        maximized: w.maximized,
        minimized: w.minimized,
        elevated,
        fights_back,
        can_apply: !elevated,
        can_place: !elevated,
        can_restore: w.managed && !elevated,
        can_diagnose: !elevated,
    }
}

fn num(d: &serde_json::Value, k: &str) -> Option<i64> {
    d.get(k).and_then(|v| v.as_i64())
}

/// Plain text for an engine warning; unknown codes fall back to the code itself.
pub fn warning_text(w: &Warning) -> String {
    let d = &w.data;
    match w.code.as_str() {
        "size_forced" => match (num(d, "w"), num(d, "h")) {
            (Some(w), Some(h)) => i18n::tf("windows.warning.size_forced", &[("w", &w), ("h", &h)]),
            _ => i18n::t("windows.warning.size_forced_unknown").into_owned(),
        },
        "position_forced" => match (num(d, "x"), num(d, "y")) {
            (Some(x), Some(y)) => {
                i18n::tf("windows.warning.position_forced", &[("x", &x), ("y", &y)])
            }
            _ => i18n::t("windows.warning.position_forced_unknown").into_owned(),
        },
        "style_reverted" => i18n::t("windows.warning.style_reverted").into_owned(),
        "game_resolution_exceeds_island" => {
            i18n::t("windows.warning.game_resolution_exceeds_island").into_owned()
        }
        "fights_back" => match d
            .get("forced")
            .and_then(|f| Some((num(f, "w")?, num(f, "h")?)))
        {
            Some((w, h)) => i18n::tf("windows.warning.fights_back", &[("w", &w), ("h", &h)]),
            None => i18n::t("windows.warning.fights_back_unknown").into_owned(),
        },
        other => other.to_owned(),
    }
}

pub enum ActionResult {
    Apply(ApplyResult),
    Place(ApplyResult),
    Restore(RestoreResult),
    RestoreAll(RestoreAllResult),
}

#[derive(PartialEq, Eq)]
pub struct Msg {
    pub kind: BadgeKind,
    pub text: String,
    pub detail: Vec<String>,
}

fn msg(kind: BadgeKind, text: impl Into<String>, detail: Vec<String>) -> Msg {
    Msg {
        kind,
        text: text.into(),
        detail,
    }
}

pub fn describe(r: &ActionResult) -> Msg {
    let t = |key: &str| i18n::t(key).into_owned();
    match r {
        ActionResult::Apply(a) | ActionResult::Place(a) if !a.applied => {
            match a.reason.as_deref() {
                Some("already_placed") => {
                    msg(BadgeKind::Info, t("windows.result.already_placed"), vec![])
                }
                Some("excluded") => msg(BadgeKind::Info, t("windows.result.excluded"), vec![]),
                _ => msg(BadgeKind::Info, t("windows.result.no_change"), vec![]),
            }
        }
        ActionResult::Apply(a) | ActionResult::Place(a) if !a.warnings.is_empty() => msg(
            BadgeKind::Warn,
            if matches!(r, ActionResult::Apply(_)) {
                t("windows.result.applied_warnings")
            } else {
                t("windows.result.placed_warnings")
            },
            a.warnings.iter().map(warning_text).collect(),
        ),
        ActionResult::Apply(_) => msg(BadgeKind::Ok, t("windows.result.applied"), vec![]),
        ActionResult::Place(_) => msg(BadgeKind::Ok, t("windows.result.placed"), vec![]),
        ActionResult::Restore(r) if !r.restored => msg(
            BadgeKind::Info,
            t("windows.result.nothing_to_restore"),
            vec![],
        ),
        ActionResult::Restore(r) if !r.exact => msg(
            BadgeKind::Warn,
            t("windows.result.restored_inexact"),
            vec![],
        ),
        ActionResult::Restore(_) => msg(BadgeKind::Ok, t("windows.result.restored"), vec![]),
        ActionResult::RestoreAll(a) if a.failed.is_empty() => msg(
            BadgeKind::Ok,
            match a.restored {
                0 => t("windows.result.none_managed"),
                n => i18n::tn("windows.result.restored_all", n as u64, &[]),
            },
            vec![],
        ),
        ActionResult::RestoreAll(a) => msg(
            BadgeKind::Warn,
            i18n::tf(
                "windows.result.restored_partial",
                &[("restored", &a.restored), ("failed", &a.failed.len())],
            ),
            a.failed
                .iter()
                .map(|f| format!("{:#x}: {}", f.hwnd, f.error))
                .collect(),
        ),
    }
}

/// hwnd that reported `fights_back` in a place or apply result.
pub fn fights_back_hwnd(r: &ActionResult) -> Option<isize> {
    match r {
        ActionResult::Apply(a) | ActionResult::Place(a)
            if a.warnings.iter().any(|w| w.code == "fights_back") =>
        {
            Some(a.hwnd)
        }
        _ => None,
    }
}
type Loaded = (Vec<WindowInfo>, Vec<String>);

fn load(c: &mut Client, include_hidden: bool) -> Result<Reply<Loaded>, CallError> {
    let params = json!({ "include_hidden": include_hidden });
    let windows = match get::<ListWindowsResult>(c, "list_windows", params)? {
        Reply::Ok(w) => w.windows,
        Reply::Remote(b) => return Ok(Reply::Remote(b)),
    };
    Ok(lift(
        get::<ListIslandsResult>(c, "list_islands", json!({}))?,
        |i| {
            let ids = i
                .layouts
                .iter()
                .flat_map(|l| l.islands.iter().map(|x| format!("{}/{}", l.id, x.id)))
                .collect();
            (windows, ids)
        },
    ))
}

fn act(c: &mut Client, a: Action) -> Result<Reply<ActionResult>, CallError> {
    Ok(match a {
        Action::Apply { hwnd } => lift(
            get::<ApplyResult>(c, "apply", json!({ "window": { "hwnd": hwnd } }))?,
            ActionResult::Apply,
        ),
        Action::Place { hwnd, island } => lift(
            get::<ApplyResult>(
                c,
                "place",
                json!({ "window": { "hwnd": hwnd }, "island": island }),
            )?,
            ActionResult::Place,
        ),
        Action::Restore { hwnd } => lift(
            get::<RestoreResult>(c, "restore", json!({ "window": { "hwnd": hwnd } }))?,
            ActionResult::Restore,
        ),
        Action::RestoreAll => lift(
            get::<RestoreAllResult>(c, "restore_all", json!({}))?,
            ActionResult::RestoreAll,
        ),
    })
}

// ------------------------------------------------------------------------ view

enum Action {
    Apply { hwnd: isize },
    Place { hwnd: isize, island: String },
    Restore { hwnd: isize },
    RestoreAll,
}

/// Emitted by a row's "Diagnose…"; the shell opens Diagnostics with that window selected.
pub struct DiagnoseWindow {
    pub hwnd: isize,
}

pub struct WindowsView {
    wins: Vec<WindowInfo>,
    /// `layout/island` ids for the place menu.
    islands: Vec<String>,
    loaded: bool,
    /// Last poll failure; data stays (dimmed, actions disabled).
    error: Option<Failure>,
    show_all: bool,
    only_managed: bool,
    search: Entity<InputState>,
    selected: Option<isize>,
    open_menu: Option<isize>,
    confirm_all: bool,
    busy: bool,
    polling: bool,
    fights_back: HashSet<isize>,
    message: Option<Msg>,
    message_gen: u64,
    last_render: Instant,
    /// Language the search placeholder was last set for.
    locale: Locale,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DiagnoseWindow> for WindowsView {}

impl WindowsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n::t("windows.search_placeholder").into_owned())
        });
        let subs = vec![cx.subscribe(&search, |_, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                cx.notify();
            }
        })];
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this
                    .update(cx, |t, cx| {
                        if t.last_render.elapsed() < VISIBLE_GRACE {
                            t.poll(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut v = Self {
            wins: Vec::new(),
            islands: Vec::new(),
            loaded: false,
            error: None,
            show_all: false,
            only_managed: false,
            search,
            selected: None,
            open_menu: None,
            confirm_all: false,
            busy: false,
            polling: false,
            fights_back: HashSet::new(),
            message: None,
            message_gen: 0,
            last_render: Instant::now(),
            locale: i18n::locale(),
            _subs: subs,
        };
        v.poll(cx);
        v
    }

    /// Immediate refresh (after an action, or when the connection comes back).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.poll(cx);
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        let show_all = self.show_all;
        cx.spawn(async move |this, cx| {
            let r = call(cx, move |c| load(c, show_all)).await;
            this.update(cx, |t, cx| {
                t.polling = false;
                match r {
                    Ok((mut wins, islands)) => {
                        sort_windows(&mut wins);
                        let d = diff_windows(&t.wins, &wins);
                        for h in &d.removed {
                            t.fights_back.remove(h);
                        }
                        if t.selected.is_some_and(|s| d.removed.contains(&s)) {
                            t.selected = None;
                        }
                        if t.open_menu.is_some_and(|s| d.removed.contains(&s)) {
                            t.open_menu = None;
                        }
                        if !d.is_empty() {
                            t.wins = wins;
                        }
                        t.islands = islands;
                        t.error = None;
                        t.loaded = true;
                    }
                    Err(f) => {
                        t.error = Some(f);
                        t.message = super::without_stale_error(t.message.take());
                    }
                }
                // Always notify: the repaint is the visibility heartbeat for the next tick.
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-set the search placeholder after a language switch.
    fn sync_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let l = i18n::locale();
        if l != self.locale {
            self.locale = l;
            let text = i18n::t("windows.search_placeholder").into_owned();
            self.search
                .update(cx, |s, cx| s.set_placeholder(text, window, cx));
        }
    }

    fn set_message(&mut self, m: Msg, cx: &mut Context<Self>) {
        self.message = Some(m);
        self.message_gen += 1;
        let gen_ = self.message_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MESSAGE_TTL).await;
            this.update(cx, |t, cx| {
                if t.message_gen == gen_ {
                    t.message = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn run(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.open_menu = None;
        self.confirm_all = false;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let r = call(cx, move |c| act(c, action)).await;
            this.update(cx, |t, cx| {
                t.busy = false;
                let m = match r {
                    Ok(res) => {
                        if let Some(h) = fights_back_hwnd(&res) {
                            t.fights_back.insert(h);
                        }
                        describe(&res)
                    }
                    Err(f) => failure_msg(&f),
                };
                t.set_message(m, cx);
                t.poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let stale = self.error.is_some();
        let mut all = secondary_button(
            "show-all",
            i18n::t("windows.show_all").into_owned(),
            cx.listener(|t, _, _, cx| {
                t.show_all = !t.show_all;
                t.poll(cx);
                cx.notify();
            }),
        );
        if self.show_all {
            all = all.icon(IconName::Check);
        }
        let restore_all = if self.confirm_all {
            div()
                .flex()
                .items_center()
                .gap(SP_2)
                .child(div().child(i18n::t("windows.restore_all_confirm").into_owned()))
                .child(
                    secondary_button(
                        "restore-all-yes",
                        i18n::t("windows.restore_all").into_owned(),
                        cx.listener(|t, _, _, cx| t.run(Action::RestoreAll, cx)),
                    )
                    .disabled(self.busy || stale),
                )
                .child(secondary_button(
                    "restore-all-no",
                    i18n::t("windows.cancel").into_owned(),
                    cx.listener(|t, _, _, cx| {
                        t.confirm_all = false;
                        cx.notify();
                    }),
                ))
        } else {
            div().child(
                secondary_button(
                    "restore-all",
                    i18n::t("windows.restore_all").into_owned(),
                    cx.listener(|t, _, _, cx| {
                        t.confirm_all = true;
                        cx.notify();
                    }),
                )
                .icon(IconName::Undo2)
                .disabled(self.busy || stale),
            )
        };
        let mut managed = secondary_button(
            "only-managed",
            i18n::t("windows.only_managed").into_owned(),
            cx.listener(|t, _, _, cx| {
                t.only_managed = !t.only_managed;
                cx.notify();
            }),
        );
        if self.only_managed {
            managed = managed.icon(IconName::Check);
        }
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(SP_2)
            .child(div().w(SEARCH_W).child(Input::new(&self.search)))
            .child(all)
            .child(managed)
            .child(restore_all)
    }

    fn row(&mut self, r: RowModel, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hwnd = r.hwnd;
        let off = self.busy || self.error.is_some();
        let selected = self.selected == Some(hwnd);
        let menu_open = self.open_menu == Some(hwnd);

        let mut badges = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(SP_1)
            .child(badge(
                r.state,
                if r.managed {
                    BadgeKind::Ok
                } else {
                    BadgeKind::Neutral
                },
            ));
        if let Some(i) = &r.island {
            badges = badges.child(badge(
                i18n::tf("windows.row.island", &[("island", i)]),
                BadgeKind::Info,
            ));
        }
        if r.fights_back {
            badges = badges.child(badge(
                i18n::t("windows.row.fights_back").into_owned(),
                BadgeKind::Warn,
            ));
        }
        if r.maximized {
            badges = badges.child(badge(
                i18n::t("windows.row.maximized").into_owned(),
                BadgeKind::Neutral,
            ));
        }
        if r.minimized {
            badges = badges.child(badge(
                i18n::t("windows.row.minimized").into_owned(),
                BadgeKind::Neutral,
            ));
        }
        if r.elevated {
            badges = badges.child(badge(
                i18n::t("windows.row.elevated").into_owned(),
                BadgeKind::Warn,
            ));
        }
        badges = badges.child(badge(
            format!("{} · {}", r.dpi, r.awareness),
            BadgeKind::Neutral,
        ));

        let mut actions = div().flex().flex_wrap().items_center().gap(SP_2).child(
            secondary_button(
                format!("apply-{hwnd}"),
                i18n::t("windows.row.apply").into_owned(),
                cx.listener(move |t, _, _, cx| t.run(Action::Apply { hwnd }, cx)),
            )
            .disabled(off || !r.can_apply),
        );
        actions = actions.child(
            secondary_button(
                format!("place-{hwnd}"),
                i18n::t("windows.row.place").into_owned(),
                cx.listener(move |t, _, _, cx| {
                    t.open_menu = if t.open_menu == Some(hwnd) {
                        None
                    } else {
                        Some(hwnd)
                    };
                    cx.notify();
                }),
            )
            .disabled(off || !r.can_place),
        );
        actions = actions.child(
            secondary_button(
                format!("diagnose-{hwnd}"),
                i18n::t("windows.row.diagnose").into_owned(),
                cx.listener(move |_, _, _, cx| cx.emit(DiagnoseWindow { hwnd })),
            )
            .disabled(off || !r.can_diagnose),
        );
        if r.managed {
            actions = actions.child(
                secondary_button(
                    format!("restore-{hwnd}"),
                    i18n::t("windows.row.restore").into_owned(),
                    cx.listener(move |t, _, _, cx| t.run(Action::Restore { hwnd }, cx)),
                )
                .disabled(off || !r.can_restore),
            );
        }

        let menu = menu_open.then(|| {
            let mut m = div().flex().flex_wrap().gap(SP_2).pt(SP_2);
            if self.islands.is_empty() {
                m = m.child(
                    div()
                        .text_color(text_muted())
                        .child(i18n::t("windows.row.no_islands").into_owned()),
                );
            }
            for (i, id) in self.islands.iter().enumerate() {
                let island = id.clone();
                m = m.child(secondary_button(
                    format!("place-{hwnd}-{i}"),
                    id.clone(),
                    cx.listener(move |t, _, _, cx| {
                        t.run(
                            Action::Place {
                                hwnd,
                                island: island.clone(),
                            },
                            cx,
                        )
                    }),
                ));
            }
            m
        });

        div()
            .id(format!("row-{hwnd}"))
            .min_h(HIT)
            .p(SP_2)
            .flex()
            .flex_col()
            .rounded(R_SM)
            .when(selected, |d| d.bg(raised()))
            .hover(|s| s.bg(text().opacity(0.04)))
            .on_click(cx.listener(move |t, _, _, cx| {
                t.selected = Some(hwnd);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_3)
                    .child(
                        div()
                            .flex_1()
                            .min_w(INFO_MIN_W)
                            .flex()
                            .flex_col()
                            .gap(SP_1)
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_x(SP_2)
                                    .child(div().font_weight(FontWeight::MEDIUM).child(r.process))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_color(text_muted())
                                            .font_features(tabular())
                                            .child(r.title),
                                    ),
                            )
                            .child(badges),
                    )
                    .child(actions),
            )
            .when(r.elevated, |d| {
                d.child(
                    div()
                        .pt(SP_1)
                        .text_size(TEXT_SM)
                        .text_color(text_muted())
                        .child(i18n::t("windows.row.elevated_note").into_owned()),
                )
            })
            .children(menu)
    }

    fn message_view(&self) -> Option<impl IntoElement> {
        self.message.as_ref().map(notice_view)
    }

    /// Sentence + small stable code; "Retry" asks for an immediate poll.
    fn failure_card(&self, f: &Failure, cx: &mut Context<Self>) -> Div {
        let (sentence, code) = failure_text(f);
        div()
            .flex()
            .flex_col()
            .gap(SP_2)
            .p(SP_3)
            .rounded(R_SM)
            .bg(raised())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_2)
                    .child(badge(
                        i18n::t("gui.notice.error").into_owned(),
                        BadgeKind::Error,
                    ))
                    .child(div().min_w_0().text_color(danger()).child(sentence)),
            )
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(i18n::tf("gui.error.code", &[("code", &code)])),
            )
            .child(div().flex().child(secondary_button(
                "windows-retry",
                i18n::t("windows.retry").into_owned(),
                cx.listener(|t, _, _, cx| t.poll(cx)),
            )))
    }
}

impl Render for WindowsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_locale(window, cx);
        self.last_render = Instant::now();
        let query = self.search.read(cx).value().to_string();
        let models: Vec<RowModel> =
            visible_windows(&self.wins, self.show_all, &query, self.only_managed)
                .into_iter()
                .map(|w| row_model(w, self.fights_back.contains(&w.hwnd)))
                .collect();
        let count = models.len();

        let body = if !self.loaded {
            match &self.error {
                Some(f) => self.failure_card(f, cx).into_any_element(),
                None => div()
                    .text_color(text_muted())
                    .child(i18n::t("windows.loading").into_owned())
                    .into_any_element(),
            }
        } else if models.is_empty() {
            empty_state(
                empty_title(&query, self.only_managed),
                empty_hint(&query, self.only_managed),
            )
            .into_any_element()
        } else {
            let stale = self.error.is_some();
            let mut rows = Vec::with_capacity(models.len());
            for m in models {
                rows.push(self.row(m, cx));
            }
            div()
                .id("windows-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    card()
                        .gap(SP_1)
                        .when(stale, |d| d.opacity(0.5))
                        .children(rows),
                )
                .into_any_element()
        };

        let banner = self
            .error
            .as_ref()
            .filter(|_| self.loaded)
            .map(|f| self.failure_card(f, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(section_header(
                i18n::t("gui.section.windows").into_owned(),
                i18n::t("windows.subtitle").into_owned(),
            ))
            .child(self.toolbar(cx))
            .children(banner)
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .font_features(tabular())
                    .child(i18n::tn("windows.count", count as u64, &[])),
            )
            .child(body)
            // Footer: a notice never pushes the rows (click targets) down.
            .children(self.message_view().map(|m| div().flex_shrink_0().child(m)))
            .pb(SP_4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{ClientSize, RestoreFailure, WindowMonitor};
    use crate::window_state::Rect;

    fn win(hwnd: isize, process: &str, title: &str) -> WindowInfo {
        WindowInfo {
            hwnd,
            pid: 1,
            process: process.into(),
            title: title.into(),
            class: "C".into(),
            visible: true,
            style: "0x0".into(),
            exstyle: "0x0".into(),
            rect: Rect::default(),
            client: ClientSize { w: 1, h: 1 },
            monitor: WindowMonitor {
                index: 0,
                device: "D".into(),
            },
            dpi: 120,
            dpi_awareness: DpiAwareness::PerMonitorV2,
            elevated: Some(false),
            maximized: false,
            minimized: false,
            managed: false,
            island: None,
        }
    }

    fn warn(code: &str, data: serde_json::Value) -> Warning {
        Warning {
            code: code.into(),
            data,
        }
    }

    fn applied(warnings: Vec<Warning>) -> ApplyResult {
        ApplyResult {
            applied: true,
            reason: None,
            hwnd: 7,
            target: Rect::default(),
            r#final: Rect::default(),
            settled: true,
            warnings,
        }
    }

    #[test]
    fn default_filter_hides_hidden_untitled_and_excluded() {
        let mut hidden = win(1, "a.exe", "A");
        hidden.visible = false;
        let all = vec![
            hidden,
            win(2, "b.exe", "   "),
            win(3, "explorer.exe", "Escritorio"),
            win(4, "c.exe", "C"),
        ];
        let got: Vec<isize> = visible_windows(&all, false, "", false)
            .iter()
            .map(|w| w.hwnd)
            .collect();
        assert!(got.contains(&4));
        assert!(!got.contains(&1) && !got.contains(&2));
        assert_eq!(visible_windows(&all, true, "", false).len(), 4);
    }

    #[test]
    fn query_matches_process_or_title_case_insensitively() {
        let all = vec![
            win(1, "Notepad.exe", "Untitled"),
            win(2, "game.exe", "NOTES of the game"),
            win(3, "other.exe", "Other"),
        ];
        let ids = |q| -> Vec<isize> {
            visible_windows(&all, false, q, false)
                .iter()
                .map(|w| w.hwnd)
                .collect()
        };
        assert_eq!(ids("note"), [1, 2]);
        assert_eq!(ids("  OTHER "), [3]);
        assert_eq!(ids(""), [1, 2, 3]);
        assert!(ids("zzz").is_empty());
    }

    #[test]
    fn only_managed_keeps_managed_and_combines_with_query() {
        let mut a = win(1, "a.exe", "A");
        a.managed = true;
        let mut b = win(2, "b.exe", "B");
        b.managed = true;
        let all = vec![a, b, win(3, "a2.exe", "A2")];
        let ids = |q, m| -> Vec<isize> {
            visible_windows(&all, false, q, m)
                .iter()
                .map(|w| w.hwnd)
                .collect()
        };
        assert_eq!(ids("", true), [1, 2]);
        assert_eq!(ids("a", true), [1]);
        assert_eq!(ids("a", false), [1, 3]);
    }

    #[test]
    fn empty_texts_depend_on_the_active_filter() {
        let _g = i18n::scoped_locale(Locale::En);
        let none = "No managed windows yet";
        assert_eq!(empty_title("", true), none);
        assert_eq!(empty_title("   ", true), none);
        let no_match = "No windows match the filter";
        assert_eq!(empty_title("x", true), no_match);
        assert_eq!(empty_title("x", false), no_match);
        assert_eq!(empty_title("", false), no_match);
    }

    #[test]
    fn apply_results_read_as_info_not_errors() {
        let _g = i18n::scoped_locale(Locale::En);
        let mut a = applied(vec![]);
        let ok = describe(&ActionResult::Apply(a.clone()));
        assert!(ok.kind == BadgeKind::Ok && ok.text == "Profile applied");
        a.applied = false;
        a.reason = Some("already_placed".into());
        let m = describe(&ActionResult::Apply(a.clone()));
        assert!(m.kind == BadgeKind::Info && m.text == "Already in place");
        a.reason = None;
        assert!(describe(&ActionResult::Apply(a)).kind == BadgeKind::Info);
        let w = ActionResult::Apply(applied(vec![warn(
            "fights_back",
            json!({"forced": {"w": 1, "h": 2}}),
        )]));
        assert!(describe(&w).kind == BadgeKind::Warn);
        assert_eq!(fights_back_hwnd(&w), Some(7));
    }

    #[test]
    fn elevated_rows_have_every_action_disabled() {
        let mut w = win(1, "g.exe", "J");
        assert!(row_model(&w, false).can_apply);
        w.managed = true;
        w.elevated = Some(true);
        let r = row_model(&w, false);
        assert!(!r.can_apply && !r.can_place && !r.can_restore && !r.can_diagnose);
    }

    #[test]
    fn sorting_is_stable_and_case_insensitive() {
        let mut v = vec![
            win(3, "b.exe", "x"),
            win(2, "A.exe", "z"),
            win(1, "a.exe", "z"),
        ];
        sort_windows(&mut v);
        assert_eq!(v.iter().map(|w| w.hwnd).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn diff_reports_added_removed_changed() {
        let old = vec![win(1, "a", "t"), win(2, "b", "t")];
        let mut changed = win(2, "b", "t");
        changed.managed = true;
        let new = vec![changed, win(3, "c", "t")];
        let d = diff_windows(&old, &new);
        assert_eq!(d.added, [3]);
        assert_eq!(d.removed, [1]);
        assert_eq!(d.changed, [2]);
        assert!(diff_windows(&old, &old).is_empty());
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        let five = "\u{e9}\u{ed}\u{f3}\u{fa}\u{e1}";
        assert_eq!(truncate_chars(five, 5), five);
        assert_eq!(
            truncate_chars(&format!("{five}x"), 5),
            "\u{e9}\u{ed}\u{f3}\u{fa}…"
        );
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn dpi_is_a_percentage() {
        let _g = i18n::scoped_locale(Locale::En);
        assert_eq!(dpi_label(96), "100%");
        assert_eq!(dpi_label(120), "125%");
        assert_eq!(dpi_label(144), "150%");
        i18n::set_locale(Locale::Es);
        assert_eq!(dpi_label(120), "125 %");
    }

    #[test]
    fn row_model_gates_actions() {
        let _g = i18n::scoped_locale(Locale::En);
        let mut w = win(1, "g.exe", "Game");
        let r = row_model(&w, false);
        assert!(r.can_place && !r.can_restore && r.state == "Not managed");
        w.managed = true;
        w.island = Some("halves-16-9/right".into());
        let r = row_model(&w, true);
        assert!(r.can_restore && r.fights_back && r.state == "Managed");
        assert_eq!(r.island.as_deref(), Some("halves-16-9/right"));
        w.elevated = Some(true);
        let r = row_model(&w, false);
        assert!(r.elevated && !r.can_place && !r.can_restore);
        w.elevated = None; // unknown is not blocked
        assert!(row_model(&w, false).can_place);
    }

    #[test]
    fn warnings_in_plain_english() {
        let _g = i18n::scoped_locale(Locale::En);
        assert_eq!(
            warning_text(&warn(
                "size_forced",
                json!({"w": 1280, "h": 720, "after_ms": 5})
            )),
            "The window forces its size to 1280x720"
        );
        assert_eq!(
            warning_text(&warn("position_forced", json!({"x": 0, "y": 8}))),
            "The window forces its position to 0,8"
        );
        assert_eq!(
            warning_text(&warn(
                "fights_back",
                json!({"forced": {"w": 1280, "h": 720}})
            )),
            "The window fights back: it insists on 1280x720"
        );
        assert!(warning_text(&warn("size_forced", json!({}))).ends_with("its size"));
        assert_eq!(
            warning_text(&warn("unknown_code", json!(null))),
            "unknown_code"
        );
    }

    #[test]
    fn describe_place_results() {
        let _g = i18n::scoped_locale(Locale::En);
        let mut a = applied(vec![]);
        assert!(describe(&ActionResult::Place(a.clone())).kind == BadgeKind::Ok);
        a.applied = false;
        a.reason = Some("already_placed".into());
        assert_eq!(
            describe(&ActionResult::Place(a.clone())).text,
            "Already in place"
        );
        a.reason = Some("excluded".into());
        assert_eq!(describe(&ActionResult::Place(a)).text, "Excluded process");

        let w = applied(vec![
            warn("size_forced", json!({"w": 1, "h": 2})),
            warn("fights_back", json!({"forced": {"w": 1, "h": 2}})),
        ]);
        let r = ActionResult::Place(w);
        let m = describe(&r);
        assert!(m.kind == BadgeKind::Warn);
        assert_eq!(m.detail.len(), 2);
        assert_eq!(fights_back_hwnd(&r), Some(7));
        assert_eq!(
            fights_back_hwnd(&ActionResult::Place(applied(vec![]))),
            None
        );
    }

    #[test]
    fn describe_restore_results() {
        let _g = i18n::scoped_locale(Locale::En);
        let r = |restored, exact| {
            ActionResult::Restore(RestoreResult {
                restored,
                exact,
                hwnd: 1,
            })
        };
        assert!(describe(&r(true, true)).kind == BadgeKind::Ok);
        assert_eq!(
            describe(&r(true, false)).text,
            "Restored, but not identical to the original state"
        );
        let all =
            |restored, failed| ActionResult::RestoreAll(RestoreAllResult { restored, failed });
        assert_eq!(describe(&all(1, vec![])).text, "1 window restored");
        assert_eq!(describe(&all(3, vec![])).text, "3 windows restored");
        assert_eq!(
            describe(&all(0, vec![])).text,
            "There were no managed windows"
        );
        let m = describe(&all(
            2,
            vec![RestoreFailure {
                hwnd: 255,
                error: "The window no longer exists".into(),
            }],
        ));
        assert!(m.kind == BadgeKind::Warn);
        assert_eq!(m.detail, ["0xff: The window no longer exists"]);
    }
}
