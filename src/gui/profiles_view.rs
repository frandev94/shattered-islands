//! "Profiles": list, add, edit, enable/disable and remove profiles (`get_profiles` /
//! `set_profiles`). The pure half (form -> `Profile`, validation, sorting, warning texts) is at
//! the top and unit-tested; the GPUI half below only renders and talks IPC.

use std::time::{Duration, Instant};

use gpui_kit::AppContext as _;
use gpui_kit::component::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    ClickEvent, Context, Div, ElementId, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Subscription, Window, div, px,
};
use serde_json::{Value, json};

use super::theme::{
    BadgeKind, HIT, R_SM, SP_1, SP_2, SP_3, SP_4, SP_6, TEXT_MD, TEXT_SM, accent, badge, card,
    danger, empty_state, primary_button, raised, secondary_button, section_header, tabular, text,
    text_muted, warn,
};
use super::{Reply, code_str, ipc_call, json_err, remote_message, split};
use crate::i18n::{self, Locale};
use crate::ipc::{
    CallError, ErrorCode, GetProfilesResult, ListIslandsResult, ListWindowsResult,
    SetProfilesParams, SetProfilesResult,
};
use crate::islands::{Layout, Size, canonical_ref, merged_layouts};
use crate::profiles::{MonitorChoice, Profile, Warning, is_excluded};
use crate::window_state::Offsets;

// =================================================================== pure logic

/// Engine ranges (docs/V0.2.md): kept here so the form rejects what the engine would.
pub const MAX_OFFSET: i64 = 1_000_000;
pub const MAX_RESOLUTION: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Process,
    Left,
    Top,
    Right,
    Bottom,
    Delay,
    Width,
    Height,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: Field,
    pub message: String,
}

/// Raw content of the form: free-text fields stay strings until validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormInput {
    pub process: String,
    pub enabled: bool,
    pub island: Option<String>,
    pub monitor: MonitorChoice,
    /// left, top, right, bottom
    pub offsets: [String; 4],
    pub delay: String,
    pub width: String,
    pub height: String,
}

impl FormInput {
    pub fn blank() -> Self {
        Self {
            process: String::new(),
            enabled: true,
            island: None,
            monitor: MonitorChoice::Current,
            offsets: std::array::from_fn(|_| "0".into()),
            delay: "0".into(),
            width: String::new(),
            height: String::new(),
        }
    }

    pub fn from_profile(p: &Profile) -> Self {
        let o = p.offsets;
        Self {
            process: p.process.clone(),
            enabled: p.enabled,
            island: p.island.clone(),
            monitor: p.monitor,
            offsets: [o.left, o.top, o.right, o.bottom].map(|v| v.to_string()),
            delay: p.delay_ms.to_string(),
            width: p
                .game_resolution
                .map(|s| s.width.to_string())
                .unwrap_or_default(),
            height: p
                .game_resolution
                .map(|s| s.height.to_string())
                .unwrap_or_default(),
        }
    }
}

fn base_name(s: &str) -> &str {
    s.rsplit(['\\', '/']).next().unwrap_or(s)
}

fn same_process(a: &str, b: &str) -> bool {
    base_name(a).eq_ignore_ascii_case(base_name(b))
}

/// Parse `text` as an integer in `lo..=hi`; an empty field counts as `empty` (None = required).
fn parse_in(text: &str, lo: i64, hi: i64, empty: Option<i64>) -> Result<i64, String> {
    let t = text.trim();
    if t.is_empty() {
        return empty.ok_or_else(|| i18n::t("profiles.error.number_required").into_owned());
    }
    let v: i64 = t
        .parse()
        .map_err(|_| i18n::t("profiles.error.integer_required").into_owned())?;
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(i18n::tf(
            "profiles.error.range",
            &[("lo", &lo), ("hi", &hi)],
        ))
    }
}

/// Form -> profile. `existing` are the saved profiles (for duplicates); `original` is the process
/// being edited (its own entry is not a duplicate). With an island chosen, monitor and offsets
/// are not validated and are stored as default (the island overrides them).
pub fn build_profile(
    f: &FormInput,
    existing: &[Profile],
    original: Option<&str>,
) -> Result<Profile, Vec<FieldError>> {
    let mut errs = Vec::new();
    let mut err = |field, message: String| errs.push(FieldError { field, message });

    let process = f.process.trim();
    if process.is_empty() {
        err(
            Field::Process,
            i18n::t("profiles.error.process_required").into_owned(),
        );
    } else if is_excluded(process) {
        err(
            Field::Process,
            i18n::t("profiles.error.process_excluded").into_owned(),
        );
    } else if existing.iter().any(|p| {
        same_process(&p.process, process) && original.is_none_or(|o| !same_process(o, process))
    }) {
        err(
            Field::Process,
            i18n::t("profiles.error.process_duplicate").into_owned(),
        );
    }

    let mut offsets = Offsets::default();
    if f.island.is_none() {
        let fields = [Field::Left, Field::Top, Field::Right, Field::Bottom];
        let mut vals = [0i32; 4];
        for (i, field) in fields.into_iter().enumerate() {
            match parse_in(&f.offsets[i], -MAX_OFFSET, MAX_OFFSET, Some(0)) {
                Ok(v) => vals[i] = v as i32,
                Err(m) => err(field, m),
            }
        }
        offsets = Offsets {
            left: vals[0],
            top: vals[1],
            right: vals[2],
            bottom: vals[3],
        };
    }

    let delay = match parse_in(&f.delay, 0, crate::ipc::MAX_DELAY_MS as i64, Some(0)) {
        Ok(v) => v as u64,
        Err(m) => {
            err(Field::Delay, m);
            0
        }
    };

    let game_resolution = match (f.width.trim().is_empty(), f.height.trim().is_empty()) {
        (true, true) => None,
        (w_empty, h_empty) => {
            let mut dims = [0i32; 2];
            for (i, (field, text, empty)) in [
                (Field::Width, &f.width, w_empty),
                (Field::Height, &f.height, h_empty),
            ]
            .into_iter()
            .enumerate()
            {
                if empty {
                    err(
                        field,
                        i18n::t("profiles.error.resolution_pair").into_owned(),
                    );
                } else {
                    match parse_in(text, 1, MAX_RESOLUTION, None) {
                        Ok(v) => dims[i] = v as i32,
                        Err(m) => err(field, m),
                    }
                }
            }
            Some(Size {
                width: dims[0],
                height: dims[1],
            })
        }
    };

    if !errs.is_empty() {
        return Err(errs);
    }
    let island = f.island.clone();
    Ok(Profile {
        process: process.to_owned(),
        enabled: f.enabled,
        monitor: if island.is_some() {
            MonitorChoice::Current
        } else {
            f.monitor
        },
        offsets,
        delay_ms: delay,
        clip_cursor: false,
        island,
        game_resolution,
        extra: Default::default(),
    })
}

/// Indices into `profiles` that match `query` (process or island, case-insensitive), sorted by
/// process name (case-insensitive, stable).
pub fn sorted_filtered(profiles: &[Profile], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut idx: Vec<usize> = (0..profiles.len())
        .filter(|&i| {
            let p = &profiles[i];
            q.is_empty()
                || p.process.to_lowercase().contains(&q)
                || p.island
                    .as_deref()
                    .is_some_and(|s| s.to_lowercase().contains(&q))
        })
        .collect();
    idx.sort_by_cached_key(|&i| profiles[i].process.to_lowercase());
    idx
}

/// Canonical form of a legacy island ref that still resolves; `None` when the ref is already
/// canonical or does not resolve at all.
pub fn legacy_target(layouts: &[Layout], island: &str) -> Option<String> {
    canonical_ref(layouts, island).filter(|c| c != island)
}

/// The ref a picker shows for `island`: its canonical form when it resolves, else itself.
fn shown_ref(layouts: &[Layout], island: &str) -> String {
    canonical_ref(layouts, island).unwrap_or_else(|| island.to_owned())
}

/// One-line description of what the profile applies. `layouts` resolve legacy island refs.
pub fn summary(p: &Profile, layouts: &[Layout]) -> String {
    let target = match (&p.island, p.monitor) {
        (Some(i), _) => match legacy_target(layouts, i) {
            Some(c) => i18n::tf(
                "profiles.summary.island_alias",
                &[("island", i), ("canonical", &c)],
            ),
            None => i18n::tf("profiles.summary.island", &[("island", i)]),
        },
        (None, MonitorChoice::Current) => i18n::t("profiles.summary.current_monitor").into_owned(),
        (None, MonitorChoice::Index(n)) => i18n::tf("profiles.summary.monitor", &[("n", &n)]),
    };
    let o = p.offsets;
    let mut parts = vec![target];
    if p.island.is_none() && o != Offsets::default() {
        parts.push(i18n::tf(
            "profiles.summary.offsets",
            &[
                ("left", &o.left),
                ("top", &o.top),
                ("right", &o.right),
                ("bottom", &o.bottom),
            ],
        ));
    }
    if p.delay_ms > 0 {
        parts.push(i18n::tf("profiles.summary.delay", &[("ms", &p.delay_ms)]));
    }
    if let Some(s) = p.game_resolution {
        parts.push(i18n::tf(
            "profiles.summary.game",
            &[("w", &s.width), ("h", &s.height)],
        ));
    }
    parts.join(" · ")
}

/// Island choices (canonical ids only): every `layout/island` plus the profile's current one even
/// if it no longer exists; a legacy ref is shown as the canonical island it resolves to.
pub fn island_options(layouts: &[Layout], known: &[String], current: Option<&str>) -> Vec<String> {
    let mut v = known.to_vec();
    if let Some(c) = current {
        let shown = shown_ref(layouts, c);
        if !v.contains(&shown) {
            v.push(shown);
        }
    }
    v
}

/// Process-name suggestions from the open windows: unique (case-insensitive), not excluded, not
/// already profiled, matching what is typed; at most `max`.
pub fn suggestions(
    window_processes: &[String],
    profiles: &[Profile],
    typed: &str,
    max: usize,
) -> Vec<String> {
    let q = typed.trim().to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for p in window_processes {
        let p = base_name(p);
        if p.is_empty()
            || is_excluded(p)
            || profiles.iter().any(|x| same_process(&x.process, p))
            || !p.to_lowercase().contains(&q)
            || out.iter().any(|o| o.eq_ignore_ascii_case(p))
        {
            continue;
        }
        out.push(p.to_owned());
    }
    out.sort_by_cached_key(|s| s.to_lowercase());
    out.truncate(max);
    out
}

fn wstr<'a>(w: &'a Warning, key: &str) -> &'a str {
    w.data.get(key).and_then(Value::as_str).unwrap_or("?")
}

/// Text for a profile-file warning (`code` is stable, `data` has the context).
pub fn warning_text(w: &Warning) -> String {
    let (process, island, layout) = (wstr(w, "process"), wstr(w, "island"), wstr(w, "layout"));
    match w.code.as_str() {
        "island_overrides_monitor" => i18n::tf(
            "profiles.warning.island_overrides_monitor",
            &[("process", &process)],
        ),
        "bad_island_ref" => i18n::tf(
            "profiles.warning.bad_island_ref",
            &[("process", &process), ("island", &island)],
        ),
        "island_not_found" => i18n::tf(
            "profiles.warning.island_not_found",
            &[("process", &process), ("island", &island)],
        ),
        "bad_game_resolution" => i18n::tf(
            "profiles.warning.bad_game_resolution",
            &[("process", &process)],
        ),
        "game_resolution_exceeds_island" => i18n::tf(
            "profiles.warning.game_resolution_exceeds_island",
            &[("process", &process)],
        ),
        "bad_layout_id" => i18n::tf("profiles.warning.bad_layout_id", &[("layout", &layout)]),
        "duplicate_layout" => i18n::tf("profiles.warning.duplicate_layout", &[("layout", &layout)]),
        "bad_island_id" => i18n::tf(
            "profiles.warning.bad_island_id",
            &[("island", &island), ("layout", &layout)],
        ),
        "duplicate_island" => i18n::tf(
            "profiles.warning.duplicate_island",
            &[("island", &island), ("layout", &layout)],
        ),
        "bad_island_rect" => i18n::tf(
            "profiles.warning.bad_island_rect",
            &[("island", &island), ("layout", &layout)],
        ),
        other => i18n::tf("profiles.warning.unknown", &[("code", &other)]),
    }
}

/// What a reload means for the open editor of `original` (`None` = a new profile).
#[derive(Debug, Clone, PartialEq)]
pub enum OutsideChange {
    /// The edited profile is the same in the new data: the form stays as typed.
    None,
    /// It changed outside: the form must be refilled from this version.
    Changed(Profile),
    /// It was removed outside: saving the form creates it again.
    Removed,
}

/// Compares the edited profile in the data the form came from (`old`) with the reloaded `new`.
pub fn outside_change(original: Option<&str>, old: &[Profile], new: &[Profile]) -> OutsideChange {
    let Some(o) = original else {
        return OutsideChange::None;
    };
    let find = |ps: &[Profile]| ps.iter().find(|p| same_process(&p.process, o)).cloned();
    match (find(old), find(new)) {
        (Some(_), None) => OutsideChange::Removed,
        (before, Some(now)) if before.as_ref() != Some(&now) => OutsideChange::Changed(now),
        _ => OutsideChange::None,
    }
}

// ======================================================================== IPC glue

struct Data {
    path: String,
    revision: String,
    profiles: Vec<Profile>,
    warnings: Vec<Warning>,
    /// Built-in presets plus the saved layouts: what a profile's island ref resolves against.
    layouts: Vec<Layout>,
    islands: Vec<String>,
    monitors: Vec<(usize, String)>,
    processes: Vec<String>,
}

fn load(c: &mut crate::ipc::pipe::Client) -> Result<Reply<Data>, CallError> {
    let gp = match split(c.call("get_profiles", json!({})))? {
        Reply::Ok(v) => serde_json::from_value::<GetProfilesResult>(v).map_err(json_err)?,
        Reply::Remote(b) => return Ok(Reply::Remote(b)),
    };
    // Pickers are conveniences: a failure there must not hide the profiles.
    let isl = c
        .call("list_islands", json!({}))
        .ok()
        .and_then(|v| serde_json::from_value::<ListIslandsResult>(v).ok());
    let wins = c
        .call("list_windows", json!({}))
        .ok()
        .and_then(|v| serde_json::from_value::<ListWindowsResult>(v).ok());
    let (islands, monitors) = isl.map_or_else(Default::default, |r| {
        let islands = r
            .layouts
            .iter()
            .flat_map(|l| l.islands.iter().map(move |i| format!("{}/{}", l.id, i.id)))
            .collect();
        let monitors = r
            .monitors
            .iter()
            .map(|m| (m.index, m.device.clone()))
            .collect();
        (islands, monitors)
    });
    Ok(Reply::Ok(Data {
        path: gp.path,
        revision: gp.revision,
        profiles: gp.profiles,
        warnings: gp.warnings,
        layouts: merged_layouts(&gp.layouts),
        islands,
        monitors,
        processes: wins
            .map(|w| w.windows.into_iter().map(|x| x.process).collect())
            .unwrap_or_default(),
    }))
}

#[derive(Clone)]
enum Op {
    Upsert(Profile),
    Remove(String),
}

// ===================================================================== the view

struct Editor {
    /// Process being edited (None = new profile).
    original: Option<String>,
    process: Entity<InputState>,
    offsets: [Entity<InputState>; 4],
    delay: Entity<InputState>,
    width: Entity<InputState>,
    height: Entity<InputState>,
    enabled: bool,
    island: Option<String>,
    monitor: MonitorChoice,
    _subs: Vec<Subscription>,
}

pub struct ProfilesView {
    data: Option<Data>,
    /// Last load failure (kept next to old data, never replaces it).
    error: Option<String>,
    /// `unsupported_version` on disk: the engine refuses writes, so we only show the message.
    read_only: bool,
    loading: bool,
    saving: bool,
    editor: Option<Editor>,
    notice: Option<String>,
    save_error: Option<String>,
    /// Warnings of the last `set_profiles` answer (replace the file's while visible).
    save_warnings: Vec<Warning>,
    conflict: bool,
    /// The edited profile changed outside: refill the form from it at the next render (it
    /// needs the window).
    refill: Option<Profile>,
    /// Why the open form no longer matches what it was opened with (shown while editing).
    outside: Option<String>,
    confirm_delete: Option<String>,
    last_render: Instant,
    /// Language the editor placeholders were last set for.
    locale: Locale,
}

fn new_input(
    window: &mut Window,
    cx: &mut Context<ProfilesView>,
    placeholder: &str,
    value: &str,
    subs: &mut Vec<Subscription>,
) -> Entity<InputState> {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.to_owned())
            .default_value(value.to_owned())
    });
    subs.push(cx.subscribe(&input, |_, _, e: &InputEvent, cx| {
        if matches!(e, InputEvent::Change) {
            cx.notify();
        }
    }));
    input
}

impl ProfilesView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut v = Self {
            data: None,
            error: None,
            read_only: false,
            loading: false,
            saving: false,
            editor: None,
            notice: None,
            save_error: None,
            save_warnings: Vec::new(),
            conflict: false,
            refill: None,
            outside: None,
            confirm_delete: None,
            last_render: Instant::now(),
            locale: i18n::locale(),
        };
        v.refresh(cx);
        // Poll every 5 s, only while the view is on screen and there is no draft to protect.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let Ok(due) = this.update(cx, |t, _| {
                    t.editor.is_none()
                        && !t.saving
                        && !t.conflict
                        && t.confirm_delete.is_none()
                        && t.last_render.elapsed() < Duration::from_secs(2)
                }) else {
                    break;
                };
                if due {
                    this.update(cx, |t, cx| t.refresh(cx)).ok();
                }
            }
        })
        .detach();
        v
    }

    /// Re-set the editor placeholders after a language switch.
    fn sync_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let l = i18n::locale();
        if l == self.locale {
            return;
        }
        self.locale = l;
        let Some(e) = &self.editor else { return };
        let mut set = |input: &Entity<InputState>, text: String, cx: &mut Context<Self>| {
            input.update(cx, |s, cx| s.set_placeholder(text, window, cx));
        };
        set(
            &e.process,
            i18n::t("profiles.editor.process_placeholder").into_owned(),
            cx,
        );
        set(
            &e.width,
            i18n::t("profiles.editor.width_placeholder").into_owned(),
            cx,
        );
        set(
            &e.height,
            i18n::t("profiles.editor.height_placeholder").into_owned(),
            cx,
        );
    }

    /// Hook for Diagnostics: open the editor for `process` (existing profile or a new one) with
    /// `game_resolution` preloaded, without saving.
    pub fn prefill_resolution(
        &mut self,
        process: &str,
        width: i32,
        height: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.data.as_ref().and_then(|d| {
            d.profiles
                .iter()
                .find(|p| same_process(&p.process, process))
                .cloned()
        });
        let mut form = existing
            .as_ref()
            .map_or_else(FormInput::blank, FormInput::from_profile);
        if existing.is_none() {
            form.process = process.to_owned();
        }
        form.width = width.to_string();
        form.height = height.to_string();
        self.open_editor(&form, existing.map(|p| p.process), window, cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let r = ipc_call(cx, load).await;
            this.update(cx, |t, cx| {
                t.loading = false;
                match r {
                    Ok(Reply::Ok(d)) => {
                        t.error = None;
                        t.read_only = false;
                        // Never keep stale form values silently over an outside change.
                        if let (Some(e), Some(old)) = (&mut t.editor, &t.data) {
                            match outside_change(e.original.as_deref(), &old.profiles, &d.profiles)
                            {
                                OutsideChange::None => {}
                                OutsideChange::Changed(p) => t.refill = Some(p),
                                OutsideChange::Removed => {
                                    t.outside = e.original.take().map(|o| {
                                        i18n::tf("profiles.outside.removed", &[("process", &o)])
                                    });
                                }
                            }
                        }
                        t.data = Some(d);
                    }
                    Ok(Reply::Remote(b)) => {
                        t.read_only = b.code == ErrorCode::UnsupportedVersion;
                        t.error = Some(format!("{} ({})", remote_message(&b), code_str(b.code)));
                    }
                    Err(e) => t.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_editor(
        &mut self,
        f: &FormInput,
        original: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut subs = Vec::new();
        let process_ph = i18n::t("profiles.editor.process_placeholder");
        let process = new_input(window, cx, &process_ph, &f.process, &mut subs);
        let offsets = [0, 1, 2, 3].map(|i| new_input(window, cx, "0", &f.offsets[i], &mut subs));
        let delay = new_input(window, cx, "0", &f.delay, &mut subs);
        let width_ph = i18n::t("profiles.editor.width_placeholder");
        let width = new_input(window, cx, &width_ph, &f.width, &mut subs);
        let height_ph = i18n::t("profiles.editor.height_placeholder");
        let height = new_input(window, cx, &height_ph, &f.height, &mut subs);
        self.editor = Some(Editor {
            original,
            process,
            offsets,
            delay,
            width,
            height,
            enabled: f.enabled,
            island: f.island.clone(),
            monitor: f.monitor,
            _subs: subs,
        });
        self.notice = None;
        self.save_error = None;
        self.outside = None;
        cx.notify();
    }

    fn form(&self, cx: &gpui_kit::App) -> Option<FormInput> {
        let e = self.editor.as_ref()?;
        let v = |s: &Entity<InputState>| s.read(cx).value().to_string();
        Some(FormInput {
            process: v(&e.process),
            enabled: e.enabled,
            island: e.island.clone(),
            monitor: e.monitor,
            offsets: std::array::from_fn(|i| v(&e.offsets[i])),
            delay: v(&e.delay),
            width: v(&e.width),
            height: v(&e.height),
        })
    }

    fn submit(&mut self, op: Op, cx: &mut Context<Self>) {
        let Some(revision) = self.data.as_ref().map(|d| d.revision.clone()) else {
            return;
        };
        self.saving = true;
        self.save_error = None;
        self.notice = None;
        cx.notify();
        let done = match &op {
            Op::Upsert(p) => i18n::tf("profiles.saved", &[("process", &p.process)]),
            Op::Remove(n) => i18n::tf("profiles.removed", &[("process", n)]),
        };
        cx.spawn(async move |this, cx| {
            let r = ipc_call(cx, move |c| {
                let mut p = SetProfilesParams {
                    expected_revision: Some(revision),
                    ..Default::default()
                };
                match op {
                    Op::Upsert(x) => p.profiles = Some(vec![x]),
                    Op::Remove(n) => p.remove = Some(vec![n]),
                }
                let params = serde_json::to_value(p).map_err(json_err)?;
                match split(c.call("set_profiles", params))? {
                    Reply::Ok(v) => Ok(Reply::Ok(
                        serde_json::from_value::<SetProfilesResult>(v).map_err(json_err)?,
                    )),
                    Reply::Remote(b) => Ok(Reply::Remote(b)),
                }
            })
            .await;
            this.update(cx, |t, cx| {
                t.saving = false;
                match r {
                    Ok(Reply::Ok(res)) => {
                        t.save_warnings = res.warnings;
                        t.editor = None;
                        t.confirm_delete = None;
                        t.conflict = false;
                        t.notice = Some(done);
                        t.refresh(cx);
                    }
                    Ok(Reply::Remote(b)) if b.code == ErrorCode::RevisionConflict => {
                        t.conflict = true;
                    }
                    Ok(Reply::Remote(b)) => t.save_error = Some(remote_message(&b)),
                    Err(e) => t.save_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let (Some(form), Some(d)) = (self.form(cx), self.data.as_ref()) else {
            return;
        };
        let original = self.editor.as_ref().and_then(|e| e.original.clone());
        if let Ok(p) = build_profile(&form, &d.profiles, original.as_deref()) {
            self.submit(Op::Upsert(p), cx);
        }
    }

    fn toggle(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(mut p) = self.data.as_ref().and_then(|d| d.profiles.get(i).cloned()) else {
            return;
        };
        p.enabled = !p.enabled;
        self.submit(Op::Upsert(p), cx);
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
            .on_click(on_click)
    }
}

fn labeled(label: &str, child: impl IntoElement, error: Option<&str>) -> Div {
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
        .when_some(error.map(str::to_owned), |d, m| {
            d.child(div().text_size(TEXT_SM).text_color(danger()).child(m))
        })
}

fn banner(msg: impl Into<SharedString>, kind: BadgeKind) -> Div {
    let (tag, color) = match kind {
        BadgeKind::Error => (i18n::t("profiles.banner.error"), danger()),
        BadgeKind::Warn => (i18n::t("profiles.banner.warning"), warn()),
        _ => (i18n::t("profiles.banner.ok"), accent()),
    };
    card()
        .flex_row()
        .items_center()
        .gap(SP_3)
        .child(
            div()
                .text_color(color)
                .font_weight(FontWeight::SEMIBOLD)
                .child(tag.into_owned()),
        )
        .child(div().flex_1().min_w_0().child(msg.into()))
}

impl Render for ProfilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_locale(window, cx);
        if let Some(p) = self.refill.take().filter(|_| self.editor.is_some()) {
            let form = FormInput::from_profile(&p);
            self.open_editor(&form, Some(p.process.clone()), window, cx);
            self.outside = Some(i18n::tf(
                "profiles.outside.changed",
                &[("process", &p.process)],
            ));
        }
        self.last_render = Instant::now();
        let busy = self.saving;
        let editing = self.editor.is_some();

        let new_btn = {
            let on = cx.listener(|t, _, w, cx| {
                t.open_editor(&FormInput::blank(), None, w, cx);
            });
            let enabled = !self.read_only && self.data.is_some() && !editing && !busy;
            let btn = if editing {
                secondary_button("new-profile", i18n::t("profiles.new").into_owned(), on)
            } else {
                primary_button("new-profile", i18n::t("profiles.new").into_owned(), on)
            };
            btn.icon(IconName::Plus).disabled(!enabled)
        };
        let refresh = secondary_button(
            "refresh",
            i18n::t("profiles.refresh").into_owned(),
            cx.listener(|t, _, _, cx| t.refresh(cx)),
        )
        .icon(IconName::RefreshCw)
        .disabled(self.loading || busy);
        let head = div()
            .flex()
            .items_start()
            .justify_between()
            .gap(SP_3)
            .child(div().flex_1().min_w_0().child(section_header(
                i18n::t("gui.section.profiles").into_owned(),
                self.data.as_ref().map_or_else(
                    || i18n::t("profiles.subtitle").into_owned(),
                    |d| i18n::tf("profiles.subtitle_path", &[("path", &d.path)]),
                ),
            )))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap(SP_2)
                    .child(refresh)
                    .child(new_btn),
            );

        let mut body = div()
            .id("profiles-scroll")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(SP_4)
            .overflow_y_scroll();

        if let Some(e) = &self.error {
            body = body.child(banner(e.clone(), BadgeKind::Error));
        }
        if let Some(n) = &self.notice {
            body = body.child(banner(n.clone(), BadgeKind::Ok));
        }
        if let Some(n) = self.outside.as_ref().filter(|_| self.editor.is_some()) {
            body = body.child(banner(n.clone(), BadgeKind::Warn));
        }
        if let Some(e) = &self.save_error {
            body = body.child(banner(e.clone(), BadgeKind::Error));
        }
        if self.conflict {
            body = body.child(self.conflict_card(cx));
        }
        if let Some(d) = &self.data {
            let warnings = if self.save_warnings.is_empty() {
                &d.warnings
            } else {
                &self.save_warnings
            };
            if !warnings.is_empty() {
                body = body.child(
                    card()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(warn())
                                .child(i18n::t("profiles.warnings_title").into_owned()),
                        )
                        .children(warnings.iter().map(|w| {
                            div()
                                .text_size(TEXT_MD)
                                .text_color(text_muted())
                                .child(warning_text(w))
                        })),
                );
            }
        }
        if editing {
            body = body.child(self.editor_card(cx));
        }
        match &self.data {
            None if self.error.is_none() => {
                body = body.child(
                    div()
                        .text_color(text_muted())
                        .child(i18n::t("profiles.loading").into_owned()),
                );
            }
            None => {}
            Some(d) if d.profiles.is_empty() => {
                body = body.child(empty_state(
                    i18n::t("profiles.empty.title").into_owned(),
                    i18n::t("profiles.empty.body").into_owned(),
                ));
            }
            Some(_) => body = body.child(self.list_card(cx)),
        }
        // Old data stays visible but dimmed when the last refresh failed.
        let dim = self.error.is_some() && self.data.is_some();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(head)
            .child(body.when(dim, |b| b.opacity(0.85)))
    }
}

impl ProfilesView {
    fn conflict_card(&self, cx: &mut Context<Self>) -> Div {
        let draft = self.editor.is_some();
        card()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(warn())
                    .child(i18n::t("profiles.conflict.title").into_owned()),
            )
            .child(
                div()
                    .text_color(text_muted())
                    .child(i18n::t("profiles.conflict.body").into_owned()),
            )
            .child(
                div()
                    .flex()
                    .gap(SP_2)
                    .child(
                        primary_button(
                            "conflict-reload",
                            i18n::t("profiles.conflict.reload").into_owned(),
                            cx.listener(|t, _, _, cx| {
                                t.conflict = false;
                                t.refresh(cx);
                            }),
                        )
                        .disabled(self.loading),
                    )
                    .when(draft, |d| {
                        d.child(secondary_button(
                            "conflict-discard",
                            i18n::t("profiles.conflict.discard").into_owned(),
                            cx.listener(|t, _, _, cx| {
                                t.conflict = false;
                                t.editor = None;
                                t.refresh(cx);
                            }),
                        ))
                    })
                    .when(!draft, |d| {
                        d.child(secondary_button(
                            "conflict-cancel",
                            i18n::t("profiles.cancel").into_owned(),
                            cx.listener(|t, _, _, cx| {
                                t.conflict = false;
                                t.confirm_delete = None;
                                cx.notify();
                            }),
                        ))
                    }),
            )
    }

    fn list_card(&self, cx: &mut Context<Self>) -> Div {
        let Some(d) = &self.data else { return div() };
        let locked = self.read_only || self.saving || self.editor.is_some();
        let order = sorted_filtered(&d.profiles, "");
        let mut list = card();
        if let Some(name) = &self.confirm_delete {
            let n = name.clone();
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(SP_3)
                    .p(SP_2)
                    .rounded(R_SM)
                    .bg(raised())
                    .child(
                        div()
                            .flex_1()
                            .child(i18n::tf("profiles.list.confirm_delete", &[("name", name)])),
                    )
                    .child(secondary_button(
                        "delete-cancel",
                        i18n::t("profiles.cancel").into_owned(),
                        cx.listener(|t, _, _, cx| {
                            t.confirm_delete = None;
                            cx.notify();
                        }),
                    ))
                    .child(
                        primary_button(
                            "delete-confirm",
                            i18n::t("profiles.list.delete").into_owned(),
                            cx.listener(move |t, _, _, cx| t.submit(Op::Remove(n.clone()), cx)),
                        )
                        .icon(IconName::Delete)
                        .disabled(self.saving),
                    ),
            );
        }
        for i in order {
            let p = &d.profiles[i];
            let name = p.process.clone();
            let name_for_delete = name.clone();
            list = list.child(
                div()
                    .min_h(HIT)
                    .flex()
                    .items_center()
                    .gap(SP_3)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(TEXT_SM)
                                    .text_color(text_muted())
                                    .font_features(tabular())
                                    .child(summary(p, &d.layouts)),
                            ),
                    )
                    .child(if p.enabled {
                        badge(i18n::t("profiles.list.enabled").into_owned(), BadgeKind::Ok)
                    } else {
                        badge(
                            i18n::t("profiles.list.disabled").into_owned(),
                            BadgeKind::Neutral,
                        )
                    })
                    .child(
                        secondary_button(
                            SharedString::from(format!("toggle-{i}")),
                            if p.enabled {
                                i18n::t("profiles.list.disable").into_owned()
                            } else {
                                i18n::t("profiles.list.enable").into_owned()
                            },
                            cx.listener(move |t, _, _, cx| t.toggle(i, cx)),
                        )
                        .disabled(locked),
                    )
                    .child(
                        secondary_button(
                            SharedString::from(format!("edit-{i}")),
                            i18n::t("profiles.list.edit").into_owned(),
                            cx.listener(move |t, _, w, cx| {
                                let Some(p) =
                                    t.data.as_ref().and_then(|d| d.profiles.get(i)).cloned()
                                else {
                                    return;
                                };
                                t.open_editor(&FormInput::from_profile(&p), Some(p.process), w, cx);
                            }),
                        )
                        .icon(IconName::Settings)
                        .disabled(locked),
                    )
                    .child(
                        secondary_button(
                            SharedString::from(format!("delete-{i}")),
                            i18n::t("profiles.list.delete").into_owned(),
                            cx.listener(move |t, _, _, cx| {
                                t.confirm_delete = Some(name_for_delete.clone());
                                cx.notify();
                            }),
                        )
                        .icon(IconName::Delete)
                        .disabled(locked),
                    ),
            );
        }
        list
    }

    fn editor_card(&self, cx: &mut Context<Self>) -> Div {
        let (Some(e), Some(d), Some(form)) = (&self.editor, &self.data, self.form(cx)) else {
            return div();
        };
        let result = build_profile(&form, &d.profiles, e.original.as_deref());
        let errs: Vec<FieldError> = result.as_ref().err().cloned().unwrap_or_default();
        let err_of = |f: Field| {
            errs.iter()
                .find(|x| x.field == f)
                .map(|x| x.message.clone())
        };
        let has_island = e.island.is_some();
        let title = if e.original.is_some() {
            i18n::t("profiles.editor.title_edit")
        } else {
            i18n::t("profiles.editor.title_new")
        };

        let sugg = if e.original.is_none() {
            suggestions(&d.processes, &d.profiles, &form.process, 6)
                .into_iter()
                .filter(|s| !s.eq_ignore_ascii_case(form.process.trim()))
                .collect()
        } else {
            Vec::new()
        };
        let process_input = e.process.clone();
        let process_field = labeled(
            &i18n::t("profiles.editor.process"),
            Input::new(&e.process).disabled(e.original.is_some()),
            err_of(Field::Process).as_deref(),
        )
        .when(!sugg.is_empty(), |f| {
            f.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_2)
                    .child(
                        div()
                            .text_size(TEXT_SM)
                            .text_color(text_muted())
                            .child(i18n::t("profiles.editor.open_now").into_owned()),
                    )
                    .children(sugg.into_iter().enumerate().map(|(i, s)| {
                        let input = process_input.clone();
                        let v = s.clone();
                        chip(
                            SharedString::from(format!("sugg-{i}")),
                            s,
                            false,
                            false,
                            move |_, w, cx| {
                                input.update(cx, |st, cx| st.set_value(v.clone(), w, cx));
                            },
                        )
                    })),
            )
        });

        let islands = island_options(&d.layouts, &d.islands, e.island.as_deref());
        let current_shown = e.island.as_deref().map(|c| shown_ref(&d.layouts, c));
        // A legacy ref stays valid; say which canonical island it stands for.
        let alias_hint = e.island.as_deref().and_then(|c| {
            legacy_target(&d.layouts, c).map(|canon| {
                format!(
                    "{c} {}",
                    i18n::tf(
                        "profiles.editor.island_alias_hint",
                        &[("canonical", &canon)]
                    )
                )
            })
        });
        let island_row = labeled(
            &i18n::t("profiles.editor.island"),
            div()
                .flex()
                .flex_wrap()
                .gap(SP_2)
                .child(chip(
                    "island-none",
                    i18n::t("profiles.editor.no_island").into_owned(),
                    !has_island,
                    false,
                    cx.listener(|t, _, _, cx| {
                        if let Some(e) = &mut t.editor {
                            e.island = None;
                        }
                        cx.notify();
                    }),
                ))
                .children(islands.into_iter().enumerate().map(|(i, name)| {
                    let sel = current_shown.as_deref() == Some(name.as_str());
                    let n = name.clone();
                    chip(
                        SharedString::from(format!("island-{i}")),
                        name,
                        sel,
                        false,
                        cx.listener(move |t, _, _, cx| {
                            if let Some(e) = &mut t.editor {
                                e.island = Some(n.clone());
                            }
                            cx.notify();
                        }),
                    )
                })),
            None,
        )
        .when_some(alias_hint, |row, hint| {
            row.child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(hint),
            )
        });

        let mut monitors: Vec<(MonitorChoice, String)> = vec![(
            MonitorChoice::Current,
            i18n::t("profiles.editor.monitor_current").into_owned(),
        )];
        monitors.extend(
            d.monitors
                .iter()
                .map(|(i, dev)| (MonitorChoice::Index(*i), format!("{i} · {dev}"))),
        );
        if let MonitorChoice::Index(n) = e.monitor
            && !monitors.iter().any(|(m, _)| *m == MonitorChoice::Index(n))
        {
            monitors.push((
                MonitorChoice::Index(n),
                i18n::tf("profiles.editor.monitor_unavailable", &[("n", &n)]),
            ));
        }
        let monitor_row = labeled(
            &i18n::t("profiles.editor.monitor"),
            div()
                .flex()
                .flex_wrap()
                .gap(SP_2)
                .children(monitors.into_iter().enumerate().map(|(i, (m, label))| {
                    chip(
                        SharedString::from(format!("monitor-{i}")),
                        label,
                        e.monitor == m && !has_island,
                        has_island,
                        cx.listener(move |t, _, _, cx| {
                            if let Some(e) = &mut t.editor {
                                e.monitor = m;
                            }
                            cx.notify();
                        }),
                    )
                })),
            None,
        );

        let num = |label: &str, st: &Entity<InputState>, f: Field, off: bool| {
            labeled(label, Input::new(st).disabled(off), err_of(f).as_deref())
                .w(px(130.))
                .when(off, |d| d.opacity(0.5))
        };
        let offs = div().flex().flex_wrap().gap(SP_3).children(
            [
                (i18n::t("profiles.editor.offset_left"), Field::Left),
                (i18n::t("profiles.editor.offset_top"), Field::Top),
                (i18n::t("profiles.editor.offset_right"), Field::Right),
                (i18n::t("profiles.editor.offset_bottom"), Field::Bottom),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (l, f))| num(&l, &e.offsets[i], f, has_island)),
        );
        let can_save = result.is_ok() && !self.saving && !self.read_only;
        let enabled_now = e.enabled;

        card()
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.into_owned()),
            )
            .child(process_field)
            .child(island_row)
            .child(monitor_row)
            .child(labeled(&i18n::t("profiles.editor.offsets"), offs, None))
            .when(has_island, |c| {
                c.child(
                    div()
                        .text_size(TEXT_SM)
                        .text_color(warn())
                        .child(i18n::t("profiles.editor.island_overrides").into_owned()),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_3)
                    .child(num(
                        &i18n::t("profiles.editor.delay"),
                        &e.delay,
                        Field::Delay,
                        false,
                    ))
                    .child(num(
                        &i18n::t("profiles.editor.width"),
                        &e.width,
                        Field::Width,
                        false,
                    ))
                    .child(num(
                        &i18n::t("profiles.editor.height"),
                        &e.height,
                        Field::Height,
                        false,
                    )),
            )
            .child(
                div()
                    .text_size(TEXT_SM)
                    .text_color(text_muted())
                    .child(i18n::t("profiles.editor.resolution_help").into_owned()),
            )
            .child(chip(
                "enabled",
                if enabled_now {
                    i18n::t("profiles.editor.enabled_yes").into_owned()
                } else {
                    i18n::t("profiles.editor.enabled_no").into_owned()
                },
                enabled_now,
                false,
                cx.listener(|t, _, _, cx| {
                    if let Some(e) = &mut t.editor {
                        e.enabled = !e.enabled;
                    }
                    cx.notify();
                }),
            ))
            .child(
                div()
                    .flex()
                    .gap(SP_2)
                    .mt(SP_6 - SP_4)
                    .child(
                        primary_button(
                            "save-profile",
                            i18n::t("profiles.editor.save").into_owned(),
                            cx.listener(|t, _, _, cx| t.save_editor(cx)),
                        )
                        .icon(IconName::Check)
                        .disabled(!can_save),
                    )
                    .child(
                        secondary_button(
                            "cancel-profile",
                            i18n::t("profiles.cancel").into_owned(),
                            cx.listener(|t, _, _, cx| {
                                t.editor = None;
                                t.save_error = None;
                                cx.notify();
                            }),
                        )
                        .disabled(self.saving),
                    ),
            )
    }
}

// ======================================================================== tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::ErrorBody;

    fn prof(name: &str) -> Profile {
        Profile {
            process: name.into(),
            enabled: true,
            monitor: MonitorChoice::Current,
            offsets: Offsets::default(),
            delay_ms: 0,
            clip_cursor: false,
            island: None,
            game_resolution: None,
            extra: Default::default(),
        }
    }

    fn form(name: &str) -> FormInput {
        FormInput {
            process: name.into(),
            ..FormInput::blank()
        }
    }

    fn fields(r: Result<Profile, Vec<FieldError>>) -> Vec<Field> {
        r.unwrap_err().into_iter().map(|e| e.field).collect()
    }

    #[test]
    fn outside_change_refills_a_profile_changed_outside() {
        let mut old = prof("game.exe");
        old.delay_ms = 0;
        let mut new = old.clone();
        new.delay_ms = 3000;
        let others = prof("other.exe");
        let got = outside_change(
            Some("GAME.exe"),
            &[others.clone(), old],
            &[others, new.clone()],
        );
        assert_eq!(got, OutsideChange::Changed(new));
    }

    #[test]
    fn outside_change_flags_a_profile_removed_outside() {
        let got = outside_change(Some("game.exe"), &[prof("game.exe")], &[prof("other.exe")]);
        assert_eq!(got, OutsideChange::Removed);
    }

    #[test]
    fn outside_change_ignores_unchanged_or_new_profiles() {
        let mut other = prof("other.exe");
        let same = [prof("game.exe"), other.clone()];
        other.delay_ms = 5;
        let changed_other = [prof("game.exe"), other];
        assert_eq!(
            outside_change(Some("game.exe"), &same, &changed_other),
            OutsideChange::None
        );
        assert_eq!(
            outside_change(None, &same, &changed_other),
            OutsideChange::None
        );
    }

    #[test]
    fn blank_form_with_process_is_valid_defaults() {
        let p = build_profile(&form("  game.exe "), &[], None).unwrap();
        assert_eq!(p, prof("game.exe"));
    }

    #[test]
    fn empty_and_excluded_process_rejected() {
        assert_eq!(
            fields(build_profile(&form("  "), &[], None)),
            [Field::Process]
        );
        assert_eq!(
            fields(build_profile(&form("Explorer.EXE"), &[], None)),
            [Field::Process]
        );
    }

    #[test]
    fn duplicate_process_rejected_unless_it_is_the_one_being_edited() {
        let existing = [prof("Game.exe")];
        assert_eq!(
            fields(build_profile(&form("game.EXE"), &existing, None)),
            [Field::Process]
        );
        assert_eq!(
            fields(build_profile(&form("c:\\x\\game.exe"), &existing, None)),
            [Field::Process]
        );
        assert!(build_profile(&form("game.exe"), &existing, Some("Game.exe")).is_ok());
        // renaming onto another existing profile is still a duplicate
        let two = [prof("a.exe"), prof("b.exe")];
        assert!(build_profile(&form("b.exe"), &two, Some("a.exe")).is_err());
    }

    #[test]
    fn delay_range_is_inclusive_0_to_60000() {
        let mut f = form("g.exe");
        for (v, ok) in [
            ("0", true),
            ("60000", true),
            ("60001", false),
            ("-1", false),
            ("abc", false),
            ("1.5", false),
            ("", true),
        ] {
            f.delay = v.into();
            assert_eq!(build_profile(&f, &[], None).is_ok(), ok, "delay {v:?}");
        }
        f.delay = "60000".into();
        assert_eq!(build_profile(&f, &[], None).unwrap().delay_ms, 60_000);
    }

    #[test]
    fn offset_range_is_plus_minus_one_million_per_side() {
        for (i, field) in [Field::Left, Field::Top, Field::Right, Field::Bottom]
            .into_iter()
            .enumerate()
        {
            for (v, ok) in [
                ("1000000", true),
                ("-1000000", true),
                ("1000001", false),
                ("-1000001", false),
                ("99999999999", false),
            ] {
                let mut f = form("g.exe");
                f.offsets[i] = v.into();
                let r = build_profile(&f, &[], None);
                assert_eq!(r.is_ok(), ok, "{field:?} {v}");
                if !ok {
                    assert_eq!(fields(r), [field]);
                }
            }
        }
        let mut f = form("g.exe");
        f.offsets = ["-8".into(), "0".into(), "16".into(), "1".into()];
        assert_eq!(
            build_profile(&f, &[], None).unwrap().offsets,
            Offsets {
                left: -8,
                top: 0,
                right: 16,
                bottom: 1
            }
        );
    }

    #[test]
    fn game_resolution_is_all_or_nothing_within_1_to_1e6() {
        let mut f = form("g.exe");
        assert_eq!(build_profile(&f, &[], None).unwrap().game_resolution, None);
        f.width = "1280".into();
        assert_eq!(fields(build_profile(&f, &[], None)), [Field::Height]);
        f.height = "720".into();
        assert_eq!(
            build_profile(&f, &[], None).unwrap().game_resolution,
            Some(Size {
                width: 1280,
                height: 720
            })
        );
        for (w, h, bad) in [
            ("0", "720", vec![Field::Width]),
            ("1280", "1000001", vec![Field::Height]),
            ("0", "0", vec![Field::Width, Field::Height]),
            ("x", "1", vec![Field::Width]),
        ] {
            f.width = w.into();
            f.height = h.into();
            assert_eq!(fields(build_profile(&f, &[], None)), bad, "{w}x{h}");
        }
        f.width = "1000000".into();
        f.height = "1".into();
        assert!(build_profile(&f, &[], None).is_ok());
    }

    #[test]
    fn island_clears_monitor_and_offsets_and_skips_their_validation() {
        let mut f = form("g.exe");
        f.island = Some("halves-16-9/right".into());
        f.monitor = MonitorChoice::Index(1);
        f.offsets = ["junk".into(), "5".into(), "5".into(), "5".into()];
        let p = build_profile(&f, &[], None).unwrap();
        assert_eq!(p.island.as_deref(), Some("halves-16-9/right"));
        assert_eq!(p.monitor, MonitorChoice::Current);
        assert_eq!(p.offsets, Offsets::default());
        // without island the same garbage is an error and the monitor is kept
        f.island = None;
        assert_eq!(fields(build_profile(&f, &[], None)), [Field::Left]);
        f.offsets = std::array::from_fn(|_| "0".into());
        assert_eq!(
            build_profile(&f, &[], None).unwrap().monitor,
            MonitorChoice::Index(1)
        );
    }

    #[test]
    fn all_errors_are_reported_together() {
        let mut f = form("");
        f.delay = "99999".into();
        f.offsets[2] = "x".into();
        f.width = "5".into();
        assert_eq!(
            fields(build_profile(&f, &[], None)),
            [Field::Process, Field::Right, Field::Delay, Field::Height]
        );
    }

    #[test]
    fn form_roundtrip_through_profile() {
        let mut p = prof("Game.exe");
        p.enabled = false;
        p.monitor = MonitorChoice::Index(2);
        p.offsets = Offsets {
            left: -1,
            top: 2,
            right: 3,
            bottom: -4,
        };
        p.delay_ms = 1500;
        p.game_resolution = Some(Size {
            width: 1280,
            height: 720,
        });
        assert_eq!(
            build_profile(&FormInput::from_profile(&p), &[p.clone()], Some("Game.exe")).unwrap(),
            p
        );
    }

    #[test]
    fn sort_is_case_insensitive_and_filter_matches_process_or_island() {
        let mut c = prof("zeta.exe");
        c.island = Some("halves/left".into());
        let ps = [prof("Beta.exe"), c, prof("alpha.exe")];
        assert_eq!(sorted_filtered(&ps, ""), [2, 0, 1]);
        assert_eq!(sorted_filtered(&ps, " BET "), [0]);
        assert_eq!(sorted_filtered(&ps, "left"), [1]);
        assert!(sorted_filtered(&ps, "nope").is_empty());
    }

    #[test]
    fn summary_describes_target() {
        let _g = i18n::scoped_locale(Locale::En);
        let layouts = merged_layouts(&[]);
        let mut p = prof("a.exe");
        assert_eq!(summary(&p, &layouts), "Current monitor");
        p.monitor = MonitorChoice::Index(1);
        p.offsets.left = -8;
        p.delay_ms = 500;
        p.game_resolution = Some(Size {
            width: 1280,
            height: 720,
        });
        assert_eq!(
            summary(&p, &layouts),
            "Monitor 1 · offsets -8, 0, 0, 0 · delay 500 ms · game 1280x720"
        );
        p.island = Some("l/i".into());
        assert_eq!(
            summary(&p, &layouts),
            "Island l/i · delay 500 ms · game 1280x720"
        );
    }

    #[test]
    fn a_legacy_island_ref_is_valid_and_shows_its_canonical_form() {
        let _g = i18n::scoped_locale(Locale::En);
        let layouts = merged_layouts(&[]);
        let legacy = "mitades-16-9/derecha"; // legacy alias test
        assert_eq!(
            legacy_target(&layouts, legacy).as_deref(),
            Some("halves-16-9/right")
        );
        assert_eq!(legacy_target(&layouts, "halves-16-9/right"), None);
        assert_eq!(legacy_target(&layouts, "gone/x"), None);
        let mut p = prof("a.exe");
        p.island = Some(legacy.into());
        assert_eq!(
            summary(&p, &layouts),
            "Island mitades-16-9/derecha (= halves-16-9/right)" // legacy alias test
        );
        // the form accepts it unchanged: nothing is rewritten silently
        let mut f = form("a.exe");
        f.island = Some(legacy.into());
        assert_eq!(
            build_profile(&f, &[], None).unwrap().island.as_deref(),
            Some(legacy)
        );
    }

    #[test]
    fn island_options_lists_canonical_ids_and_keeps_a_vanished_current_island() {
        let layouts = merged_layouts(&[]);
        let known = vec![
            "halves-16-9/left".to_owned(),
            "halves-16-9/right".to_owned(),
        ];
        assert_eq!(
            island_options(&layouts, &known, Some("halves-16-9/right")),
            known
        );
        // a legacy ref is shown as the canonical island, never added as a second choice
        let legacy = Some("mitades-16-9/izquierda"); // legacy alias test
        assert_eq!(island_options(&layouts, &known, legacy), known);
        assert_eq!(
            island_options(&layouts, &known, Some("gone/y")),
            ["halves-16-9/left", "halves-16-9/right", "gone/y"]
        );
        assert_eq!(island_options(&layouts, &known, None), known);
    }

    #[test]
    fn suggestions_dedupe_filter_and_exclude() {
        let wins: Vec<String> = [
            "Game.exe",
            "game.exe",
            "explorer.exe",
            "Zed.exe",
            "has.exe",
            "",
            "C:\\x\\Gamer.exe",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let profiles = [prof("has.exe")];
        assert_eq!(
            suggestions(&wins, &profiles, "", 10),
            ["Game.exe", "Gamer.exe", "Zed.exe"]
        );
        assert_eq!(
            suggestions(&wins, &profiles, "GAME", 10),
            ["Game.exe", "Gamer.exe"]
        );
        assert_eq!(suggestions(&wins, &profiles, "", 1), ["Game.exe"]);
    }

    fn w(code: &str, data: Value) -> Warning {
        Warning {
            code: code.into(),
            data,
        }
    }

    #[test]
    fn warnings_map_to_text_with_context() {
        let _g = i18n::scoped_locale(Locale::En);
        let t = warning_text(&w(
            "island_not_found",
            json!({"process": "g.exe", "island": "a/b"}),
        ));
        assert_eq!(t, "g.exe: the island \"a/b\" does not exist in any layout.");
        let t = warning_text(&w(
            "duplicate_island",
            json!({"layout": "L", "island": "I"}),
        ));
        assert!(t.contains("\"I\"") && t.contains("\"L\""), "{t}");
        assert!(
            warning_text(&w("island_overrides_monitor", json!({"process": "g.exe"})))
                .starts_with("g.exe:")
        );
        assert_eq!(
            warning_text(&w("something_new", json!({}))),
            "Engine warning: something_new"
        );
        // missing context never panics
        assert!(warning_text(&w("bad_island_ref", json!(null))).contains('?'));
    }

    #[test]
    fn remote_message_prefers_rejected_warnings() {
        let _g = i18n::scoped_locale(Locale::En);
        let e = ErrorBody::new(ErrorCode::ProfileError, "profiles rejected by validation")
            .with_data(
                json!({"warnings": [{"code": "bad_game_resolution", "data": {"process": "g.exe"}}]}),
            );
        assert_eq!(
            remote_message(&e),
            "g.exe: the game resolution must be greater than 0."
        );
        // without warnings the sentence for the code is shown, never the engine's raw text
        let plain = ErrorBody::new(ErrorCode::ProfileError, "invalid JSON: x");
        assert_eq!(
            remote_message(&plain),
            i18n::error_sentence(ErrorCode::ProfileError)
        );
    }
}
