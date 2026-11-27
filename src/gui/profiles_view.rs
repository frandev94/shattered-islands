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

/// Engine ranges (`MAX_COORD` in `engine.rs`): kept here so the form rejects what the engine would.
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
    Enabled,
    Island,
    Monitor,
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
    /// It changed outside: merge it into the form (see [`after_reload`]).
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

/// Result of [`merge_form`]: the form to show and the fields changed on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub form: FormInput,
    pub conflicts: Vec<Field>,
}

/// Every field a merge compares, in form order.
const MERGE_FIELDS: [Field; 11] = [
    Field::Process,
    Field::Enabled,
    Field::Island,
    Field::Monitor,
    Field::Left,
    Field::Top,
    Field::Right,
    Field::Bottom,
    Field::Delay,
    Field::Width,
    Field::Height,
];

/// The text of a text field (`None` for `enabled`, `island` and `monitor`).
fn text_of(f: &FormInput, field: Field) -> Option<&str> {
    Some(match field {
        Field::Process => &f.process,
        Field::Left => &f.offsets[0],
        Field::Top => &f.offsets[1],
        Field::Right => &f.offsets[2],
        Field::Bottom => &f.offsets[3],
        Field::Delay => &f.delay,
        Field::Width => &f.width,
        Field::Height => &f.height,
        Field::Enabled | Field::Island | Field::Monitor => return None,
    })
}

/// Whether `field` holds the same value in both forms (text compared with `trim()`).
fn same_field(a: &FormInput, b: &FormInput, field: Field) -> bool {
    match field {
        Field::Enabled => a.enabled == b.enabled,
        Field::Island => a.island.as_deref().map(str::trim) == b.island.as_deref().map(str::trim),
        Field::Monitor => a.monitor == b.monitor,
        _ => text_of(a, field).map(str::trim) == text_of(b, field).map(str::trim),
    }
}

/// Copies `field` from `src` into `dst`.
fn copy_field(dst: &mut FormInput, src: &FormInput, field: Field) {
    match field {
        Field::Enabled => dst.enabled = src.enabled,
        Field::Island => dst.island.clone_from(&src.island),
        Field::Monitor => dst.monitor = src.monitor,
        Field::Process => dst.process.clone_from(&src.process),
        Field::Left => dst.offsets[0].clone_from(&src.offsets[0]),
        Field::Top => dst.offsets[1].clone_from(&src.offsets[1]),
        Field::Right => dst.offsets[2].clone_from(&src.offsets[2]),
        Field::Bottom => dst.offsets[3].clone_from(&src.offsets[3]),
        Field::Delay => dst.delay.clone_from(&src.delay),
        Field::Width => dst.width.clone_from(&src.width),
        Field::Height => dst.height.clone_from(&src.height),
    }
}

/// Fields the form ignores while an island is set (`build_profile`).
const ISLAND_COUPLED: [Field; 5] = [
    Field::Monitor,
    Field::Left,
    Field::Top,
    Field::Right,
    Field::Bottom,
];

/// Three-way merge per field of the typed form (`mine`) with the reloaded profile (`theirs`)
/// over the saved version it was opened on (`base`). A field changed on both sides to different
/// values is a conflict and keeps `mine`; `pending` conflicts from an earlier merge stay
/// conflicts unless both sides now agree. Fields the profile couples are merged together: an
/// island changed on one side clashes with offsets or monitor changed on the other, and
/// width/height are one resolution, so no typed value is lost silently.
pub fn merge_form(
    base: &FormInput,
    mine: &FormInput,
    theirs: &FormInput,
    pending: &[Field],
) -> Merged {
    let mut form = mine.clone();
    let mut marked = Vec::new();
    for field in MERGE_FIELDS {
        if same_field(mine, theirs, field) {
            continue;
        }
        if pending.contains(&field) {
            marked.push(field);
        } else if same_field(mine, base, field) {
            copy_field(&mut form, theirs, field);
        } else if !same_field(theirs, base, field) {
            marked.push(field);
        }
    }
    let changed = |side: &FormInput, field| !same_field(side, base, field);
    let differs = |field| !same_field(mine, theirs, field);
    if differs(Field::Island) {
        for (island_side, other) in [(mine, theirs), (theirs, mine)] {
            if !changed(island_side, Field::Island) {
                continue;
            }
            let hit: Vec<Field> = ISLAND_COUPLED
                .into_iter()
                .filter(|&f| changed(other, f) && differs(f))
                .collect();
            if !hit.is_empty() {
                marked.push(Field::Island);
                marked.extend(hit);
            }
        }
    }
    let pair_changed = |side| changed(side, Field::Width) || changed(side, Field::Height);
    if pair_changed(mine)
        && pair_changed(theirs)
        && (differs(Field::Width) || differs(Field::Height))
    {
        marked.extend([Field::Width, Field::Height]);
    }
    let conflicts: Vec<Field> = MERGE_FIELDS
        .into_iter()
        .filter(|f| marked.contains(f))
        .collect();
    for &field in &conflicts {
        copy_field(&mut form, mine, field);
    }
    Merged { form, conflicts }
}

/// What the open editor does after a reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// Leave the form as typed.
    Keep,
    /// Reopen the form with this merge.
    Merge(Box<Merged>),
    /// The profile is gone outside: forget the base and the conflicts.
    Removed,
}

/// Decides what a reload does to the form `mine`. Without a `base` (cannot happen for an edit)
/// every field that differs is a conflict that keeps `mine`: nothing is taken silently.
pub fn after_reload(
    base: Option<&FormInput>,
    mine: &FormInput,
    pending: &[Field],
    change: &OutsideChange,
) -> ReloadOutcome {
    match change {
        OutsideChange::None => ReloadOutcome::Keep,
        OutsideChange::Removed => ReloadOutcome::Removed,
        OutsideChange::Changed(p) => {
            let theirs = FormInput::from_profile(p);
            ReloadOutcome::Merge(Box::new(match base {
                Some(b) => merge_form(b, mine, &theirs, pending),
                None => merge_form(mine, mine, &theirs, &MERGE_FIELDS),
            }))
        }
    }
}

/// `form` with the given `fields` replaced by the ones in `theirs`.
pub fn take_fields(form: &FormInput, theirs: &FormInput, fields: &[Field]) -> FormInput {
    let mut out = form.clone();
    for &field in fields {
        copy_field(&mut out, theirs, field);
    }
    out
}

/// The outside value of `field`, readable in the active language. An island shows the ref the
/// pickers show (canonical in `layouts`).
pub fn outside_value(theirs: &FormInput, field: Field, layouts: &[Layout]) -> String {
    match field {
        Field::Enabled if theirs.enabled => i18n::t("profiles.editor.enabled_yes").into_owned(),
        Field::Enabled => i18n::t("profiles.editor.enabled_no").into_owned(),
        Field::Island => match theirs.island.as_deref().map(str::trim) {
            Some(i) if !i.is_empty() => shown_ref(layouts, i),
            _ => i18n::t("profiles.editor.no_island").into_owned(),
        },
        Field::Monitor => match theirs.monitor {
            MonitorChoice::Current => i18n::t("profiles.editor.monitor_current").into_owned(),
            MonitorChoice::Index(n) => i18n::tf("profiles.summary.monitor", &[("n", &n)]),
        },
        _ => match text_of(theirs, field).map(str::trim) {
            Some(t) if !t.is_empty() => t.to_owned(),
            _ => i18n::t("profiles.outside.empty").into_owned(),
        },
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

/// A notice kept as its i18n key and process name, so it is translated when shown.
struct Notice {
    key: &'static str,
    process: String,
}

impl Notice {
    fn new(key: &'static str, process: &str) -> Self {
        Self {
            key,
            process: process.to_owned(),
        }
    }

    fn text(&self) -> String {
        i18n::tf(self.key, &[("process", &self.process)])
    }
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
    /// The saved version the form is based on (None = new profile): the merge base.
    base: Option<FormInput>,
    /// Fields changed here and outside to different values; `base` holds the outside ones.
    conflicts: Vec<Field>,
    _subs: Vec<Subscription>,
}

impl Editor {
    /// The text input of `field` (`None` for the chip fields).
    fn input(&self, field: Field) -> Option<Entity<InputState>> {
        Some(
            match field {
                Field::Process => &self.process,
                Field::Left => &self.offsets[0],
                Field::Top => &self.offsets[1],
                Field::Right => &self.offsets[2],
                Field::Bottom => &self.offsets[3],
                Field::Delay => &self.delay,
                Field::Width => &self.width,
                Field::Height => &self.height,
                Field::Enabled | Field::Island | Field::Monitor => return None,
            }
            .clone(),
        )
    }
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
    notice: Option<Notice>,
    save_error: Option<String>,
    /// Warnings of the last `set_profiles` answer (replace the file's while visible).
    save_warnings: Vec<Warning>,
    conflict: bool,
    /// The edited profile changed outside: reopen the form with this merge at the next render
    /// (it needs the window).
    refill: Option<Merged>,
    /// Why the open form no longer matches what it was opened with (shown while editing).
    outside: Option<Notice>,
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
            this.update(cx, |t, cx| t.apply_load(r, cx)).ok();
        })
        .detach();
    }

    /// A finished `load`: keep its data, and never keep stale form values silently over an
    /// outside change.
    fn apply_load(&mut self, r: Result<Reply<Data>, String>, cx: &mut Context<Self>) {
        self.loading = false;
        match r {
            Ok(Reply::Ok(d)) => {
                self.error = None;
                self.read_only = false;
                let outcome = match (&self.editor, &self.data, self.form(cx)) {
                    (Some(e), Some(old), Some(mine)) => {
                        let change =
                            outside_change(e.original.as_deref(), &old.profiles, &d.profiles);
                        after_reload(e.base.as_ref(), &mine, &e.conflicts, &change)
                    }
                    _ => ReloadOutcome::Keep,
                };
                match outcome {
                    ReloadOutcome::Keep => {}
                    ReloadOutcome::Merge(m) => self.refill = Some(*m),
                    ReloadOutcome::Removed => {
                        if let Some(e) = &mut self.editor {
                            e.base = None;
                            e.conflicts.clear();
                            self.outside = e
                                .original
                                .take()
                                .map(|o| Notice::new("profiles.outside.removed", &o));
                        }
                    }
                }
                self.data = Some(d);
            }
            Ok(Reply::Remote(b)) => {
                self.read_only = b.code == ErrorCode::UnsupportedVersion;
                self.error = Some(format!("{} ({})", remote_message(&b), code_str(b.code)));
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
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
        let base = original.as_deref().and_then(|o| {
            let d = self.data.as_ref()?;
            let p = d.profiles.iter().find(|p| same_process(&p.process, o))?;
            Some(FormInput::from_profile(p))
        });
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
            base,
            conflicts: Vec::new(),
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
            Op::Upsert(p) => Notice::new("profiles.saved", &p.process),
            Op::Remove(n) => Notice::new("profiles.removed", n),
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
        let (Some(form), Some(d), Some(e)) = (self.form(cx), self.data.as_ref(), &self.editor)
        else {
            return;
        };
        // A merge is waiting to reopen the form: this form is stale, saving it would skip the
        // marked conflicts.
        if self.refill.is_some() {
            return;
        }
        // Pending conflicts are resolved only by the banner buttons, never by a plain save.
        if !e.conflicts.is_empty() {
            return;
        }
        if let Ok(p) = build_profile(&form, &d.profiles, e.original.as_deref()) {
            self.submit(Op::Upsert(p), cx);
        }
    }

    /// "Use the outside values": the conflicting fields take the outside version; nothing is
    /// saved yet.
    fn use_outside(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(e), Some(form)) = (&self.editor, self.form(cx)) else {
            return;
        };
        let Some(base) = &e.base else { return };
        let next = take_fields(&form, base, &e.conflicts);
        let fields = e.conflicts.clone();
        let original = e.original.clone();
        let outside = self.outside.take();
        self.open_editor(&next, original, window, cx);
        self.outside = outside;
        // The clicked button is gone: keep keyboard focus in the form, on the first taken field.
        if let Some(input) = fields.iter().find_map(|&f| self.editor.as_ref()?.input(f)) {
            input.update(cx, |s, cx| s.focus(window, cx));
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

/// Under a field changed here and outside: its outside value, in the field error's slot.
fn with_outside(field: Div, theirs: Option<String>) -> Div {
    field.when_some(theirs, |d, v| {
        d.child(
            div()
                .text_size(TEXT_SM)
                .text_color(warn())
                .child(i18n::tf("profiles.outside.theirs", &[("value", &v)])),
        )
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
        if let Some(m) = self.refill.take() {
            let original = self.editor.as_ref().and_then(|e| e.original.clone());
            if let Some(process) = original.clone() {
                self.open_editor(&m.form, original, window, cx);
                if let Some(e) = &mut self.editor {
                    e.conflicts = m.conflicts;
                }
                self.outside = Some(Notice::new("profiles.outside.merged", &process));
            }
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
            body = body.child(banner(n.text(), BadgeKind::Ok));
        }
        if let Some(n) = self.outside.as_ref().filter(|_| self.editor.is_some()) {
            body = body.child(banner(n.text(), BadgeKind::Warn));
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|e| !e.conflicts.is_empty())
        {
            body = body.child(self.merge_card(cx));
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

    /// Fields changed here and outside: say so and offer the two ways out.
    fn merge_card(&self, cx: &mut Context<Self>) -> Div {
        let valid = match (&self.editor, &self.data, self.form(cx)) {
            (Some(e), Some(d), Some(f)) => {
                build_profile(&f, &d.profiles, e.original.as_deref()).is_ok()
            }
            _ => false,
        };
        let idle = !self.saving && !self.loading && !self.read_only;
        card()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(warn())
                    .child(i18n::t("profiles.outside.conflicts").into_owned()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(SP_2)
                    .child(
                        primary_button(
                            "merge-keep-mine",
                            i18n::t("profiles.outside.keep_mine").into_owned(),
                            cx.listener(|t, _, _, cx| {
                                if let Some(e) = &mut t.editor {
                                    e.conflicts.clear();
                                }
                                t.save_editor(cx);
                            }),
                        )
                        // Disabled while invalid: clearing the marks without saving would lie.
                        .disabled(!(valid && idle)),
                    )
                    .child(
                        secondary_button(
                            "merge-use-theirs",
                            i18n::t("profiles.outside.use_theirs").into_owned(),
                            cx.listener(|t, _, w, cx| t.use_outside(w, cx)),
                        )
                        .disabled(!idle),
                    ),
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
        let theirs_of = |f: Field| {
            e.base
                .as_ref()
                .filter(|_| e.conflicts.contains(&f))
                .map(|b| outside_value(b, f, &d.layouts))
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
        let island_row = with_outside(island_row, theirs_of(Field::Island));

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
        let monitor_row = with_outside(monitor_row, theirs_of(Field::Monitor));

        let num = |label: &str, st: &Entity<InputState>, f: Field, off: bool| {
            with_outside(
                labeled(label, Input::new(st).disabled(off), err_of(f).as_deref()),
                theirs_of(f),
            )
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
        let can_save = result.is_ok() && e.conflicts.is_empty() && !self.saving && !self.read_only;
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
            .child(with_outside(
                div().flex().flex_col().gap(SP_1).child(chip(
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
                )),
                theirs_of(Field::Enabled),
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
    fn outside_notice_is_translated_when_shown() {
        let notice = Notice::new("profiles.outside.merged", "game.exe");
        let en = {
            let _g = i18n::scoped_locale(Locale::En);
            notice.text()
        };
        let es = {
            let _g = i18n::scoped_locale(Locale::Es);
            notice.text()
        };
        assert!(en.starts_with("game.exe was changed outside"), "{en}");
        assert_ne!(en, es);
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

    // ------------------------------------------------------- three-way merge

    fn with(f: impl FnOnce(&mut FormInput)) -> FormInput {
        let mut x = form("g.exe");
        f(&mut x);
        x
    }

    #[test]
    fn merge_takes_outside_only_changes() {
        let base = form("g.exe");
        let theirs = with(|f| {
            f.offsets[1] = "7".into();
            f.enabled = false;
            f.island = Some("l/i".into());
            f.monitor = MonitorChoice::Index(1);
            f.width = "1280".into();
        });
        let got = merge_form(&base, &base, &theirs, &[]);
        assert_eq!(
            got,
            Merged {
                form: theirs,
                conflicts: vec![]
            }
        );
    }

    #[test]
    fn merge_keeps_local_only_changes() {
        let base = form("g.exe");
        let mine = with(|f| {
            f.offsets[0] = "5".into();
            f.delay = "200".into();
            f.enabled = false;
        });
        let theirs = with(|f| f.offsets[1] = "7".into());
        let got = merge_form(&base, &mine, &theirs, &[]);
        let want = with(|f| {
            f.offsets = ["5".into(), "7".into(), "0".into(), "0".into()];
            f.delay = "200".into();
            f.enabled = false;
        });
        assert_eq!(
            got,
            Merged {
                form: want,
                conflicts: vec![]
            }
        );
    }

    #[test]
    fn merge_flags_fields_changed_on_both_sides() {
        let base = form("g.exe");
        let mine = with(|f| {
            f.island = Some("a/x".into());
            f.monitor = MonitorChoice::Index(1);
            f.offsets = ["1".into(), "2".into(), "3".into(), "4".into()];
            f.delay = "10".into();
            f.width = "100".into();
            f.height = "200".into();
        });
        let theirs = with(|f| {
            f.island = Some("b/y".into());
            f.monitor = MonitorChoice::Index(2);
            f.offsets = ["9".into(), "8".into(), "7".into(), "6".into()];
            f.delay = "20".into();
            f.width = "300".into();
            f.height = "400".into();
        });
        let got = merge_form(&base, &mine, &theirs, &[]);
        assert_eq!(got.form, mine);
        assert_eq!(
            got.conflicts,
            [
                Field::Island,
                Field::Monitor,
                Field::Left,
                Field::Top,
                Field::Right,
                Field::Bottom,
                Field::Delay,
                Field::Width,
                Field::Height,
            ]
        );
        // A bool cannot clash three ways; `enabled` conflicts only as a pending one.
        let off = with(|f| f.enabled = false);
        let got = merge_form(&base, &off, &base, &[Field::Enabled]);
        assert_eq!(
            got,
            Merged {
                form: off,
                conflicts: vec![Field::Enabled]
            }
        );
    }

    /// The form with every text field trimmed: the merge may keep either padding of equal values.
    fn trimmed(f: &FormInput) -> FormInput {
        FormInput {
            island: f.island.as_deref().map(|s| s.trim().to_owned()),
            offsets: f.offsets.clone().map(|s| s.trim().to_owned()),
            delay: f.delay.trim().into(),
            width: f.width.trim().into(),
            height: f.height.trim().into(),
            ..f.clone()
        }
    }

    #[test]
    fn merge_same_value_on_both_sides_is_no_conflict() {
        let base = form("g.exe");
        let both = with(|f| {
            f.delay = " 50 ".into();
            f.island = Some("l/i".into());
        });
        let theirs = with(|f| {
            f.delay = "50".into();
            f.island = Some("l/i".into());
        });
        let got = merge_form(&base, &both, &theirs, &[]);
        assert!(got.conflicts.is_empty());
        assert_eq!(got.form.delay.trim(), "50");
        assert_eq!(got.form.island.as_deref(), Some("l/i"));
    }

    #[test]
    fn merge_keeps_pending_conflicts_across_reloads() {
        // First reload: delay conflicted (mine 200, outside 300); base became theirs.
        let base = with(|f| f.delay = "300".into());
        let mine = with(|f| f.delay = "200".into());
        // Second reload: outside did not touch delay again.
        let got = merge_form(&base, &mine, &base, &[Field::Delay]);
        assert_eq!(
            got,
            Merged {
                form: mine.clone(),
                conflicts: vec![Field::Delay]
            }
        );
        // Without the pending mark the field would silently stop being a conflict.
        assert!(merge_form(&base, &mine, &base, &[]).conflicts.is_empty());
        // Typing the outside value resolves it.
        let same = merge_form(&base, &base, &base, &[Field::Delay]);
        assert!(same.conflicts.is_empty());
    }

    #[test]
    fn merge_marks_offsets_typed_while_an_island_was_set_outside() {
        let base = form("g.exe");
        let mine = with(|f| f.offsets[0] = "5".into());
        let theirs = with(|f| f.island = Some("l/i".into()));
        let got = merge_form(&base, &mine, &theirs, &[]);
        assert_eq!(got.conflicts, [Field::Island, Field::Left]);
        assert_eq!(got.form, mine);
        let mine = with(|f| f.monitor = MonitorChoice::Index(1));
        let got = merge_form(&base, &mine, &theirs, &[]);
        assert_eq!(got.conflicts, [Field::Island, Field::Monitor]);
        assert_eq!(got.form, mine);
    }

    #[test]
    fn merge_marks_an_island_typed_over_offsets_changed_outside() {
        let base = form("g.exe");
        let mine = with(|f| f.island = Some("l/i".into()));
        let theirs = with(|f| {
            f.offsets[1] = "7".into();
            f.monitor = MonitorChoice::Index(2);
        });
        let got = merge_form(&base, &mine, &theirs, &[]);
        assert_eq!(got.conflicts, [Field::Island, Field::Monitor, Field::Top]);
        assert_eq!(got.form, mine);
    }

    #[test]
    fn merge_does_not_couple_an_island_both_sides_agree_on() {
        let base = form("g.exe");
        let mine = with(|f| f.island = Some("l/i".into()));
        let theirs = with(|f| {
            f.island = Some("l/i".into());
            f.offsets[1] = "7".into();
        });
        assert!(merge_form(&base, &mine, &theirs, &[]).conflicts.is_empty());
    }

    #[test]
    fn merge_treats_width_and_height_as_a_pair() {
        let base = form("g.exe");
        let mine = with(|f| f.width = "1280".into());
        let theirs = with(|f| f.height = "1440".into());
        let got = merge_form(&base, &mine, &theirs, &[]);
        assert_eq!(got.conflicts, [Field::Width, Field::Height]);
        assert_eq!(got.form, mine);
        // One side changing the pair is not a clash: both halves travel together.
        let got = merge_form(&base, &base, &with(|f| f.width = "1280".into()), &[]);
        assert!(got.conflicts.is_empty());
        // Both sides agreeing on the pair is no clash either.
        let same = with(|f| {
            f.width = "1280".into();
            f.height = "720".into();
        });
        assert!(merge_form(&base, &same, &same, &[]).conflicts.is_empty());
    }

    #[test]
    fn merge_takes_a_padded_island_as_the_same_island() {
        let base = form("g.exe");
        let mine = with(|f| f.island = Some(" l/i ".into()));
        let theirs = with(|f| f.island = Some("l/i".into()));
        assert!(merge_form(&base, &mine, &theirs, &[]).conflicts.is_empty());
    }

    #[test]
    fn merge_takes_a_process_changed_outside() {
        let base = form("g.exe");
        let theirs = form("h.exe");
        let got = merge_form(&base, &base, &theirs, &[]);
        assert_eq!(got.form.process, "h.exe");
        assert!(got.conflicts.is_empty());
    }

    #[test]
    fn merge_keeps_a_pending_field_in_conflict_when_outside_changes_again() {
        let base = form("g.exe");
        let theirs = with(|f| f.delay = "300".into());
        let got = merge_form(&base, &base, &theirs, &[Field::Delay]);
        assert_eq!(got.conflicts, [Field::Delay]);
        assert_eq!(got.form, base);
    }

    #[test]
    fn take_fields_changes_only_the_given_fields() {
        let mine = with(|f| {
            f.delay = "200".into();
            f.offsets[0] = "5".into();
            f.enabled = false;
            f.island = Some("a/x".into());
        });
        let theirs = with(|f| {
            f.delay = "300".into();
            f.offsets[0] = "9".into();
            f.enabled = true;
            f.island = None;
            f.monitor = MonitorChoice::Index(3);
        });
        let got = take_fields(&mine, &theirs, &[Field::Delay, Field::Island]);
        let want = FormInput {
            delay: "300".into(),
            island: None,
            ..mine.clone()
        };
        assert_eq!(got, want);
        assert_eq!(take_fields(&mine, &theirs, &[]), mine);
    }

    #[test]
    fn outside_value_is_readable() {
        let theirs = with(|f| {
            f.delay = " 300 ".into();
            f.offsets[3] = "-4".into();
            f.monitor = MonitorChoice::Index(2);
            f.enabled = false;
        });
        let isl = with(|f| f.island = Some(" l/i ".into()));
        let texts = |l: Locale| {
            let _g = i18n::scoped_locale(l);
            [
                outside_value(&theirs, Field::Island, &[]),
                outside_value(&theirs, Field::Width, &[]),
                outside_value(&theirs, Field::Monitor, &[]),
                outside_value(&FormInput::blank(), Field::Monitor, &[]),
                outside_value(&theirs, Field::Enabled, &[]),
                outside_value(&FormInput::blank(), Field::Enabled, &[]),
            ]
        };
        let en = texts(Locale::En);
        assert_eq!(
            en,
            [
                "No island",
                "(empty)",
                "Monitor 2",
                "Current",
                "Enabled: no",
                "Enabled: yes"
            ]
        );
        // Spanish is not pinned: it must exist, differ from English and not be the raw key.
        for (i, es) in texts(Locale::Es).into_iter().enumerate() {
            // "Monitor 2" is the same word in both languages.
            if i != 2 {
                assert_ne!(es, en[i], "{es}");
            }
            assert!(!es.starts_with("profiles."), "{es}");
        }
        for l in [Locale::En, Locale::Es] {
            let _g = i18n::scoped_locale(l);
            assert_eq!(outside_value(&theirs, Field::Delay, &[]), "300");
            assert_eq!(outside_value(&theirs, Field::Bottom, &[]), "-4");
            assert_eq!(outside_value(&isl, Field::Island, &[]), "l/i");
        }
    }

    #[test]
    fn outside_value_shows_the_canonical_island_ref() {
        let layouts = merged_layouts(&[]);
        let legacy = with(|f| f.island = Some(" mitades-16-9/derecha ".into())); // legacy alias
        assert_eq!(
            outside_value(&legacy, Field::Island, &layouts),
            "halves-16-9/right"
        );
        let gone = with(|f| f.island = Some("gone/x".into()));
        assert_eq!(outside_value(&gone, Field::Island, &layouts), "gone/x");
    }

    // ---------------------------------------------------------- reload

    #[test]
    fn after_reload_keeps_the_form_when_nothing_changed() {
        let mine = with(|f| f.delay = "200".into());
        let got = after_reload(Some(&form("g.exe")), &mine, &[], &OutsideChange::None);
        assert_eq!(got, ReloadOutcome::Keep);
    }

    #[test]
    fn after_reload_forgets_the_base_when_the_profile_is_removed() {
        let mine = form("g.exe");
        let got = after_reload(Some(&mine), &mine, &[Field::Delay], &OutsideChange::Removed);
        assert_eq!(got, ReloadOutcome::Removed);
    }

    #[test]
    fn after_reload_merges_over_the_base() {
        let base = form("g.exe");
        let mine = with(|f| f.delay = "200".into());
        let mut p = prof("g.exe");
        p.offsets.top = 7;
        let got = after_reload(Some(&base), &mine, &[], &OutsideChange::Changed(p));
        let want = with(|f| {
            f.delay = "200".into();
            f.offsets[1] = "7".into();
        });
        assert_eq!(
            got,
            ReloadOutcome::Merge(Box::new(Merged {
                form: want,
                conflicts: vec![]
            }))
        );
    }

    #[test]
    fn after_reload_passes_the_pending_conflicts_through() {
        // Delay conflicted before (mine 200); outside did not touch it again.
        let base = form("g.exe");
        let mine = with(|f| f.delay = "200".into());
        let change = OutsideChange::Changed(prof("g.exe"));
        let kept = after_reload(Some(&base), &mine, &[Field::Delay], &change);
        assert_eq!(
            kept,
            ReloadOutcome::Merge(Box::new(Merged {
                form: mine.clone(),
                conflicts: vec![Field::Delay]
            }))
        );
        // Same inputs without the pending mark: the conflict would vanish.
        let dropped = after_reload(Some(&base), &mine, &[], &change);
        assert_eq!(
            dropped,
            ReloadOutcome::Merge(Box::new(Merged {
                form: mine,
                conflicts: vec![]
            }))
        );
    }

    #[test]
    fn after_reload_without_a_base_keeps_mine_and_marks_every_difference() {
        let mine = with(|f| {
            f.delay = "200".into();
            f.offsets[0] = "5".into();
        });
        let mut p = prof("g.exe");
        p.delay_ms = 300;
        p.offsets.top = 7;
        let got = after_reload(None, &mine, &[], &OutsideChange::Changed(p));
        assert_eq!(
            got,
            ReloadOutcome::Merge(Box::new(Merged {
                form: mine,
                conflicts: vec![Field::Left, Field::Top, Field::Delay]
            }))
        );
    }

    // ---------------------------------------------------------- headless view (GPUI test platform)

    use gpui_kit::base::test_support::{ElementSnapshot, find};
    use gpui_kit::{
        Bounds, Modifiers, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, size,
    };

    /// The panel's Profiles view in a headless test window, set up as `gui::run` does.
    fn open_view(cx: &mut TestAppContext) -> (VisualTestContext, Entity<ProfilesView>) {
        let (win, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::gui::theme::install(cx);
            let opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(1200.), px(2000.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(opts, cx, |w, cx| cx.new(|cx| ProfilesView::new(w, cx))).unwrap()
        });
        (VisualTestContext::from_window(win, cx), view)
    }

    /// The element registered as `id` (`.test_support()`) in the last frame, if any.
    fn element(cx: &mut VisualTestContext, id: &'static str) -> Option<ElementSnapshot> {
        cx.update(|w, _| find(w, &[], &id.into()))
    }

    /// A left click at the centre of the element `id`, as the mouse would do it.
    fn click(cx: &mut VisualTestContext, id: &'static str) {
        let at = element(cx, id)
            .unwrap_or_else(|| panic!("{id} is not on screen"))
            .bounds()
            .center();
        cx.simulate_click(at, Modifiers::none());
    }

    #[gpui_kit::test]
    fn the_profiles_view_renders_headless(cx: &mut TestAppContext) {
        let (mut cx, _view) = open_view(cx);
        cx.run_until_parked();
        assert!(element(&mut cx, "refresh").is_some_and(|e| e.visible()));
        assert!(element(&mut cx, "new-profile").is_some_and(|e| e.visible()));
    }

    /// What `load` returns for one profile, `game.exe`, saved with `delay_ms`.
    fn loaded(delay_ms: u64, revision: &str) -> Result<Reply<Data>, String> {
        let mut p = prof("game.exe");
        p.delay_ms = delay_ms;
        Ok(Reply::Ok(Data {
            path: "profiles.json".into(),
            revision: revision.into(),
            profiles: vec![p],
            warnings: Vec::new(),
            layouts: Vec::new(),
            islands: Vec::new(),
            monitors: Vec::new(),
            processes: Vec::new(),
        }))
    }

    /// The open editor's Delay text and conflicting fields.
    fn delay_and_conflicts(
        cx: &mut VisualTestContext,
        view: &Entity<ProfilesView>,
    ) -> (String, Vec<Field>) {
        cx.update(|_, app| {
            let e = view.read(app).editor.as_ref().expect("editor open");
            (e.delay.read(app).value().to_string(), e.conflicts.clone())
        })
    }

    /// #15: the editor is open on game.exe (delay 100), 200 is typed in Delay, and a reload
    /// brings 300 saved outside for the same field.
    fn typed_over_an_outside_change(
        cx: &mut TestAppContext,
    ) -> (VisualTestContext, Entity<ProfilesView>) {
        let (mut cx, view) = open_view(cx);
        cx.run_until_parked(); // the first refresh fails: tests never reach a resident
        cx.update(|_, app| view.update(app, |v, cx| v.apply_load(loaded(100, "r1"), cx)));
        click(&mut cx, "edit-0");
        cx.update(|w, app| {
            let delay = view
                .read(app)
                .editor
                .as_ref()
                .expect("editor open")
                .delay
                .clone();
            delay.update(app, |s, cx| s.focus(w, cx));
        });
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("200");
        assert_eq!(delay_and_conflicts(&mut cx, &view), ("200".into(), vec![]));
        cx.update(|_, app| view.update(app, |v, cx| v.apply_load(loaded(300, "r2"), cx)));
        // The typed value stays, the field is marked and both ways out are offered.
        assert_eq!(
            delay_and_conflicts(&mut cx, &view),
            ("200".into(), vec![Field::Delay])
        );
        assert!(element(&mut cx, "merge-keep-mine").is_some_and(|e| e.visible()));
        assert!(element(&mut cx, "merge-use-theirs").is_some_and(|e| e.visible()));
        (cx, view)
    }

    #[gpui_kit::test]
    fn use_the_outside_values_takes_the_outside_delay(cx: &mut TestAppContext) {
        let (mut cx, view) = typed_over_an_outside_change(cx);
        click(&mut cx, "merge-use-theirs");
        assert_eq!(delay_and_conflicts(&mut cx, &view), ("300".into(), vec![]));
        assert!(element(&mut cx, "merge-keep-mine").is_none());
    }

    #[gpui_kit::test]
    fn keep_mine_and_overwrite_saves_the_typed_delay(cx: &mut TestAppContext) {
        let (mut cx, view) = typed_over_an_outside_change(cx);
        click(&mut cx, "merge-keep-mine");
        assert_eq!(delay_and_conflicts(&mut cx, &view), ("200".into(), vec![]));
        assert!(element(&mut cx, "merge-keep-mine").is_none());
        // The save went out (tests have no resident, so it fails there) and kept the form.
        let save_error = cx.update(|_, app| view.read(app).save_error.clone());
        assert_eq!(
            save_error.as_deref(),
            Some(&*i18n::t("common.error.resident_unreachable"))
        );
    }

    // ---------------------------------------------------------- properties

    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngSeed};

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0xB0_5EED),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    /// Small domains so base/mine/theirs collide often; padded values test the `trim()` rule.
    fn any_form() -> impl Strategy<Value = FormInput> {
        let txt = prop::sample::select(vec!["0", "5", " 5 ", "7", ""]);
        let island = prop::option::of(prop::sample::select(vec!["a/x", "b/y"]));
        let monitor = prop_oneof![
            Just(MonitorChoice::Current),
            (0usize..3).prop_map(MonitorChoice::Index)
        ];
        (
            any::<bool>(),
            island,
            monitor,
            [txt.clone(), txt.clone(), txt.clone(), txt.clone()],
            (txt.clone(), txt.clone(), txt),
        )
            .prop_map(
                |(enabled, island, monitor, o, (delay, width, height))| FormInput {
                    process: "g.exe".into(),
                    enabled,
                    island: island.map(String::from),
                    monitor,
                    offsets: o.map(String::from),
                    delay: delay.into(),
                    width: width.into(),
                    height: height.into(),
                },
            )
    }

    proptest! {
        #![proptest_config(cfg())]

        #[test]
        fn no_outside_change_keeps_the_form(b in any_form(), m in any_form()) {
            let got = merge_form(&b, &m, &b, &[]);
            prop_assert_eq!(trimmed(&got.form), trimmed(&m));
            prop_assert!(got.conflicts.is_empty());
        }

        #[test]
        fn no_local_edit_takes_the_outside_form(b in any_form(), t in any_form()) {
            let got = merge_form(&b, &b, &t, &[]);
            prop_assert_eq!(trimmed(&got.form), trimmed(&t));
            prop_assert!(got.conflicts.is_empty());
        }

        #[test]
        fn conflicts_are_real_and_everything_else_is_one_side(
            b in any_form(), m in any_form(), t in any_form(),
            pend in prop::sample::subsequence(MERGE_FIELDS.to_vec(), 0..=11),
        ) {
            let got = merge_form(&b, &m, &t, &pend);
            for f in MERGE_FIELDS {
                let on_mine = take_fields(&got.form, &m, &[f]) == got.form;
                let on_theirs = take_fields(&got.form, &t, &[f]) == got.form;
                if got.conflicts.contains(&f) {
                    prop_assert!(on_mine);
                } else {
                    prop_assert!(on_mine || on_theirs);
                }
            }
        }

        #[test]
        fn a_field_the_user_changed_never_silently_differs_from_mine(
            b in any_form(), m in any_form(), t in any_form(),
            pend in prop::sample::subsequence(MERGE_FIELDS.to_vec(), 0..=11),
        ) {
            let got = merge_form(&b, &m, &t, &pend);
            for f in MERGE_FIELDS {
                if !same_field(&m, &b, f) {
                    prop_assert!(same_field(&got.form, &m, f) || got.conflicts.contains(&f));
                }
            }
        }
    }
}
