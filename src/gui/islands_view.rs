//! "Islands": how each monitor is split and which window sits where.
//!
//! Pure helpers (occupants, who can be placed, error texts, layout/monitor grouping) are at the
//! top with tests; the GPUI view below only draws them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, px,
};
use serde_json::json;

use super::Failure;
use super::monitor_preview::{
    MonitorPreview, PreviewIsland, PreviewMonitor, desktop_layout, format_size, fraction_label,
    occupant_label, preview_max_width, window_matches,
};
use super::theme::{
    BadgeKind, HIT, R_SM, SP_1, SP_2, SP_3, SP_4, TEXT_SM, accent, badge, card, danger,
    empty_state, primary_button, raised, secondary_button, section_header, tabular, text,
    text_muted,
};
use super::windows_view::{ActionResult, Msg, describe, sort_windows, truncate_chars};
use super::{Reply, call, failure_msg, failure_text, get, layout_label, lift, notice_view};
use crate::i18n::{self, Locale};
use crate::ipc::pipe::Client;
use crate::ipc::{
    ApplyResult, CallError, GetProfilesResult, IslandInfo, LayoutInfo, ListIslandsResult,
    ListWindowsResult, RestoreResult, WindowInfo,
};
use crate::islands::{Layout, MonitorInfo, MonitorSel, select_monitor};
use crate::window_state::Rect;

const POLL: Duration = Duration::from_secs(2);
/// A view that painted within this long is on screen (there is no visibility hook).
const VISIBLE_GRACE: Duration = Duration::from_secs(4);
const NOTICE_TTL: Duration = Duration::from_secs(8);
const PREVIEW_MAX_H: f32 = 420.0;
const PICKER_MAX_H: f32 = 220.0;
const TITLE_MAX: usize = 70;

// ------------------------------------------------------------------ pure logic

/// A window sitting on an island, resolved against `list_windows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occupant {
    pub hwnd: isize,
    pub label: String,
    pub process: Option<String>,
}

pub fn occupants_of(island: &IslandInfo, windows: &[WindowInfo]) -> Vec<Occupant> {
    island
        .occupants
        .iter()
        .map(|&hwnd| {
            let w = windows.iter().find(|w| w.hwnd == hwnd);
            Occupant {
                hwnd,
                label: occupant_label(
                    hwnd,
                    w.map(|w| w.process.as_str()),
                    w.map(|w| w.title.as_str()),
                ),
                process: w.map(|w| w.process.clone()),
            }
        })
        .collect()
}

/// May a window be placed? Needs an available monitor and a non-elevated window.
pub fn placeable(monitor_ok: bool, window: Option<&WindowInfo>) -> bool {
    monitor_ok && window.is_some_and(|w| w.elevated != Some(true))
}

/// Windows the picker offers: titled ones that match `query`, sorted like the Ventanas list.
pub fn picker_windows(all: &[WindowInfo], query: &str) -> Vec<WindowInfo> {
    let mut v: Vec<WindowInfo> = all
        .iter()
        .filter(|w| !w.title.trim().is_empty() && window_matches(query, &w.process, &w.title))
        .cloned()
        .collect();
    sort_windows(&mut v);
    v
}

/// `MonitorInfo::index` of the monitor a layout targets, `None` when it is not connected.
pub fn layout_monitor(sel: &MonitorSel, monitors: &[MonitorInfo]) -> Option<usize> {
    select_monitor(sel, monitors).ok().map(|m| m.index)
}

/// Positions (into `layouts`) of the layouts targeting monitor `mi`.
pub fn layouts_on(layouts: &[LayoutInfo], monitors: &[MonitorInfo], mi: usize) -> Vec<usize> {
    (0..layouts.len())
        .filter(|&i| layout_monitor(&layouts[i].monitor, monitors) == Some(mi))
        .collect()
}

/// Saved designs the engine left out because their monitor is not connected.
pub fn missing_layouts(
    saved: &[Layout],
    listed: &[LayoutInfo],
    monitors: &[MonitorInfo],
) -> Vec<Layout> {
    saved
        .iter()
        .filter(|l| {
            layout_monitor(&l.monitor, monitors).is_none() && !listed.iter().any(|i| i.id == l.id)
        })
        .cloned()
        .collect()
}

/// The layout shown on monitor `mi`: the chosen one if it still exists there, else the first.
pub fn active_layout(
    layouts: &[LayoutInfo],
    monitors: &[MonitorInfo],
    chosen: Option<&str>,
    mi: usize,
) -> Option<usize> {
    let on = layouts_on(layouts, monitors, mi);
    on.iter()
        .copied()
        .find(|&i| Some(layouts[i].id.as_str()) == chosen)
        .or_else(|| on.first().copied())
}

/// Does `layout/island` still exist?
pub fn selection_exists(layouts: &[LayoutInfo], layout: &str, island: &str) -> bool {
    layouts
        .iter()
        .any(|l| l.id == layout && l.islands.iter().any(|i| i.id == island))
}

/// What a layout's monitor selector means, for the "Monitor not available" card.
pub fn monitor_sel_text(sel: &MonitorSel) -> String {
    match sel {
        MonitorSel::Index(i) => i18n::tf("islands.monitor_sel.index", &[("i", i)]),
        MonitorSel::Device(d) => i18n::tf("islands.monitor_sel.device", &[("device", d)]),
        MonitorSel::Primary => i18n::t("islands.monitor_sel.primary").into_owned(),
    }
}

/// "Monitor 1 · 5120×1440 px · primary".
pub fn monitor_legend(m: &MonitorInfo) -> String {
    let n = m.index + 1;
    let size = format_size(m.rect.width(), m.rect.height());
    if m.primary {
        i18n::tf(
            "islands.monitor_legend_primary",
            &[("n", &n), ("size", &size)],
        )
    } else {
        i18n::tf("islands.monitor_legend", &[("n", &n), ("size", &size)])
    }
}

// ------------------------------------------------------------------ IPC

struct Data {
    monitors: Vec<MonitorInfo>,
    layouts: Vec<LayoutInfo>,
    /// Saved designs whose monitor is not connected (the engine does not list them).
    missing: Vec<Layout>,
    windows: Vec<WindowInfo>,
}

enum Action {
    Place { hwnd: isize, island: String },
    Restore { hwnd: isize },
}

fn load_all(c: &mut Client) -> Result<Reply<Data>, CallError> {
    let windows = match get::<ListWindowsResult>(c, "list_windows", json!({}))? {
        Reply::Ok(w) => w.windows,
        Reply::Remote(b) => return Ok(Reply::Remote(b)),
    };
    let i = match get::<ListIslandsResult>(c, "list_islands", json!({}))? {
        Reply::Ok(i) => i,
        Reply::Remote(b) => return Ok(Reply::Remote(b)),
    };
    // `list_islands` skips layouts whose monitor is gone, so the profile file is the only place
    // that still names them. An unreadable file just means there is nothing extra to show.
    let missing = match get::<GetProfilesResult>(c, "get_profiles", json!({}))? {
        Reply::Ok(p) => missing_layouts(&p.layouts, &i.layouts, &i.monitors),
        Reply::Remote(_) => Vec::new(),
    };
    Ok(Reply::Ok(Data {
        monitors: i.monitors,
        layouts: i.layouts,
        missing,
        windows,
    }))
}

fn act(c: &mut Client, a: Action) -> Result<Reply<ActionResult>, CallError> {
    Ok(match a {
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
    })
}

// ------------------------------------------------------------------ view

/// Emitted by "Create layout" (`None`) and "Edit layout" (`Some(layout id)`); the shell opens the
/// editor with it.
pub struct OpenEditor {
    pub layout: Option<String>,
}

pub struct IslandsView {
    data: Option<Data>,
    /// Last failure; old data stays next to it (dimmed, actions disabled).
    error: Option<Failure>,
    loading: bool,
    /// A refresh was asked while one was in flight: run it when that one lands.
    reload: bool,
    busy: bool,
    selected: Option<(String, String)>,
    /// Layout shown per monitor index when several target the same monitor.
    chosen: HashMap<usize, String>,
    picker_open: bool,
    picker_sel: Option<isize>,
    search: Entity<InputState>,
    notice: Option<Msg>,
    notice_gen: u64,
    last_render: Instant,
    /// Language the search placeholder was last set for.
    locale: Locale,
    active: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenEditor> for IslandsView {}

impl IslandsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n::t("islands.search_placeholder").into_owned())
        });
        let subs = vec![cx.subscribe(&search, |_, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                cx.notify();
            }
        })];
        // Poll only while the view is on screen and the panel is the active window.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |t, cx| t.tick(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut v = Self {
            data: None,
            error: None,
            loading: false,
            reload: false,
            busy: false,
            selected: None,
            chosen: HashMap::new(),
            picker_open: false,
            picker_sel: None,
            search,
            notice: None,
            notice_gen: 0,
            last_render: Instant::now(),
            locale: i18n::locale(),
            active: true,
            _subs: subs,
        };
        v.refresh(cx);
        v
    }

    /// Re-set the search placeholder after a language switch.
    fn sync_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let l = i18n::locale();
        if l != self.locale {
            self.locale = l;
            let text = i18n::t("islands.search_placeholder").into_owned();
            self.search
                .update(cx, |s, cx| s.set_placeholder(text, window, cx));
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.last_render.elapsed() >= VISIBLE_GRACE {
            return;
        }
        if !self.active {
            // Heartbeat: repainting keeps `last_render` fresh while the panel is just inactive.
            cx.notify();
            return;
        }
        if !self.busy && !self.loading {
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.reload = true;
            return;
        }
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let r = call(cx, load_all).await;
            this.update(cx, |t, cx| t.apply(r, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn apply(&mut self, r: Result<Data, Failure>, cx: &mut Context<Self>) {
        self.loading = false;
        match r {
            Ok(d) => {
                self.error = None;
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|(l, i)| !selection_exists(&d.layouts, l, i))
                {
                    self.selected = None;
                    self.picker_open = false;
                }
                if self
                    .picker_sel
                    .is_some_and(|h| !d.windows.iter().any(|w| w.hwnd == h))
                {
                    self.picker_sel = None;
                }
                self.data = Some(d);
            }
            Err(f) => {
                self.error = Some(f);
                self.notice = super::without_stale_error(self.notice.take());
            }
        }
        if std::mem::take(&mut self.reload) {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn run(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let r = call(cx, move |c| act(c, action)).await;
            this.update(cx, |t, cx| t.finish(r, cx)).ok();
        })
        .detach();
    }

    fn finish(&mut self, r: Result<ActionResult, Failure>, cx: &mut Context<Self>) {
        self.busy = false;
        let msg = match r {
            Ok(res) => {
                if matches!(res, ActionResult::Place(_)) {
                    self.picker_open = false;
                    self.picker_sel = None;
                }
                describe(&res)
            }
            Err(f) => failure_msg(&f),
        };
        self.set_notice(msg, cx);
        self.refresh(cx);
    }

    fn set_notice(&mut self, m: Msg, cx: &mut Context<Self>) {
        self.notice = Some(m);
        self.notice_gen += 1;
        let gen_ = self.notice_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TTL).await;
            this.update(cx, |t, cx| {
                if t.notice_gen == gen_ {
                    t.notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn select(&mut self, key: (String, String), cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&key) {
            self.picker_open = false;
            self.picker_sel = None;
        }
        self.selected = Some(key);
        cx.notify();
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = true;
        self.picker_sel = None;
        self.search.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    // -------------------------------------------------------------- pieces

    fn notice_view(&self) -> Option<impl IntoElement + use<>> {
        self.notice.as_ref().map(notice_view)
    }

    fn error_card(&self, f: &Failure, cx: &mut Context<Self>) -> AnyElement {
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
                    .items_center()
                    .gap(SP_2)
                    .child(badge(
                        i18n::t("gui.notice.error").into_owned(),
                        BadgeKind::Error,
                    ))
                    .child(div().text_color(danger()).child(sentence)),
            )
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(i18n::tf("gui.error.code", &[("code", &code)])),
            )
            .child(div().flex().child(secondary_button(
                "islands-retry",
                i18n::t("islands.retry").into_owned(),
                cx.listener(|t, _, _, cx| t.refresh(cx)),
            )))
            .into_any_element()
    }

    fn preview_card(&self, d: &Data, viewport_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let rects: Vec<Rect> = d.monitors.iter().map(|m| m.rect).collect();
        let layout = desktop_layout(&rects, preview_max_width(viewport_w), PREVIEW_MAX_H);
        let monitors: Vec<PreviewMonitor> = d
            .monitors
            .iter()
            .map(|m| PreviewMonitor {
                rect: m.rect,
                // The legend below names the monitors: an island in the corner would cover a label.
                label: SharedString::default(),
            })
            .collect();

        let mut keys: Vec<(String, String)> = Vec::new();
        let mut islands: Vec<PreviewIsland> = Vec::new();
        let mut chips: Vec<AnyElement> = Vec::new();
        for m in &d.monitors {
            let mi = m.index;
            let Some(ai) = active_layout(
                &d.layouts,
                &d.monitors,
                self.chosen.get(&mi).map(String::as_str),
                mi,
            ) else {
                continue;
            };
            let l = &d.layouts[ai];
            for (n, i) in l.islands.iter().enumerate() {
                islands.push(PreviewIsland {
                    rect: i.rect,
                    name: i.id.clone().into(),
                    occupants: occupants_of(i, &d.windows)
                        .into_iter()
                        .map(|o| SharedString::from(o.process.unwrap_or(o.label)))
                        .collect(),
                    color_idx: n,
                });
                keys.push((l.id.clone(), i.id.clone()));
            }
            let on = layouts_on(&d.layouts, &d.monitors, mi);
            if on.len() > 1 {
                let row = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_2)
                    .child(
                        div()
                            .text_color(text_muted())
                            .child(i18n::tf("islands.monitor_layout", &[("n", &(mi + 1))])),
                    )
                    .children(on.iter().map(|&li| {
                        let id = d.layouts[li].id.clone();
                        let pick = id.clone();
                        let mut b = secondary_button(
                            format!("chip-{mi}-{id}"),
                            layout_label(&id),
                            cx.listener(move |t, _, _, cx| {
                                t.chosen.insert(mi, pick.clone());
                                t.selected = None;
                                t.picker_open = false;
                                cx.notify();
                            }),
                        );
                        if li == ai {
                            b = b.icon(IconName::Check);
                        }
                        b
                    }));
                chips.push(row.into_any_element());
            }
        }

        let selected = self
            .selected
            .as_ref()
            .and_then(|s| keys.iter().position(|k| k == s));
        let view = cx.entity();
        let click_keys = keys.clone();
        let preview = MonitorPreview::new(layout, monitors, islands)
            .selected(selected)
            .on_click(move |i, _, cx| {
                if let Some(k) = click_keys.get(i).cloned() {
                    view.update(cx, |t, cx| t.select(k, cx));
                }
            });

        card()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(i18n::t("islands.monitors").into_owned()),
            )
            .child(div().flex().justify_center().child(preview))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x(SP_4)
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .font_features(tabular())
                    .children(d.monitors.iter().map(monitor_legend)),
            )
            .children(chips)
            .into_any_element()
    }

    fn island_card(&self, d: &Data, off: bool, cx: &mut Context<Self>) -> AnyElement {
        let hint = || {
            card()
                .text_color(text_muted())
                .child(i18n::t("islands.hint").into_owned())
                .into_any_element()
        };
        let Some((lid, iid)) = self.selected.clone() else {
            return hint();
        };
        let Some(layout) = d.layouts.iter().find(|l| l.id == lid) else {
            return hint();
        };
        let Some(isl) = layout.islands.iter().find(|i| i.id == iid) else {
            return hint();
        };
        let mon = layout_monitor(&layout.monitor, &d.monitors)
            .and_then(|mi| d.monitors.iter().find(|m| m.index == mi));
        let monitor_ok = mon.is_some();
        let size = format_size(isl.rect.width(), isl.rect.height());
        let fractions = mon.map(|m| fraction_label(isl.rect, m.rect));
        let occ = occupants_of(isl, &d.windows);
        let island_ref = format!("{lid}/{iid}");

        let occupant_rows = occ.iter().map(|o| {
            let hwnd = o.hwnd;
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(SP_2)
                .child(div().flex_1().min_w_0().truncate().child(o.label.clone()))
                .child(
                    secondary_button(
                        format!("restore-{hwnd}"),
                        i18n::t("islands.restore").into_owned(),
                        cx.listener(move |t, _, _, cx| t.run(Action::Restore { hwnd }, cx)),
                    )
                    .disabled(off || self.busy),
                )
        });

        let edit_id = lid.clone();
        let mut c = card()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_2)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(iid.clone()))
                    .child(badge(
                        i18n::tf("islands.layout_badge", &[("layout", &layout_label(&lid))]),
                        BadgeKind::Neutral,
                    ))
                    .child(badge(
                        i18n::tf("islands.occupants_badge", &[("n", &occ.len())]),
                        if occ.is_empty() {
                            BadgeKind::Neutral
                        } else {
                            BadgeKind::Info
                        },
                    )),
            )
            .child(
                div()
                    .text_color(text_muted())
                    .font_features(tabular())
                    .child(match fractions {
                        Some(f) => format!("{size} · {f}"),
                        None => size,
                    }),
            );
        if occ.is_empty() {
            c = c.child(
                div()
                    .text_color(text_muted())
                    .child(i18n::t("islands.no_windows").into_owned()),
            );
        } else {
            c = c.child(div().flex().flex_col().gap(SP_2).children(occupant_rows));
        }
        c = c.child(
            div()
                .flex()
                .flex_wrap()
                .gap(SP_2)
                .child(
                    secondary_button(
                        "island-place",
                        i18n::t("islands.place_window").into_owned(),
                        cx.listener(|t, _, window, cx| t.open_picker(window, cx)),
                    )
                    .disabled(off || self.busy || !monitor_ok),
                )
                .child(secondary_button(
                    "island-edit",
                    i18n::t("islands.edit").into_owned(),
                    cx.listener(move |_, _, _, cx| {
                        cx.emit(OpenEditor {
                            layout: Some(edit_id.clone()),
                        });
                    }),
                )),
        );
        if self.picker_open {
            c = c.child(self.picker(d, island_ref, monitor_ok, off, cx));
        }
        c.into_any_element()
    }

    fn picker(
        &self,
        d: &Data,
        island_ref: String,
        monitor_ok: bool,
        off: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.search.read(cx).value().to_string();
        let wins = picker_windows(&d.windows, &query);
        let chosen_win = self
            .picker_sel
            .and_then(|h| d.windows.iter().find(|w| w.hwnd == h));
        let can_place = !off && !self.busy && placeable(monitor_ok, chosen_win);

        let rows = wins.iter().map(|w| {
            let hwnd = w.hwnd;
            let elevated = w.elevated == Some(true);
            let sel = self.picker_sel == Some(hwnd);
            div()
                .id(format!("pick-{hwnd}"))
                .min_h(HIT)
                .px(SP_3)
                .flex()
                .items_center()
                .justify_between()
                .gap(SP_2)
                .rounded(R_SM)
                .when(sel, |d| d.bg(accent().opacity(0.14)))
                .when(elevated, |d| d.opacity(0.45))
                .when(!elevated, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(text().opacity(0.04)))
                        .on_click(cx.listener(move |t, _, _, cx| {
                            t.picker_sel = Some(hwnd);
                            cx.notify();
                        }))
                })
                .child(div().flex_1().min_w_0().truncate().child(occupant_label(
                    hwnd,
                    Some(&w.process),
                    Some(&truncate_chars(&w.title, TITLE_MAX)),
                )))
                .when(elevated, |d| {
                    d.child(badge(
                        i18n::t("windows.row.elevated").into_owned(),
                        BadgeKind::Warn,
                    ))
                })
        });

        let list = div()
            .id("picker-list")
            .max_h(px(PICKER_MAX_H))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(SP_1)
            .children(rows)
            .when(wins.is_empty(), |d| {
                d.child(
                    div()
                        .p(SP_3)
                        .text_color(text_muted())
                        .child(i18n::t("islands.picker.no_match").into_owned()),
                )
            });

        div()
            .flex()
            .flex_col()
            .gap(SP_2)
            .p(SP_3)
            .rounded(R_SM)
            .bg(raised())
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(i18n::t("islands.picker.title").into_owned()),
            )
            .child(Input::new(&self.search))
            .child(list)
            .child(
                div()
                    .flex()
                    .gap(SP_2)
                    .child(
                        primary_button(
                            "picker-go",
                            i18n::t("islands.picker.place").into_owned(),
                            cx.listener(move |t, _, _, cx| {
                                if let Some(hwnd) = t.picker_sel {
                                    t.run(
                                        Action::Place {
                                            hwnd,
                                            island: island_ref.clone(),
                                        },
                                        cx,
                                    );
                                }
                            }),
                        )
                        .disabled(!can_place),
                    )
                    .child(secondary_button(
                        "picker-cancel",
                        i18n::t("islands.cancel").into_owned(),
                        cx.listener(|t, _, _, cx| {
                            t.picker_open = false;
                            t.picker_sel = None;
                            cx.notify();
                        }),
                    )),
            )
            .into_any_element()
    }

    fn unavailable_card(&self, l: &Layout, cx: &mut Context<Self>) -> AnyElement {
        let id = l.id.clone();
        let names: Vec<&str> = l.islands.iter().map(|i| i.id.as_str()).collect();
        card()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(SP_2)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(layout_label(&l.id)),
                    )
                    .child(badge(
                        i18n::t("islands.unavailable.badge").into_owned(),
                        BadgeKind::Warn,
                    )),
            )
            .child(div().text_color(text_muted()).child(i18n::tf(
                "islands.unavailable.body",
                &[("monitor", &monitor_sel_text(&l.monitor))],
            )))
            .child(div().text_color(text_muted()).child(i18n::tf(
                "islands.unavailable.islands",
                &[("names", &names.join(", "))],
            )))
            .child(div().flex().child(secondary_button(
                format!("unavailable-edit-{id}"),
                i18n::t("islands.edit").into_owned(),
                cx.listener(move |_, _, _, cx| {
                    cx.emit(OpenEditor {
                        layout: Some(id.clone()),
                    });
                }),
            )))
            .into_any_element()
    }

    fn content(&self, d: &Data, viewport_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let off = self.error.is_some();
        if d.layouts.is_empty() && d.missing.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap(SP_3)
                .child(empty_state(
                    i18n::t("islands.empty.title").into_owned(),
                    i18n::t("islands.empty.body").into_owned(),
                ))
                .child(div().flex().justify_center().child(self.create_button(cx)))
                .into_any_element();
        }
        let unavailable: Vec<AnyElement> = d
            .missing
            .iter()
            .map(|l| self.unavailable_card(l, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(SP_3)
            .when(off, |d| d.opacity(0.5))
            .child(self.preview_card(d, viewport_w, cx))
            .child(self.island_card(d, off, cx))
            .children(unavailable)
            .into_any_element()
    }

    fn create_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        secondary_button(
            "islands-create",
            i18n::t("islands.create").into_owned(),
            cx.listener(|_, _, _, cx| cx.emit(OpenEditor { layout: None })),
        )
        .icon(IconName::Plus)
    }
}

impl Render for IslandsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_locale(window, cx);
        // A view that paints is visible: reload when it (re)appears after being hidden.
        if self.last_render.elapsed() > VISIBLE_GRACE && !self.loading && !self.busy {
            self.refresh(cx);
        }
        self.last_render = Instant::now();
        self.active = window.is_window_active();
        let viewport_w = f32::from(window.viewport_size().width);

        let body: AnyElement = match (&self.data, &self.error) {
            (Some(d), _) => self.content(d, viewport_w, cx),
            (None, Some(f)) => self.error_card(f, cx),
            (None, None) => div()
                .text_color(text_muted())
                .child(i18n::t("islands.loading").into_owned())
                .into_any_element(),
        };
        let banner = self
            .data
            .as_ref()
            .and(self.error.as_ref())
            .map(|f| self.error_card(f, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(SP_3)
            .child(section_header(
                i18n::t("gui.section.islands").into_owned(),
                i18n::t("islands.subtitle").into_owned(),
            ))
            .when(
                self.data.as_ref().is_some_and(|d| !d.layouts.is_empty()),
                |d| d.child(div().flex().child(self.create_button(cx))),
            )
            .children(banner)
            .child(
                div()
                    .id("islands-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(SP_4)
                    .child(body),
            )
            // Footer: pushes nothing and overlays nothing, so the monitor card stays readable.
            .children(self.notice_view().map(|n| div().flex_shrink_0().child(n)))
    }
}
// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{ClientSize, WindowMonitor};
    use crate::win32::DpiAwareness;
    use crate::window_state::Rect;

    fn rect(l: i32, t: i32, r: i32, b: i32) -> Rect {
        Rect {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    fn win(hwnd: isize, process: &str, title: &str, elevated: Option<bool>) -> WindowInfo {
        WindowInfo {
            hwnd,
            pid: 1,
            process: process.into(),
            title: title.into(),
            class: String::new(),
            visible: true,
            style: "0x0".into(),
            exstyle: "0x0".into(),
            rect: rect(0, 0, 10, 10),
            client: ClientSize { w: 10, h: 10 },
            monitor: WindowMonitor {
                index: 0,
                device: "d".into(),
            },
            dpi: 96,
            dpi_awareness: DpiAwareness::PerMonitorV2,
            elevated,
            maximized: false,
            minimized: false,
            managed: false,
            island: None,
        }
    }

    fn mon(index: usize, device: &str, primary: bool) -> MonitorInfo {
        MonitorInfo {
            index,
            device: device.into(),
            rect: rect(index as i32 * 1920, 0, index as i32 * 1920 + 1920, 1080),
            dpi: 96,
            primary,
        }
    }

    fn island(id: &str, occupants: Vec<isize>) -> IslandInfo {
        IslandInfo {
            id: id.into(),
            rect: rect(0, 0, 960, 1080),
            occupants,
        }
    }

    fn layout(id: &str, monitor: MonitorSel, islands: Vec<IslandInfo>) -> LayoutInfo {
        LayoutInfo {
            id: id.into(),
            monitor,
            islands,
        }
    }

    #[test]
    fn occupants_resolve_process_and_title_in_island_order() {
        let wins = vec![win(2, "b.exe", "B", None), win(1, "a.exe", "A", None)];
        let occ = occupants_of(&island("i", vec![1, 2]), &wins);
        assert_eq!(occ.len(), 2);
        assert_eq!(occ[0].hwnd, 1);
        assert_eq!(occ[0].label, "a.exe — A");
        assert_eq!(occ[0].process.as_deref(), Some("a.exe"));
        assert_eq!(occ[1].label, "b.exe — B");
    }

    #[test]
    fn unknown_occupant_has_a_label_without_process() {
        let occ = occupants_of(&island("i", vec![77]), &[]);
        assert_eq!(
            occ,
            vec![Occupant {
                hwnd: 77,
                label: occupant_label(77, None, None),
                process: None
            }]
        );
    }

    #[test]
    fn empty_island_has_no_occupants() {
        assert!(occupants_of(&island("i", vec![]), &[win(1, "a", "A", None)]).is_empty());
    }

    #[test]
    fn placeable_needs_monitor_and_non_elevated_window() {
        let normal = win(1, "a", "A", Some(false));
        let unknown = win(2, "b", "B", None);
        let elevated = win(3, "c", "C", Some(true));
        assert!(placeable(true, Some(&normal)));
        assert!(placeable(true, Some(&unknown)));
        assert!(!placeable(true, Some(&elevated)));
        assert!(!placeable(false, Some(&normal)));
        assert!(!placeable(true, None));
    }

    #[test]
    fn picker_filters_by_query_drops_untitled_and_sorts() {
        let all = vec![
            win(1, "zeta.exe", "Zeta", None),
            win(2, "alfa.exe", "Notes of ALFA", None),
            win(3, "alfa.exe", "", None),
            win(4, "beta.exe", "Beta", Some(true)),
        ];
        let q = |s| {
            picker_windows(&all, s)
                .iter()
                .map(|w| w.hwnd)
                .collect::<Vec<_>>()
        };
        assert_eq!(q(""), vec![2, 4, 1]);
        assert_eq!(q("alfa"), vec![2]);
        assert_eq!(q("  ZETA "), vec![1]);
        assert!(q("nothing").is_empty());
    }

    #[test]
    fn layouts_group_by_monitor_and_missing_monitors_are_apart() {
        let monitors = vec![
            mon(0, "\\\\.\\DISPLAY1", true),
            mon(1, "\\\\.\\DISPLAY2", false),
        ];
        let layouts = vec![
            layout("a", MonitorSel::Primary, vec![]),
            layout("b", MonitorSel::Index(0), vec![]),
            layout("c", MonitorSel::Device("\\\\.\\display2".into()), vec![]),
            layout("d", MonitorSel::Index(7), vec![]),
        ];
        assert_eq!(layout_monitor(&layouts[0].monitor, &monitors), Some(0));
        assert_eq!(layout_monitor(&layouts[2].monitor, &monitors), Some(1));
        assert_eq!(layout_monitor(&layouts[3].monitor, &monitors), None);
        assert_eq!(layouts_on(&layouts, &monitors, 0), vec![0, 1]);
        assert_eq!(layouts_on(&layouts, &monitors, 1), vec![2]);
        let profile = vec![
            Layout {
                id: "a".into(),
                monitor: MonitorSel::Primary,
                islands: vec![],
            },
            Layout {
                id: "d".into(),
                monitor: MonitorSel::Index(7),
                islands: vec![],
            },
            // Not listed but its monitor exists: not "missing", just not reported.
            Layout {
                id: "x".into(),
                monitor: MonitorSel::Primary,
                islands: vec![],
            },
        ];
        let missing = missing_layouts(&profile, &layouts[..3], &monitors);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "d");
    }

    #[test]
    fn active_layout_follows_the_choice_else_the_first() {
        let monitors = vec![mon(0, "d", true)];
        let layouts = vec![
            layout("a", MonitorSel::Primary, vec![]),
            layout("b", MonitorSel::Primary, vec![]),
        ];
        assert_eq!(active_layout(&layouts, &monitors, Some("b"), 0), Some(1));
        assert_eq!(active_layout(&layouts, &monitors, Some("gone"), 0), Some(0));
        assert_eq!(active_layout(&layouts, &monitors, None, 0), Some(0));
        assert_eq!(active_layout(&layouts, &monitors, None, 5), None);
    }

    #[test]
    fn selection_exists_checks_layout_and_island() {
        let layouts = vec![layout("a", MonitorSel::Primary, vec![island("l", vec![])])];
        assert!(selection_exists(&layouts, "a", "l"));
        assert!(!selection_exists(&layouts, "a", "r"));
        assert!(!selection_exists(&layouts, "b", "l"));
    }

    #[test]
    fn missing_monitor_is_described_by_its_selector() {
        let _g = i18n::scoped_locale(Locale::En);
        assert_eq!(
            monitor_sel_text(&MonitorSel::Index(3)),
            "the monitor with index 3"
        );
        assert_eq!(
            monitor_sel_text(&MonitorSel::Device("\\\\.\\DISPLAY9".into())),
            "the monitor \\\\.\\DISPLAY9"
        );
        assert_eq!(
            monitor_sel_text(&MonitorSel::Primary),
            "the primary monitor"
        );
    }

    #[test]
    fn monitor_legend_names_number_size_and_primary() {
        let _g = i18n::scoped_locale(Locale::En);
        assert_eq!(
            monitor_legend(&mon(0, "d", true)),
            "Monitor 1 · 1920×1080 px · primary"
        );
        assert_eq!(
            monitor_legend(&mon(1, "d", false)),
            "Monitor 2 · 1920×1080 px"
        );
        i18n::set_locale(Locale::Es);
        assert_ne!(
            monitor_legend(&mon(0, "d", true)),
            "Monitor 1 · 1920×1080 px · primary"
        );
    }
}
