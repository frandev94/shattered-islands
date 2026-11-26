//! Pure logic of the island editor: physical-px geometry (move, resize, snapping, 16:9),
//! the synced px/fraction text fields, validation and warnings, and the `set_profiles`
//! payload plus the rebase used after a `revision_conflict`.
//!
//! Source of truth is the stored fraction [`IslandRect`]; px are derived with
//! [`island_rect_on`] on a monitor rect at `(0, 0)` so untouched islands keep their exact
//! fractions (`0.333333…`). [`PxRect`] coordinates are physical px relative to the monitor's
//! top-left corner.
//!
//! Drag pipeline (`move_rect`, `resize_rect`): delta → snap → clamp → minimum size. Alt (no
//! snapping) = pass `None` as `snap`.

use std::borrow::Cow;

use crate::gui::monitor_preview::{Handle, PRect, overlap_rect};
use crate::i18n::t;
use crate::ipc::SetProfilesParams;
use crate::islands::{
    IslandRect, Layout, MonitorInfo, Size, Unit, find_island, island_rect_on, merged_layouts,
    resolve, select_monitor,
};
use crate::profiles::Profile;
use crate::window_state::Rect;

/// Smallest island side in physical px (capped by the monitor's own size).
pub const MIN_SIZE: i32 = 64;

/// Island rectangle in physical px, relative to its monitor's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxRect {
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }
}

/// Snap guides to draw (monitor-relative x and y), when an edge snapped.
pub type Guides = (Option<i32>, Option<i32>);

/// Candidate snap lines per axis, sorted and deduplicated.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapTargets {
    pub xs: Vec<i32>,
    pub ys: Vec<i32>,
}

fn monitor_at_origin(mw: i32, mh: i32) -> Rect {
    Rect {
        left: 0,
        top: 0,
        right: mw,
        bottom: mh,
    }
}

/// Px of an island on a `mw` x `mh` monitor, resolved exactly like the engine does.
pub fn island_px(r: &IslandRect, mw: i32, mh: i32) -> PxRect {
    let p = island_rect_on(monitor_at_origin(mw, mh), r);
    PxRect {
        x: p.left,
        y: p.top,
        w: p.width(),
        h: p.height(),
    }
}

/// Rounds to 6 decimals; `-0.0` becomes `0.0`.
pub fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6 + 0.0
}

/// Position and length as round6 fractions. The engine rejects `pos + len > 1.0` exactly, and
/// rounding both halves up can overshoot 1 by up to 1e-6 (plus f64 noise): such a length loses
/// 1e-6 steps until it fits (same px: the error stays far below half a px on any real monitor).
/// A rect truly past the edge is left alone for `validate` to report.
fn fit_unit(pos: f64, len: f64) -> (f64, f64) {
    let (pos, mut len) = (round6(pos), round6(len));
    while pos + len > 1.0 && pos + len <= 1.0 + 3e-6 {
        len = round6(len - 1e-6);
    }
    (pos, len)
}

fn inside_unit(r: &IslandRect) -> bool {
    r.x + r.w <= 1.0 && r.y + r.h <= 1.0
}

/// Fractions for `target` px: each component keeps its old fraction while its px is unchanged
/// (when the result still resolves to `target`), else everything is converted and round6'd.
/// Always `Unit::Fraction`; a degenerate monitor returns `old`.
pub fn rect_from_px(old: &IslandRect, target: PxRect, mw: i32, mh: i32) -> IslandRect {
    if mw <= 0 || mh <= 0 {
        return *old;
    }
    let frac = |v: i32, d: i32| f64::from(v) / f64::from(d);
    if old.unit == Unit::Fraction {
        let cur = island_px(old, mw, mh);
        let pick = |same: bool, o: f64, v: i32, d: i32| if same { o } else { round6(frac(v, d)) };
        let cand = IslandRect {
            x: pick(target.x == cur.x, old.x, target.x, mw),
            y: pick(target.y == cur.y, old.y, target.y, mh),
            w: pick(target.w == cur.w, old.w, target.w, mw),
            h: pick(target.h == cur.h, old.h, target.h, mh),
            unit: Unit::Fraction,
        };
        if island_px(&cand, mw, mh) == target && (cand == *old || inside_unit(&cand)) {
            return cand;
        }
    }
    let (x, w) = fit_unit(frac(target.x, mw), frac(target.w, mw));
    let (y, h) = fit_unit(frac(target.y, mh), frac(target.h, mh));
    IslandRect {
        x,
        y,
        w,
        h,
        unit: Unit::Fraction,
    }
}

/// Monitor edges, halves, thirds and quarters (rounded like `island_rect_on`) and the edges of
/// the `others` islands.
pub fn snap_targets(mw: i32, mh: i32, others: &[PxRect]) -> SnapTargets {
    let axis = |len: i32, edges: Vec<i32>| {
        let mut v = edges;
        for n in 2..=4 {
            for k in 0..=n {
                v.push((f64::from(k) / f64::from(n) * f64::from(len)).round() as i32);
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    };
    SnapTargets {
        xs: axis(mw, others.iter().flat_map(|o| [o.x, o.right()]).collect()),
        ys: axis(mh, others.iter().flat_map(|o| [o.y, o.bottom()]).collect()),
    }
}

fn dist(a: i32, b: i32) -> f64 {
    (i64::from(a) - i64::from(b)).abs() as f64
}

/// Nearest target within `tol`; on a tie the first one in `targets` order (the lower one for
/// sorted targets).
pub fn snap_value(v: i32, targets: &[i32], tol: f64) -> Option<i32> {
    targets
        .iter()
        .map(|&t| (dist(t, v), t))
        .filter(|(d, _)| *d <= tol)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, t)| t)
}

/// Snaps a span by whichever edge is closer to a target (the leading edge on a tie).
fn snap_span(pos: i32, len: i32, targets: &[i32], tol: f64) -> (i32, Option<i32>) {
    let end = pos.saturating_add(len);
    let lo = snap_value(pos, targets, tol).map(|t| (dist(t, pos), t, t));
    let hi = snap_value(end, targets, tol).map(|t| (dist(t, end), t.saturating_sub(len), t));
    match (lo, hi) {
        (Some(l), Some(h)) if h.0 < l.0 => (h.1, Some(h.2)),
        (Some(l), _) => (l.1, Some(l.2)),
        (None, Some(h)) => (h.1, Some(h.2)),
        (None, None) => (pos, None),
    }
}

/// Size inside `[min(MIN_SIZE, monitor), monitor]` and position inside the monitor.
fn normalize_px(r: PxRect, mw: i32, mh: i32) -> PxRect {
    let w = r.w.clamp(MIN_SIZE.min(mw), mw);
    let h = r.h.clamp(MIN_SIZE.min(mh), mh);
    PxRect {
        x: r.x.clamp(0, mw - w),
        y: r.y.clamp(0, mh - h),
        w,
        h,
    }
}

/// A guide survives only if an edge of the final rect still lies on it.
fn keep_guides(r: PxRect, (gx, gy): Guides) -> Guides {
    (
        gx.filter(|g| *g == r.x || *g == r.right()),
        gy.filter(|g| *g == r.y || *g == r.bottom()),
    )
}

/// Moves `start` by `(dx, dy)`, snaps, then clamps inside the `mw` x `mh` monitor.
pub fn move_rect(
    start: PxRect,
    dx: i32,
    dy: i32,
    mw: i32,
    mh: i32,
    snap: Option<(&SnapTargets, f64)>,
) -> (PxRect, Guides) {
    let (mw, mh) = (mw.max(0), mh.max(0));
    let s = normalize_px(start, mw, mh);
    let (mut x, mut y) = (s.x.saturating_add(dx), s.y.saturating_add(dy));
    let mut guides = (None, None);
    if let Some((t, tol)) = snap {
        (x, guides.0) = snap_span(x, s.w, &t.xs, tol);
        (y, guides.1) = snap_span(y, s.h, &t.ys, tol);
    }
    let r = PxRect {
        x: x.clamp(0, mw - s.w),
        y: y.clamp(0, mh - s.h),
        ..s
    };
    (r, keep_guides(r, guides))
}

/// Drags the edges of `handle` by `(dx, dy)`, snaps only those edges, then clamps them inside
/// the monitor keeping `MIN_SIZE` (or the monitor size when smaller).
pub fn resize_rect(
    start: PxRect,
    handle: Handle,
    dx: i32,
    dy: i32,
    mw: i32,
    mh: i32,
    snap: Option<(&SnapTargets, f64)>,
) -> (PxRect, Guides) {
    use Handle::*;
    let (mw, mh) = (mw.max(0), mh.max(0));
    let s = normalize_px(start, mw, mh);
    let (west, east) = (matches!(handle, W | NW | SW), matches!(handle, E | NE | SE));
    let (north, south) = (matches!(handle, N | NE | NW), matches!(handle, S | SE | SW));
    let (mut l, mut t, mut r, mut b) = (s.x, s.y, s.right(), s.bottom());
    // Only one edge per axis moves, so it alone is shifted and snapped.
    let edge_x = if west {
        Some(&mut l)
    } else {
        east.then_some(&mut r)
    };
    let edge_y = if north {
        Some(&mut t)
    } else {
        south.then_some(&mut b)
    };
    let mut guides = (None, None);
    if let Some(e) = edge_x {
        *e = e.saturating_add(dx);
        if let Some(v) = snap.and_then(|(tg, tol)| snap_value(*e, &tg.xs, tol)) {
            (*e, guides.0) = (v, Some(v));
        }
    }
    if let Some(e) = edge_y {
        *e = e.saturating_add(dy);
        if let Some(v) = snap.and_then(|(tg, tol)| snap_value(*e, &tg.ys, tol)) {
            (*e, guides.1) = (v, Some(v));
        }
    }
    let (min_w, min_h) = (MIN_SIZE.min(mw), MIN_SIZE.min(mh));
    if west {
        l = l.clamp(0, r - min_w);
    } else if east {
        r = r.clamp(l + min_w, mw);
    }
    if north {
        t = t.clamp(0, b - min_h);
    } else if south {
        b = b.clamp(t + min_h, mh);
    }
    let out = PxRect {
        x: l,
        y: t,
        w: r - l,
        h: b - t,
    };
    (out, keep_guides(out, guides))
}

/// "Fit to 16:9": width from the height; shifted left/up when it does not fit; capped at the
/// monitor width (height then follows from it).
pub fn fit_16_9(r: PxRect, mw: i32, mh: i32) -> PxRect {
    let (mw, mh) = (mw.max(0), mh.max(0));
    let mut h = r.h.clamp(0, mh);
    let mut w = (f64::from(h) * 16.0 / 9.0).round() as i32;
    if w > mw {
        w = mw;
        h = (f64::from(mw) * 9.0 / 16.0).round() as i32;
    }
    PxRect {
        x: r.x.clamp(0, mw - w),
        y: r.y.clamp(0, mh - h),
        w,
        h,
    }
}

/// Ratio within 0.01 of 16:9 (the "16:9" badge).
pub fn is_16_9(w: i32, h: i32) -> bool {
    h > 0 && (f64::from(w) / f64::from(h) - 16.0 / 9.0).abs() < 0.01
}

// ------------------------------------------------------------------ fields

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    PxX,
    PxY,
    PxW,
    PxH,
    FrX,
    FrY,
    FrW,
    FrH,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldError {
    NotNumber,
    OutOfMonitor,
    NonPositive,
}

impl FieldError {
    pub fn message(&self) -> Cow<'static, str> {
        t(match self {
            FieldError::NotNumber => "editor.err.not_number",
            FieldError::OutOfMonitor => "editor.err.out_of_monitor",
            FieldError::NonPositive => "editor.err.non_positive",
        })
    }
}

/// Untrusted field text: optional `-`, ASCII digits and, when `decimal`, one `,` or `.`.
/// Anything else (exponents, `NaN`, `inf`, `+`, inner spaces, non-ASCII digits) is refused.
fn parse_number(text: &str, decimal: bool) -> Option<f64> {
    let t = text.trim();
    let body = t.strip_prefix('-').unwrap_or(t);
    let seps = body.chars().filter(|c| matches!(c, ',' | '.')).count();
    let allowed = body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.'));
    if !allowed || !body.chars().any(|c| c.is_ascii_digit()) || seps > usize::from(decimal) {
        return None;
    }
    let v = t.replace(',', ".").parse::<f64>().ok()?;
    v.is_finite().then_some(v + 0.0)
}

/// Applies one edited field. Px fields take whole px and keep the other fractions; fraction
/// fields take decimal comma or point and are round6'd. A px-unit rect is converted first.
pub fn field_edit(
    rect: &IslandRect,
    field: Field,
    text: &str,
    mw: i32,
    mh: i32,
) -> Result<IslandRect, FieldError> {
    use Field::*;
    let is_px = matches!(field, PxX | PxY | PxW | PxH);
    let v = parse_number(text, !is_px).ok_or(FieldError::NotNumber)?;
    // Fractions are stored round6'd: check the value that will be stored.
    let v = if is_px { v } else { round6(v) };
    if matches!(field, PxW | PxH | FrW | FrH) && v <= 0.0 {
        return Err(FieldError::NonPositive);
    }
    if v < 0.0 {
        return Err(FieldError::OutOfMonitor);
    }
    if is_px {
        let p = island_px(rect, mw, mh);
        let (pos, len, limit) = match field {
            PxX => (v, f64::from(p.w), mw),
            PxW => (f64::from(p.x), v, mw),
            PxY => (v, f64::from(p.h), mh),
            _ => (f64::from(p.y), v, mh),
        };
        if pos + len > f64::from(limit) {
            return Err(FieldError::OutOfMonitor);
        }
        // In range: `v` fits the monitor, so the cast is exact.
        let n = v as i32;
        let target = match field {
            PxX => PxRect { x: n, ..p },
            PxY => PxRect { y: n, ..p },
            PxW => PxRect { w: n, ..p },
            _ => PxRect { h: n, ..p },
        };
        return Ok(rect_from_px(rect, target, mw, mh));
    }
    let mut r = match rect.unit {
        Unit::Fraction => *rect,
        Unit::Px => rect_from_px(rect, island_px(rect, mw, mh), mw, mh),
    };
    match field {
        FrX => r.x = v,
        FrY => r.y = v,
        FrW => r.w = v,
        _ => r.h = v,
    }
    if !inside_unit(&r) {
        return Err(FieldError::OutOfMonitor);
    }
    Ok(r)
}

// ------------------------------------------------------------------ checks

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    LayoutIdEmpty,
    LayoutIdSlash,
    LayoutIdDup,
    IslandIdEmpty,
    IslandIdSlash,
    IslandIdDup,
    OutOfMonitor,
    NonPositive,
    TooSmall,
}

impl IssueKind {
    pub fn message(&self) -> Cow<'static, str> {
        t(match self {
            IssueKind::LayoutIdEmpty => "editor.issue.layout_id_empty",
            IssueKind::LayoutIdSlash => "editor.issue.layout_id_slash",
            IssueKind::LayoutIdDup => "editor.issue.layout_id_dup",
            IssueKind::IslandIdEmpty => "editor.issue.island_id_empty",
            IssueKind::IslandIdSlash => "editor.issue.island_id_slash",
            IssueKind::IslandIdDup => "editor.issue.island_id_dup",
            IssueKind::OutOfMonitor => "editor.err.out_of_monitor",
            IssueKind::NonPositive => "editor.err.non_positive",
            IssueKind::TooSmall => "editor.issue.too_small",
        })
    }
}

/// A blocking error; `layout`/`island` index the slice given to [`validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issue {
    pub layout: usize,
    pub island: Option<usize>,
    pub kind: IssueKind,
}

/// Ids compare ignoring case: "Left" and "left" side by side would only confuse.
fn same_id(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// 0 = empty (or blank), 1 = contains `/`, 2 = repeats an earlier id.
fn id_problem<'a>(id: &str, mut earlier: impl Iterator<Item = &'a str>) -> Option<usize> {
    if id.trim().is_empty() {
        Some(0)
    } else if id.contains('/') {
        Some(1)
    } else if earlier.any(|e| same_id(e, id)) {
        Some(2)
    } else {
        None
    }
}

fn monitor_size(l: &Layout, monitors: &[MonitorInfo]) -> Option<(i32, i32)> {
    select_monitor(&l.monitor, monitors)
        .ok()
        .map(|m| (m.rect.width(), m.rect.height()))
}

/// What the engine will store for a rect: px converted to fractions when the monitor is known,
/// fractions round6'd.
fn normalize_rect(r: &IslandRect, size: Option<(i32, i32)>) -> IslandRect {
    let (sx, sy) = match (r.unit, size) {
        (Unit::Fraction, _) => (1.0, 1.0),
        (Unit::Px, Some((mw, mh))) if mw > 0 && mh > 0 => (f64::from(mw), f64::from(mh)),
        (Unit::Px, _) => return *r,
    };
    let (x, w) = fit_unit(r.x / sx, r.w / sx);
    let (y, h) = fit_unit(r.y / sy, r.h / sy);
    IslandRect {
        x,
        y,
        w,
        h,
        unit: Unit::Fraction,
    }
}

/// Upper bound the engine accepts for px islands whose monitor is unknown.
const MAX_PX: f64 = 100_000.0;

fn rect_issue(r: &IslandRect, size: Option<(i32, i32)>) -> Option<IssueKind> {
    let n = normalize_rect(r, size);
    let finite = [n.x, n.y, n.w, n.h].iter().all(|v| v.is_finite());
    if finite && (n.w <= 0.0 || n.h <= 0.0) {
        return Some(IssueKind::NonPositive);
    }
    let beyond = match n.unit {
        Unit::Fraction => !inside_unit(&n),
        Unit::Px => n.x + n.w > MAX_PX || n.y + n.h > MAX_PX,
    };
    if !finite || n.x < 0.0 || n.y < 0.0 || beyond {
        return Some(IssueKind::OutOfMonitor);
    }
    let (mw, mh) = size?;
    let p = island_px(&n, mw, mh);
    (p.w < MIN_SIZE.min(mw) || p.h < MIN_SIZE.min(mh)).then_some(IssueKind::TooSmall)
}

/// Blocking errors of the draft, checked on what will be saved (see [`normalize_layout`]). The
/// size check needs the layout's monitor and is skipped when it is not connected.
pub fn validate(layouts: &[Layout], monitors: &[MonitorInfo]) -> Vec<Issue> {
    let mut out = Vec::new();
    for (li, l) in layouts.iter().enumerate() {
        let earlier = layouts[..li].iter().map(|x| x.id.as_str());
        if let Some(p) = id_problem(&l.id, earlier) {
            let kind = [
                IssueKind::LayoutIdEmpty,
                IssueKind::LayoutIdSlash,
                IssueKind::LayoutIdDup,
            ][p];
            out.push(Issue {
                layout: li,
                island: None,
                kind,
            });
        }
        let size = monitor_size(l, monitors);
        for (ii, island) in l.islands.iter().enumerate() {
            let earlier = l.islands[..ii].iter().map(|x| x.id.as_str());
            let id_kind = id_problem(&island.id, earlier).map(|p| {
                [
                    IssueKind::IslandIdEmpty,
                    IssueKind::IslandIdSlash,
                    IssueKind::IslandIdDup,
                ][p]
            });
            for kind in [id_kind, rect_issue(&island.rect, size)]
                .into_iter()
                .flatten()
            {
                out.push(Issue {
                    layout: li,
                    island: Some(ii),
                    kind,
                });
            }
        }
    }
    out
}

/// Pairs `(i, j)`, `i < j`, whose intersection has a positive area (a warning, not an error).
pub fn overlaps(rects: &[PxRect]) -> Vec<(usize, usize)> {
    let p = |r: &PxRect| PRect {
        x: r.x as f32,
        y: r.y as f32,
        w: r.w as f32,
        h: r.h as f32,
    };
    let mut out = Vec::new();
    for (i, a) in rects.iter().enumerate() {
        for (j, b) in rects.iter().enumerate().skip(i + 1) {
            if overlap_rect(p(a), p(b)).is_some() {
                out.push((i, j));
            }
        }
    }
    out
}

/// `(process, "layout/island")` of profiles whose island exists now (`saved`, presets included)
/// but not after saving `draft`: removed or renamed islands or layouts.
pub fn profile_refs(
    profiles: &[Profile],
    saved: &[Layout],
    draft: &[Layout],
) -> Vec<(String, String)> {
    let (before, after) = (merged_layouts(saved), merged_layouts(draft));
    profiles
        .iter()
        .filter_map(|p| {
            let r = p.island.as_ref()?;
            (find_island(&before, r).is_ok() && find_island(&after, r).is_err())
                .then(|| (p.process.clone(), r.clone()))
        })
        .collect()
}

/// An island whose aspect ratio differs from the `game_resolution` of a profile using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AspectWarning {
    pub process: String,
    pub island: String,
    pub island_size: Size,
    pub game: Size,
}

/// Informative: profiles whose `game_resolution` ratio is more than 0.01 away from the ratio of
/// their island in `draft` (unresolvable islands and invalid sizes are skipped).
pub fn aspect_warnings(
    profiles: &[Profile],
    draft: &[Layout],
    monitors: &[MonitorInfo],
) -> Vec<AspectWarning> {
    let layouts = merged_layouts(draft);
    profiles
        .iter()
        .filter_map(|p| {
            let (r, game) = (p.island.as_ref()?, p.game_resolution?);
            let rect = resolve(&layouts, r, monitors).ok()?;
            let island_size = Size {
                width: rect.width(),
                height: rect.height(),
            };
            let ratio = |s: Size| f64::from(s.width) / f64::from(s.height);
            let valid = |s: Size| s.width > 0 && s.height > 0;
            (valid(game) && valid(island_size) && (ratio(game) - ratio(island_size)).abs() > 0.01)
                .then(|| AspectWarning {
                    process: p.process.clone(),
                    island: r.clone(),
                    island_size,
                    game,
                })
        })
        .collect()
}

// ------------------------------------------------------------------ drafts and saving

/// Id for "Duplicate to edit": `<id>-copy`, then `<id>-copy-2`, … free ignoring case.
pub fn duplicate_id(id: &str, taken: &[&str]) -> String {
    let free = |c: &str| !taken.iter().any(|t| same_id(t, c));
    let base = format!("{id}-copy");
    if free(&base) {
        return base;
    }
    // Among len + 1 candidates at least one is free.
    (2..=taken.len() + 2)
        .map(|n| format!("{base}-{n}"))
        .find(|c| free(c))
        .unwrap_or(base)
}

/// The layout as it will be stored: px islands become fractions when the monitor is connected,
/// fractions are round6'd (never summing past 1); the monitor selector is untouched.
pub fn normalize_layout(l: &Layout, monitors: &[MonitorInfo]) -> Layout {
    let size = monitor_size(l, monitors);
    let mut out = l.clone();
    for i in &mut out.islands {
        i.rect = normalize_rect(&i.rect, size);
    }
    out
}

/// `set_profiles` for saving `draft` over `saved`: only new or changed layouts (normalized),
/// `remove_layouts` = saved ids missing from the draft (deleted or renamed), the read
/// `revision`. `None` when nothing changed. The server upserts first, then removes, so a
/// removed id never collides with an upserted one.
pub fn build_payload(
    saved: &[Layout],
    draft: &[Layout],
    revision: &str,
    monitors: &[MonitorInfo],
) -> Option<SetProfilesParams> {
    let layouts: Vec<Layout> = draft
        .iter()
        .filter(|d| saved.iter().find(|s| s.id == d.id) != Some(*d))
        .map(|d| normalize_layout(d, monitors))
        .collect();
    let remove_layouts: Vec<String> = saved
        .iter()
        .filter(|s| !draft.iter().any(|d| d.id == s.id))
        .map(|s| s.id.clone())
        .collect();
    if layouts.is_empty() && remove_layouts.is_empty() {
        return None;
    }
    Some(SetProfilesParams {
        layouts: (!layouts.is_empty()).then_some(layouts),
        remove_layouts,
        expected_revision: Some(revision.to_owned()),
        ..Default::default()
    })
}

/// After a `revision_conflict`: layouts the user did not touch adopt `new_saved` (or vanish if
/// deleted outside); edited ones are kept; layouts added outside are appended; deleted ones
/// stay deleted. Returns the new draft and the ids where the user's version (edit or deletion)
/// was kept over an outside change, to flag them.
pub fn rebase(
    old_saved: &[Layout],
    new_saved: &[Layout],
    draft: &[Layout],
) -> (Vec<Layout>, Vec<String>) {
    let find = |ls: &'_ [Layout], id: &str| ls.iter().find(|l| l.id == id).cloned();
    let mut out = Vec::new();
    let mut flagged = Vec::new();
    for d in draft {
        let (old, new) = (find(old_saved, &d.id), find(new_saved, &d.id));
        if old.as_ref() == Some(d) {
            out.extend(new);
        } else {
            out.push(d.clone());
            if old != new {
                flagged.push(d.id.clone());
            }
        }
    }
    for o in old_saved {
        let deleted_here = !draft.iter().any(|d| d.id == o.id);
        if deleted_here && find(new_saved, &o.id).is_some_and(|n| n != *o) {
            flagged.push(o.id.clone());
        }
    }
    for n in new_saved {
        let known = old_saved.iter().chain(draft).any(|l| l.id == n.id);
        if !known {
            out.push(n.clone());
        }
    }
    (out, flagged)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Locale, scoped_locale};
    use crate::islands::{Island, MonitorSel};
    use crate::profiles::{ProfileFile, upsert};
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;

    fn fr(x: f64, y: f64, w: f64, h: f64) -> IslandRect {
        IslandRect {
            x,
            y,
            w,
            h,
            unit: Unit::Fraction,
        }
    }

    fn px(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    fn mon(index: usize, w: i32, h: i32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            index,
            device: format!(r"\\.\DISPLAY{}", index + 1),
            rect: Rect {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
            dpi: 96,
            primary,
        }
    }

    fn lay(id: &str, islands: &[(&str, IslandRect)]) -> Layout {
        Layout {
            id: id.into(),
            monitor: MonitorSel::Primary,
            islands: islands
                .iter()
                .map(|(i, r)| Island {
                    id: (*i).into(),
                    rect: *r,
                    extra: Default::default(),
                })
                .collect(),
            extra: Default::default(),
        }
    }

    fn profile(process: &str, island: &str, game: Option<(i32, i32)>) -> Profile {
        Profile {
            process: process.into(),
            enabled: true,
            monitor: Default::default(),
            offsets: Default::default(),
            delay_ms: 0,
            clip_cursor: false,
            island: Some(island.into()),
            game_resolution: game.map(|(width, height)| Size { width, height }),
            extra: Default::default(),
        }
    }

    fn decimals_ok(v: f64) -> bool {
        round6(v) == v
    }

    // ---------------------------------------------------------- fraction <-> px

    const WIDTHS: [i32; 6] = [1920, 2560, 2561, 3440, 5120, 7680];

    #[test]
    fn island_px_matches_island_rect_on() {
        let r = fr(1.0 / 3.0, 0.25, 1.0 / 3.0, 0.5);
        for w in WIDTHS {
            let m = Rect {
                left: 0,
                top: 0,
                right: w,
                bottom: 1440,
            };
            let e = island_rect_on(m, &r);
            assert_eq!(
                island_px(&r, w, 1440),
                px(e.left, e.top, e.width(), e.height())
            );
        }
    }

    #[test]
    fn unchanged_px_keep_the_exact_fractions() {
        let r = fr(1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0);
        for w in WIDTHS {
            assert_eq!(rect_from_px(&r, island_px(&r, w, 1440), w, 1440), r);
        }
    }

    #[test]
    fn unchanged_px_keep_old_fractions_even_past_the_edge() {
        // Saved before round6: x + w = 1.0000001 > 1, yet the same px must not re-derive it.
        let r = fr(0.6666667, 0.0, 0.3333334, 1.0);
        for w in WIDTHS {
            assert_eq!(rect_from_px(&r, island_px(&r, w, 1440), w, 1440), r, "{w}");
        }
    }

    #[test]
    fn changed_px_roundtrip_through_island_rect_on() {
        for w in WIDTHS {
            for r in [
                fr(0.0, 0.0, 0.5, 1.0),
                fr(0.5, 0.0, 0.5, 1.0),
                fr(1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0),
                fr(0.25, 0.25, 0.5, 0.5),
            ] {
                let p = island_px(&r, w, 1440);
                let target = px(p.x + 1, p.y + 1, p.w - 2, p.h - 2);
                let got = rect_from_px(&r, target, w, 1440);
                assert_eq!(island_px(&got, w, 1440), target, "w={w} r={r:?}");
                assert_eq!(got.unit, Unit::Fraction);
                for v in [got.x, got.y, got.w, got.h] {
                    assert!(decimals_ok(v), "{v} has more than 6 decimals");
                }
            }
        }
    }

    #[test]
    fn moving_one_axis_keeps_the_other_fractions() {
        let r = fr(1.0 / 3.0, 0.0, 1.0 / 3.0, 0.5);
        let p = island_px(&r, 2561, 1440);
        let got = rect_from_px(&r, px(p.x, p.y + 10, p.w, p.h), 2561, 1440);
        assert_eq!((got.x, got.w, got.h), (r.x, r.w, r.h));
        assert_eq!(got.y, round6(10.0 / 1440.0));
    }

    #[test]
    fn px_unit_becomes_fractions() {
        let r = IslandRect {
            x: 2560.0,
            y: 0.0,
            w: 2560.0,
            h: 1440.0,
            unit: Unit::Px,
        };
        let got = rect_from_px(&r, island_px(&r, 5120, 1440), 5120, 1440);
        assert_eq!(got, fr(0.5, 0.0, 0.5, 1.0));
    }

    #[test]
    fn right_aligned_split_never_sums_past_one() {
        // The engine rejects x + w > 1.0 exactly, so float rounding must never exceed it.
        for w in [2561, 1920, 3440] {
            for x in 1..w {
                let t = px(x, 0, w - x, 1440);
                let got = rect_from_px(&fr(0.0, 0.0, 0.1, 0.1), t, w, 1440);
                assert!(got.x + got.w <= 1.0, "w={w} x={x}: {got:?}");
                assert_eq!(island_px(&got, w, 1440), t, "w={w} x={x}");
            }
        }
    }

    #[test]
    fn degenerate_monitor_keeps_the_old_rect() {
        let r = fr(0.5, 0.0, 0.5, 1.0);
        assert_eq!(rect_from_px(&r, px(1, 1, 10, 10), 0, 1440), r);
    }

    // ---------------------------------------------------------- snapping

    #[test]
    fn snap_targets_cover_edges_halves_thirds_quarters_and_islands() {
        let t = snap_targets(2561, 1440, &[px(100, 50, 300, 200)]);
        assert_eq!(
            t.xs,
            vec![0, 100, 400, 640, 854, 1281, 1707, 1921, 2561],
            "rounded like island_rect_on (half away from zero)"
        );
        assert_eq!(t.ys, vec![0, 50, 250, 360, 480, 720, 960, 1080, 1440]);
    }

    #[test]
    fn snap_value_takes_the_nearest_inside_tolerance() {
        assert_eq!(snap_value(1275, &[0, 1280, 2560], 8.0), Some(1280));
        assert_eq!(snap_value(1270, &[0, 1280, 2560], 8.0), None);
        assert_eq!(snap_value(1272, &[0, 1280, 2560], 8.0), Some(1280));
    }

    #[test]
    fn snap_value_without_targets_is_none() {
        assert_eq!(snap_value(10, &[], 8.0), None);
        assert_eq!(snap_value(10, &[12], f64::NAN), None);
    }

    #[test]
    fn snap_value_tie_takes_the_lower_target() {
        assert_eq!(snap_value(105, &[100, 110], 8.0), Some(100));
        assert_eq!(
            snap_value(105, &[110, 100], 8.0),
            Some(110),
            "first in order"
        );
    }

    #[test]
    fn move_snaps_the_closer_edge_and_reports_the_guide() {
        let t = snap_targets(5120, 1440, &[]);
        // Right edge 2555 is 5 px from the half (2560); left edge 1255 is 25 px from 1280.
        let (r, g) = move_rect(px(1000, 0, 1300, 1440), 255, 0, 5120, 1440, Some((&t, 8.0)));
        assert_eq!(r, px(1260, 0, 1300, 1440));
        assert_eq!(g, (Some(2560), Some(0)));
    }

    #[test]
    fn move_with_alt_ignores_snapping() {
        let (r, g) = move_rect(px(1000, 0, 1300, 1440), 255, 0, 5120, 1440, None);
        assert_eq!(r, px(1255, 0, 1300, 1440));
        assert_eq!(g, (None, None));
    }

    #[test]
    fn move_tie_between_edges_prefers_the_left_one() {
        let t = SnapTargets {
            xs: vec![100, 310],
            ys: vec![],
        };
        // left 105 is 5 from 100, right 305 is 5 from 310.
        let (r, g) = move_rect(px(105, 0, 200, 100), 0, 0, 1000, 1000, Some((&t, 8.0)));
        assert_eq!((r.x, g.0), (100, Some(100)));
    }

    #[test]
    fn move_clamps_inside_the_monitor() {
        let (r, _) = move_rect(px(100, 100, 500, 400), -900, 5000, 1920, 1080, None);
        assert_eq!(r, px(0, 680, 500, 400));
    }

    #[test]
    fn move_drops_a_guide_the_clamp_moved_away_from() {
        let t = SnapTargets {
            xs: vec![-50],
            ys: vec![],
        };
        let (r, g) = move_rect(px(0, 0, 100, 100), -45, 0, 1000, 1000, Some((&t, 8.0)));
        assert_eq!((r.x, g.0), (0, None));
    }

    #[test]
    fn move_survives_hostile_deltas() {
        let (r, _) = move_rect(px(10, 10, 100, 100), i32::MAX, i32::MIN, 1920, 1080, None);
        assert_eq!(r, px(1820, 0, 100, 100));
    }

    #[test]
    fn resize_moves_only_the_handle_edges() {
        let s = px(100, 100, 400, 300);
        let cases = [
            (Handle::N, px(100, 90, 400, 310)),
            (Handle::NE, px(100, 90, 410, 310)),
            (Handle::E, px(100, 100, 410, 300)),
            (Handle::SE, px(100, 100, 410, 290)),
            (Handle::S, px(100, 100, 400, 290)),
            (Handle::SW, px(110, 100, 390, 290)),
            (Handle::W, px(110, 100, 390, 300)),
            (Handle::NW, px(110, 90, 390, 310)),
        ];
        for (h, want) in cases {
            let (r, _) = resize_rect(s, h, 10, -10, 1920, 1080, None);
            assert_eq!(r, want, "{h:?}");
        }
    }

    #[test]
    fn resize_keeps_the_64_px_minimum() {
        let (r, _) = resize_rect(px(100, 100, 400, 300), Handle::W, 1000, 0, 1920, 1080, None);
        assert_eq!(r, px(436, 100, 64, 300));
        let (r, _) = resize_rect(px(100, 100, 400, 300), Handle::N, 0, 1000, 1920, 1080, None);
        assert_eq!(r, px(100, 336, 400, 64));
    }

    #[test]
    fn resize_keeps_the_64_px_minimum_on_the_east_edge() {
        let (r, _) = resize_rect(
            px(100, 100, 400, 300),
            Handle::E,
            -1000,
            0,
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(100, 100, 64, 300));
    }

    #[test]
    fn resize_clamps_to_the_monitor() {
        let (r, _) = resize_rect(
            px(100, 100, 400, 300),
            Handle::SE,
            5000,
            5000,
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(100, 100, 1820, 980));
        let (r, _) = resize_rect(
            px(100, 100, 400, 300),
            Handle::NW,
            -500,
            -500,
            1920,
            1080,
            None,
        );
        assert_eq!(r, px(0, 0, 500, 400));
    }

    #[test]
    fn resize_snaps_only_the_moving_edge() {
        let t = snap_targets(1920, 1080, &[]);
        // E handle: right edge 955 snaps to 960; the left edge (100) stays although 100 is no line.
        let (r, g) = resize_rect(
            px(100, 100, 850, 300),
            Handle::E,
            5,
            0,
            1920,
            1080,
            Some((&t, 8.0)),
        );
        assert_eq!(r, px(100, 100, 860, 300));
        assert_eq!(g, (Some(960), None));
    }

    #[test]
    fn undersized_or_outside_start_is_normalised() {
        let (r, _) = move_rect(px(-50, 2000, 10, 5000), 0, 0, 1920, 1080, None);
        assert_eq!(r, px(0, 0, 64, 1080));
        let (r, _) = resize_rect(px(0, 0, 10, 10), Handle::SE, 0, 0, 40, 30, None);
        assert_eq!(
            r,
            px(0, 0, 40, 30),
            "a monitor under 64 px caps the minimum"
        );
    }

    // ---------------------------------------------------------- 16:9

    #[test]
    fn half_of_a_5120x1440_is_16_9() {
        assert!(is_16_9(2560, 1440));
        assert!(is_16_9(1920, 1080));
        assert!(!is_16_9(1280, 1440));
        assert!(!is_16_9(100, 0));
    }

    #[test]
    fn fit_16_9_takes_the_width_from_the_height() {
        assert_eq!(
            fit_16_9(px(0, 0, 1000, 720), 5120, 1440),
            px(0, 0, 1280, 720)
        );
    }

    #[test]
    fn fit_16_9_shifts_left_when_it_does_not_fit() {
        assert_eq!(
            fit_16_9(px(4500, 0, 600, 1440), 5120, 1440),
            px(2560, 0, 2560, 1440)
        );
    }

    #[test]
    fn fit_16_9_caps_the_width_at_the_monitor() {
        assert_eq!(
            fit_16_9(px(0, 0, 500, 1080), 1280, 1080),
            px(0, 0, 1280, 720)
        );
    }

    // ---------------------------------------------------------- fields

    #[test]
    fn fraction_fields_accept_decimal_comma_and_point() {
        let r = fr(0.0, 0.0, 0.25, 1.0);
        assert_eq!(
            field_edit(&r, Field::FrX, "0,5", 1920, 1080).map(|r| r.x),
            Ok(0.5)
        );
        assert_eq!(
            field_edit(&r, Field::FrX, " 0.5 ", 1920, 1080).map(|r| r.x),
            Ok(0.5)
        );
        assert_eq!(
            field_edit(&r, Field::FrW, ",75", 1920, 1080).map(|r| r.w),
            Ok(0.75)
        );
    }

    #[test]
    fn fraction_fields_round_to_six_decimals() {
        let got = field_edit(
            &fr(0.0, 0.0, 0.25, 1.0),
            Field::FrX,
            "0,1234567",
            1920,
            1080,
        );
        assert_eq!(got.map(|r| r.x), Ok(0.123457));
    }

    #[test]
    fn px_fields_keep_the_other_fractions() {
        let r = fr(1.0 / 3.0, 0.0, 1.0 / 3.0, 0.5);
        let got = field_edit(&r, Field::PxY, "100", 2561, 1440).map(|g| (g.x, g.w, g.y));
        assert_eq!(got, Ok((r.x, r.w, round6(100.0 / 1440.0))));
        let got = field_edit(&r, Field::PxW, "200", 2561, 1440).map(|g| island_px(&g, 2561, 1440));
        assert_eq!(got.map(|p| p.w), Ok(200));
    }

    #[test]
    fn garbage_text_is_not_a_number() {
        let r = fr(0.0, 0.0, 0.5, 0.5);
        let long_nines = "9".repeat(400);
        let hostile = [
            "",
            "   ",
            "abc",
            "NaN",
            "nan",
            "inf",
            "-inf",
            "infinity",
            "1e9",
            "1E2",
            "1.5.2",
            "1,5,2",
            "1.5,2",
            "0x10",
            "+5",
            "- 5",
            "5 5",
            "١٢",
            "５",
            "1_000",
            ".",
            ",",
            "-",
            &long_nines,
        ];
        for t in hostile {
            for f in [Field::PxX, Field::PxW, Field::FrX, Field::FrW] {
                assert_eq!(
                    field_edit(&r, f, t, 1920, 1080),
                    Err(FieldError::NotNumber),
                    "{f:?} {t:?}"
                );
            }
        }
    }

    #[test]
    fn px_fields_take_whole_numbers_only() {
        // "1.280" would be 1280 for a Spanish reader and 1,28 for a parser: refuse both readings.
        let r = fr(0.0, 0.0, 0.5, 0.5);
        for t in ["1.280", "1,5", "10.0"] {
            assert_eq!(
                field_edit(&r, Field::PxX, t, 1920, 1080),
                Err(FieldError::NotNumber),
                "{t}"
            );
        }
    }

    #[test]
    fn long_zero_padded_text_still_parses() {
        let t = format!("{}100", "0".repeat(10_000));
        let got = field_edit(&fr(0.0, 0.0, 0.5, 0.5), Field::PxX, &t, 1920, 1080);
        assert_eq!(got.map(|r| island_px(&r, 1920, 1080).x), Ok(100));
    }

    #[test]
    fn negative_position_is_out_of_monitor() {
        let r = fr(0.25, 0.25, 0.5, 0.5);
        assert_eq!(
            field_edit(&r, Field::PxX, "-1", 1920, 1080),
            Err(FieldError::OutOfMonitor)
        );
        assert_eq!(
            field_edit(&r, Field::FrY, "-0,1", 1920, 1080),
            Err(FieldError::OutOfMonitor)
        );
    }

    #[test]
    fn minus_zero_is_zero() {
        let got = field_edit(&fr(0.25, 0.25, 0.5, 0.5), Field::FrX, "-0", 1920, 1080);
        let x = got.map(|r| r.x);
        assert_eq!(x, Ok(0.0));
        assert!(x.is_ok_and(|v| v.is_sign_positive()));
    }

    #[test]
    fn size_must_be_positive() {
        let r = fr(0.25, 0.25, 0.5, 0.5);
        for (f, t) in [
            (Field::PxW, "0"),
            (Field::PxH, "-10"),
            (Field::FrW, "0"),
            (Field::FrH, "-0,5"),
            (Field::PxW, "-0"),
        ] {
            assert_eq!(
                field_edit(&r, f, t, 1920, 1080),
                Err(FieldError::NonPositive),
                "{f:?} {t}"
            );
        }
    }

    #[test]
    fn size_that_rounds_to_zero_is_not_positive() {
        let r = fr(0.1, 0.1, 0.5, 0.5);
        assert_eq!(
            field_edit(&r, Field::FrW, "0.0000004", 1920, 1080),
            Err(FieldError::NonPositive)
        );
        assert_eq!(
            field_edit(&r, Field::FrH, "0,0000004", 1920, 1080),
            Err(FieldError::NonPositive)
        );
    }

    #[test]
    fn edits_past_the_monitor_edge_are_rejected() {
        let r = fr(0.25, 0.25, 0.5, 0.5);
        for (f, t) in [
            (Field::PxX, "961"),
            (Field::PxW, "1441"),
            (Field::PxY, "1000000000"),
            (Field::FrX, "0,500001"),
            (Field::FrH, "0,76"),
            (Field::FrW, "2"),
        ] {
            assert_eq!(
                field_edit(&r, f, t, 1920, 1080),
                Err(FieldError::OutOfMonitor),
                "{f:?} {t}"
            );
        }
        assert!(field_edit(&r, Field::PxX, "960", 1920, 1080).is_ok());
    }

    #[test]
    fn fraction_edit_on_a_px_rect_converts_it() {
        let r = IslandRect {
            x: 0.0,
            y: 0.0,
            w: 960.0,
            h: 1080.0,
            unit: Unit::Px,
        };
        assert_eq!(
            field_edit(&r, Field::FrX, "0,5", 1920, 1080),
            Ok(fr(0.5, 0.0, 0.5, 1.0))
        );
    }

    #[test]
    fn field_error_messages_come_from_the_catalog_in_both_languages() {
        let all = [
            (FieldError::NotNumber, "editor.err.not_number"),
            (FieldError::OutOfMonitor, "editor.err.out_of_monitor"),
            (FieldError::NonPositive, "editor.err.non_positive"),
        ];
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            for (e, key) in all {
                assert!(crate::i18n::has(key), "{key}");
                assert_eq!(e.message(), crate::i18n::t(key));
                assert_ne!(e.message(), key);
            }
        }
        let _g = scoped_locale(Locale::En);
        assert_eq!(
            FieldError::OutOfMonitor.message(),
            "The island must stay inside the monitor"
        );
    }

    // ---------------------------------------------------------- validation

    fn kinds(layouts: &[Layout]) -> Vec<(usize, Option<usize>, IssueKind)> {
        validate(layouts, &[mon(0, 1920, 1080, true)])
            .into_iter()
            .map(|i| (i.layout, i.island, i.kind))
            .collect()
    }

    #[test]
    fn a_valid_design_has_no_issues() {
        let l = lay(
            "a",
            &[("i", fr(0.0, 0.0, 0.5, 1.0)), ("j", fr(0.5, 0.0, 0.5, 1.0))],
        );
        assert_eq!(kinds(&[l]), vec![]);
    }

    #[test]
    fn layout_id_problems() {
        let ok = fr(0.0, 0.0, 0.5, 1.0);
        assert_eq!(
            kinds(&[lay("", &[("i", ok)])]),
            vec![(0, None, IssueKind::LayoutIdEmpty)]
        );
        assert_eq!(
            kinds(&[lay("  ", &[("i", ok)])]),
            vec![(0, None, IssueKind::LayoutIdEmpty)]
        );
        assert_eq!(
            kinds(&[lay("a/b", &[("i", ok)])]),
            vec![(0, None, IssueKind::LayoutIdSlash)]
        );
        assert_eq!(
            kinds(&[lay("Mitad", &[("i", ok)]), lay("mitad", &[("i", ok)])]),
            vec![(1, None, IssueKind::LayoutIdDup)],
            "duplicates ignore case"
        );
    }

    #[test]
    fn island_id_problems() {
        let ok = fr(0.0, 0.0, 0.5, 1.0);
        let l = lay(
            "a",
            &[
                ("", ok),
                ("x/y", ok),
                ("Izq", ok),
                ("izq", ok),
                ("ÉL", ok),
                ("él", ok),
            ],
        );
        assert_eq!(
            kinds(&[l]),
            vec![
                (0, Some(0), IssueKind::IslandIdEmpty),
                (0, Some(1), IssueKind::IslandIdSlash),
                (0, Some(3), IssueKind::IslandIdDup),
                (0, Some(5), IssueKind::IslandIdDup),
            ]
        );
    }

    #[test]
    fn same_island_id_in_two_designs_is_fine() {
        let ok = fr(0.0, 0.0, 0.5, 1.0);
        assert_eq!(
            kinds(&[lay("a", &[("i", ok)]), lay("b", &[("i", ok)])]),
            vec![]
        );
    }

    #[test]
    fn rect_problems() {
        let l = lay(
            "a",
            &[
                ("out", fr(0.6, 0.0, 0.5, 1.0)),
                ("neg", fr(-0.1, 0.0, 0.5, 1.0)),
                ("nan", fr(f64::NAN, 0.0, 0.5, 1.0)),
                ("zero", fr(0.0, 0.0, 0.0, 1.0)),
                ("tiny", fr(0.0, 0.0, 0.03, 1.0)),
            ],
        );
        assert_eq!(
            kinds(&[l]),
            vec![
                (0, Some(0), IssueKind::OutOfMonitor),
                (0, Some(1), IssueKind::OutOfMonitor),
                (0, Some(2), IssueKind::OutOfMonitor),
                (0, Some(3), IssueKind::NonPositive),
                (0, Some(4), IssueKind::TooSmall),
            ]
        );
    }

    #[test]
    fn exactly_64_px_is_not_too_small() {
        let w64 = fr(0.0, 0.0, 64.0 / 1920.0, 1.0);
        let h64 = fr(0.0, 0.0, 1.0, 64.0 / 1080.0);
        assert_eq!(
            kinds(&[lay("a", &[("w", w64)]), lay("b", &[("h", h64)])]),
            vec![]
        );
        let w63 = fr(0.0, 0.0, 63.0 / 1920.0, 1.0);
        let h63 = fr(0.0, 0.0, 1.0, 63.0 / 1080.0);
        assert_eq!(
            kinds(&[lay("a", &[("w", w63)]), lay("b", &[("h", h63)])]),
            vec![
                (0, Some(0), IssueKind::TooSmall),
                (1, Some(0), IssueKind::TooSmall)
            ]
        );
    }

    #[test]
    fn px_rects_are_checked_against_their_monitor() {
        let px_rect = |x: f64, w: f64| IslandRect {
            x,
            y: 0.0,
            w,
            h: 1080.0,
            unit: Unit::Px,
        };
        let l = lay(
            "a",
            &[
                ("ok", px_rect(960.0, 960.0)),
                ("out", px_rect(1000.0, 960.0)),
            ],
        );
        assert_eq!(kinds(&[l]), vec![(0, Some(1), IssueKind::OutOfMonitor)]);
    }

    #[test]
    fn missing_monitor_skips_only_the_size_check() {
        let mut l = lay(
            "a",
            &[
                ("tiny", fr(0.0, 0.0, 0.01, 1.0)),
                ("out", fr(0.9, 0.0, 0.5, 1.0)),
            ],
        );
        l.monitor = MonitorSel::Index(7);
        assert_eq!(kinds(&[l]), vec![(0, Some(1), IssueKind::OutOfMonitor)]);
    }

    #[test]
    fn issue_messages_are_distinct_in_every_language() {
        let all = [
            IssueKind::LayoutIdEmpty,
            IssueKind::LayoutIdSlash,
            IssueKind::LayoutIdDup,
            IssueKind::IslandIdEmpty,
            IssueKind::IslandIdSlash,
            IssueKind::IslandIdDup,
            IssueKind::OutOfMonitor,
            IssueKind::NonPositive,
            IssueKind::TooSmall,
        ];
        for l in [Locale::En, Locale::Es] {
            let _g = scoped_locale(l);
            let msgs: std::collections::HashSet<_> = all.iter().map(|k| k.message()).collect();
            assert_eq!(msgs.len(), all.len());
            assert!(msgs.iter().all(|m| !m.starts_with("editor.")), "{msgs:?}");
        }
        let _g = scoped_locale(Locale::En);
        assert_eq!(IssueKind::TooSmall.message(), "The island is too small");
    }

    // ---------------------------------------------------------- warnings

    #[test]
    fn overlaps_need_a_positive_area() {
        let rects = [
            px(0, 0, 100, 100),
            px(100, 0, 100, 100),
            px(50, 50, 100, 100),
            px(0, 100, 60, 50),
        ];
        assert_eq!(overlaps(&rects), vec![(0, 2), (1, 2), (2, 3)]);
    }

    #[test]
    fn profile_refs_flag_removed_and_renamed_islands() {
        let ok = fr(0.0, 0.0, 0.5, 1.0);
        let saved = vec![lay("a", &[("i", ok), ("j", ok)]), lay("b", &[("k", ok)])];
        let draft = vec![lay("a", &[("i2", ok), ("j", ok)])];
        let profiles = [
            profile("one.exe", "a/i", None),
            profile("two.exe", "a/j", None),
            profile("three.exe", "b/k", None),
            profile("four.exe", "mitades-16-9/izquierda", None), // legacy alias test
            profile("six.exe", "halves-16-9/left", None),
            profile("five.exe", "zzz/q", None),
        ];
        assert_eq!(
            profile_refs(&profiles, &saved, &draft),
            vec![
                ("one.exe".to_string(), "a/i".to_string()),
                ("three.exe".to_string(), "b/k".to_string()),
            ]
        );
    }

    #[test]
    fn deleting_a_preset_override_keeps_the_preset_reference() {
        let over = lay("halves-16-9", &[("left", fr(0.0, 0.0, 0.4, 1.0))]);
        let profiles = [profile("g.exe", "halves-16-9/left", None)];
        assert_eq!(profile_refs(&profiles, &[over], &[]), vec![]);
    }

    #[test]
    fn deleting_a_preset_override_keeps_the_legacy_reference() {
        let over = lay("halves-16-9", &[("left", fr(0.0, 0.0, 0.4, 1.0))]);
        let profiles = [profile("g.exe", "mitades-16-9/izquierda", None)]; // legacy alias test
        assert_eq!(profile_refs(&profiles, &[over], &[]), vec![]);
    }

    #[test]
    fn aspect_warning_when_the_game_ratio_differs() {
        let draft = vec![lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))])];
        let monitors = [mon(0, 5120, 1440, true)];
        let profiles = [
            profile("ok.exe", "a/i", Some((1280, 720))),
            profile("bad.exe", "a/i", Some((1280, 1024))),
            profile("none.exe", "a/i", None),
            profile("gone.exe", "a/zz", Some((800, 600))),
            profile("zero.exe", "a/i", Some((0, 0))),
        ];
        assert_eq!(
            aspect_warnings(&profiles, &draft, &monitors),
            vec![AspectWarning {
                process: "bad.exe".into(),
                island: "a/i".into(),
                island_size: Size {
                    width: 2560,
                    height: 1440
                },
                game: Size {
                    width: 1280,
                    height: 1024
                },
            }]
        );
    }

    // ---------------------------------------------------------- drafts

    #[test]
    fn duplicate_id_adds_copy_then_a_counter() {
        assert_eq!(
            duplicate_id("halves-16-9", &["halves-16-9"]),
            "halves-16-9-copy"
        );
        assert_eq!(
            duplicate_id("a", &["a", "A-COPY", "a-copy-2"]),
            "a-copy-3",
            "taken ids compare ignoring case"
        );
    }

    #[test]
    fn duplicate_id_counter_starts_at_2() {
        assert_eq!(duplicate_id("a", &["a-copy"]), "a-copy-2");
    }

    // ---------------------------------------------------------- payload

    fn monitors() -> Vec<MonitorInfo> {
        vec![mon(0, 2561, 1440, true)]
    }

    #[test]
    fn unchanged_draft_has_no_payload() {
        let saved = vec![lay("a", &[("i", fr(1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0))])];
        assert_eq!(
            build_payload(&saved, &saved.clone(), "r1", &monitors()),
            None
        );
    }

    #[test]
    fn new_and_modified_layouts_are_sent_alone() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let b = lay("b", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut b2 = b.clone();
        b2.islands[0].rect.w = 0.25;
        let c = lay("c", &[("i", fr(0.5, 0.0, 0.5, 1.0))]);
        let p = build_payload(
            &[a.clone(), b],
            &[a, b2.clone(), c.clone()],
            "r1",
            &monitors(),
        );
        let p = p.unwrap_or_default();
        assert_eq!(p.layouts, Some(vec![b2, c]));
        assert_eq!(p.remove_layouts, Vec::<String>::new());
        assert_eq!(p.expected_revision.as_deref(), Some("r1"));
        assert_eq!((p.profiles, p.remove), (None, None));
    }

    #[test]
    fn deleted_layouts_go_to_remove_layouts_only() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let b = lay("b", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let p = build_payload(&[a.clone(), b], &[a], "r1", &monitors()).unwrap_or_default();
        assert_eq!(p.layouts, None);
        assert_eq!(p.remove_layouts, vec!["b".to_string()]);
        assert_eq!(p.expected_revision.as_deref(), Some("r1"));
    }

    #[test]
    fn only_removals_send_no_layouts() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let b = lay("b", &[("i", fr(0.5, 0.0, 0.5, 1.0))]);
        let p = build_payload(&[a, b], &[], "r1", &monitors()).unwrap_or_default();
        assert_eq!(p.layouts, None);
        assert_eq!(p.remove_layouts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn renamed_layout_is_an_upsert_plus_a_removal() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut renamed = a.clone();
        renamed.id = "A".into();
        let p = build_payload(&[a], &[renamed.clone()], "r1", &monitors()).unwrap_or_default();
        assert_eq!(p.layouts, Some(vec![renamed]));
        assert_eq!(p.remove_layouts, vec!["a".to_string()]);
    }

    #[test]
    fn px_islands_are_saved_as_fractions() {
        let mut l = lay("a", &[]);
        l.islands.push(Island {
            id: "i".into(),
            rect: IslandRect {
                x: 1280.0,
                y: 0.0,
                w: 1281.0,
                h: 1440.0,
                unit: Unit::Px,
            },
            extra: Default::default(),
        });
        let p = build_payload(&[], &[l], "r1", &monitors()).unwrap_or_default();
        let r = p.layouts.unwrap_or_default()[0].islands[0].rect;
        assert_eq!(r.unit, Unit::Fraction);
        assert_eq!((r.x, r.y, r.h), (0.499805, 0.0, 1.0));
        assert!(r.x + r.w <= 1.0);
        assert_eq!(island_px(&r, 2561, 1440), px(1280, 0, 1281, 1440));
    }

    #[test]
    fn px_islands_on_a_missing_monitor_stay_px() {
        let mut l = lay("a", &[]);
        l.monitor = MonitorSel::Device("nope".into());
        let r = IslandRect {
            x: 10.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
            unit: Unit::Px,
        };
        l.islands.push(Island {
            id: "i".into(),
            rect: r,
            extra: Default::default(),
        });
        let p = build_payload(&[], &[l], "r1", &monitors()).unwrap_or_default();
        assert_eq!(p.layouts.unwrap_or_default()[0].islands[0].rect, r);
    }

    #[test]
    fn edited_layout_payload_keeps_unknown_fields() {
        let saved: Layout = serde_json::from_value(serde_json::json!({
            "id": "a", "monitor": "primary", "_lk": [1],
            "islands": [{"id": "i", "_ik": "keep", "rect": {"x": 0, "y": 0, "w": 0.5, "h": 1}}],
        }))
        .unwrap();
        let mut draft = saved.clone();
        draft.islands[0].rect.x = 0.25;
        let p = build_payload(&[saved], &[draft], "r1", &monitors()).unwrap_or_default();
        let sent = serde_json::to_value(p.layouts.unwrap_or_default()).unwrap();
        assert_eq!(sent[0]["_lk"], serde_json::json!([1]));
        assert_eq!(sent[0]["islands"][0]["_ik"], "keep");
        assert_eq!(sent[0]["islands"][0]["rect"]["x"], 0.25);
    }

    #[test]
    fn saved_fractions_have_at_most_six_decimals() {
        let l = lay("a", &[("i", fr(1.0 / 3.0, 0.1234567, 2.0 / 3.0, 0.5))]);
        let p = build_payload(&[], &[l], "r1", &monitors()).unwrap_or_default();
        let r = p.layouts.unwrap_or_default()[0].islands[0].rect;
        assert_eq!((r.x, r.y, r.h), (0.333333, 0.123457, 0.5));
        assert!(decimals_ok(r.w) && r.x + r.w <= 1.0);
        let json = serde_json::to_string(&r).unwrap_or_default();
        assert!(!json.contains("px"), "{json}");
    }

    #[test]
    fn monitor_selector_is_kept() {
        let mut l = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        l.monitor = MonitorSel::Index(0);
        let p = build_payload(&[], &[l], "r1", &monitors()).unwrap_or_default();
        assert_eq!(
            p.layouts.unwrap_or_default()[0].monitor,
            MonitorSel::Index(0)
        );
    }

    // ---------------------------------------------------------- rebase

    #[test]
    fn untouched_layout_adopts_the_new_version() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut a_new = a.clone();
        a_new.islands[0].rect.w = 0.25;
        let old = std::slice::from_ref(&a);
        let (d, flagged) = rebase(old, std::slice::from_ref(&a_new), old);
        assert_eq!((d, flagged), (vec![a_new], vec![]));
    }

    #[test]
    fn untouched_layout_deleted_outside_disappears() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let old = std::slice::from_ref(&a);
        assert_eq!(rebase(old, &[], old), (vec![], vec![]));
    }

    #[test]
    fn modified_layout_is_kept_and_flagged_when_changed_outside() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let b = lay("b", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut mine = a.clone();
        mine.islands[0].rect.x = 0.5;
        let mut theirs = a.clone();
        theirs.islands[0].rect.w = 0.25;
        let mut b_mine = b.clone();
        b_mine.islands[0].rect.h = 0.5;
        let (d, flagged) = rebase(
            &[a.clone(), b.clone()],
            &[theirs, b],
            &[mine.clone(), b_mine.clone()],
        );
        assert_eq!(d, vec![mine, b_mine]);
        assert_eq!(flagged, vec!["a".to_string()], "b changed only here");
    }

    #[test]
    fn modified_layout_deleted_outside_is_kept_and_flagged() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut mine = a.clone();
        mine.islands[0].rect.x = 0.5;
        let (d, flagged) = rebase(&[a], &[], &[mine.clone()]);
        assert_eq!((d, flagged), (vec![mine], vec!["a".to_string()]));
    }

    #[test]
    fn layout_added_outside_appears() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let n = lay("n", &[("i", fr(0.5, 0.0, 0.5, 1.0))]);
        let old = std::slice::from_ref(&a);
        let (d, flagged) = rebase(old, &[a.clone(), n.clone()], old);
        assert_eq!(
            (d, flagged),
            (vec![lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]), n], vec![])
        );
    }

    #[test]
    fn same_id_added_on_both_sides_keeps_mine_and_flags_it() {
        let mine = lay("n", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let theirs = lay("n", &[("i", fr(0.5, 0.0, 0.5, 1.0))]);
        let (d, flagged) = rebase(&[], &[theirs], std::slice::from_ref(&mine));
        assert_eq!((d, flagged), (vec![mine], vec!["n".to_string()]));
    }

    #[test]
    fn layout_the_user_deleted_stays_deleted() {
        let a = lay("a", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let b = lay("b", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let (d, flagged) = rebase(
            &[a.clone(), b.clone()],
            &[a.clone(), b.clone()],
            std::slice::from_ref(&a),
        );
        assert_eq!((d.clone(), flagged), (vec![a.clone()], vec![]));
        // The next save still removes it.
        let p = build_payload(&[a, b], &d, "r2", &monitors()).unwrap_or_default();
        assert_eq!(p.remove_layouts, vec!["b".to_string()]);
    }

    #[test]
    fn deleted_here_and_changed_outside_stays_deleted_but_is_flagged() {
        let b = lay("b", &[("i", fr(0.0, 0.0, 0.5, 1.0))]);
        let mut theirs = b.clone();
        theirs.islands[0].rect.w = 0.25;
        assert_eq!(
            rebase(&[b], &[theirs], &[]),
            (vec![], vec!["b".to_string()])
        );
    }

    // ---------------------------------------------------------- properties

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0xB05EED),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    /// A fraction rect inside the unit square (positive size).
    fn inside_rect() -> impl Strategy<Value = IslandRect> {
        (0.0..0.99f64, 0.0..0.99f64, 0.0..1.0f64, 0.0..1.0f64).prop_map(|(x, y, a, b)| {
            let w = (1.0 - x) * a.max(0.001);
            let h = (1.0 - y) * b.max(0.001);
            fr(x, y, w, h)
        })
    }

    fn px_inside(mw: i32, mh: i32) -> impl Strategy<Value = PxRect> {
        (0..mw, 0..mh).prop_flat_map(move |(x, y)| {
            (Just(x), Just(y), 1..=mw - x, 1..=mh - y).prop_map(|(x, y, w, h)| px(x, y, w, h))
        })
    }

    fn any_layout(id: &'static str) -> impl Strategy<Value = Layout> {
        let rect = prop_oneof![
            3 => (0.0..1.0f64, 0.0..1.0f64, 0.0..1.0f64, 0.0..1.0f64)
                .prop_map(|(x, y, w, h)| fr(x, y, w, h)),
            1 => (0..3000i32, 0..1500i32, 1..3000i32, 1..1500i32).prop_map(|(x, y, w, h)| IslandRect {
                x: x.into(),
                y: y.into(),
                w: w.into(),
                h: h.into(),
                unit: Unit::Px,
            }),
        ];
        let monitor = prop_oneof![
            Just(MonitorSel::Primary),
            Just(MonitorSel::Index(0)),
            Just(MonitorSel::Index(5)),
        ];
        (monitor, proptest::collection::vec(rect, 0..4)).prop_map(move |(monitor, rects)| Layout {
            id: id.into(),
            monitor,
            islands: rects
                .into_iter()
                .enumerate()
                .map(|(i, rect)| Island {
                    id: format!("i{i}"),
                    rect,
                    extra: Default::default(),
                })
                .collect(),
            extra: Default::default(),
        })
    }

    /// Up to four layouts with distinct ids, in a shuffled order.
    fn layouts() -> impl Strategy<Value = Vec<Layout>> {
        (
            proptest::option::of(any_layout("a")),
            proptest::option::of(any_layout("b")),
            proptest::option::of(any_layout("c")),
            proptest::option::of(any_layout("d")),
        )
            .prop_map(|(a, b, c, d)| [a, b, c, d].into_iter().flatten().collect::<Vec<_>>())
            .prop_shuffle()
    }

    fn by_id(mut v: Vec<Layout>, monitors: &[MonitorInfo]) -> Vec<Layout> {
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v.iter().map(|l| normalize_layout(l, monitors)).collect()
    }

    proptest! {
        #![proptest_config(cfg())]

        // (1) fraction -> px -> fraction resolves to the same px, for unchanged and new targets.
        #[test]
        fn prop_rect_from_px_resolves_to_its_target(
            w in prop::sample::select(vec![1920, 2560, 2561, 3440, 5120, 7680]),
            h in prop::sample::select(vec![1080, 1440, 1600, 2160]),
            r in inside_rect(),
            t in px_inside(7680, 2160),
        ) {
            let p = island_px(&r, w, h);
            prop_assert_eq!(island_px(&rect_from_px(&r, p, w, h), w, h), p);
            let t = PxRect {
                x: t.x.min(w - 1),
                y: t.y.min(h - 1),
                w: t.w.min(w - t.x.min(w - 1)),
                h: t.h.min(h - t.y.min(h - 1)),
            };
            let got = rect_from_px(&r, t, w, h);
            prop_assert_eq!(island_px(&got, w, h), t);
            prop_assert!(got.x + got.w <= 1.0 && got.y + got.h <= 1.0, "{:?}", got);
        }

        // (2) the server's upsert-then-remove of the payload yields the draft; x -> x is None.
        #[test]
        fn prop_payload_applied_by_the_server_yields_the_draft(
            saved in layouts(),
            draft in layouts(),
        ) {
            let monitors = monitors();
            prop_assert_eq!(build_payload(&draft, &draft, "r", &monitors), None);
            let mut file = ProfileFile { version: 2, layouts: saved.clone(), profiles: vec![], extra: Default::default() };
            if let Some(p) = build_payload(&saved, &draft, "r", &monitors) {
                prop_assert_eq!(p.expected_revision.as_deref(), Some("r"));
                upsert(&mut file, vec![], p.layouts.unwrap_or_default(), vec![], p.remove_layouts, p.replace_extras);
            }
            prop_assert_eq!(by_id(file.layouts, &monitors), by_id(draft, &monitors));
        }

        // (3) drags never leave the monitor nor go under the minimum size.
        #[test]
        fn prop_drag_stays_inside_with_minimum_size(
            (mw, mh) in (64..8000i32, 64..4000i32),
            start in (-10_000..10_000i32, -10_000..10_000i32, -100..10_000i32, -100..10_000i32),
            (dx, dy) in (any::<i32>(), any::<i32>()),
            handle in prop::sample::select(vec![
                Handle::N, Handle::NE, Handle::E, Handle::SE,
                Handle::S, Handle::SW, Handle::W, Handle::NW,
            ]),
            snap in proptest::option::of((
                proptest::collection::vec(-100..9000i32, 0..8),
                proptest::collection::vec(-100..5000i32, 0..8),
                0.0..50.0f64,
            )),
        ) {
            let start = px(start.0, start.1, start.2, start.3);
            let targets = snap.as_ref().map(|(xs, ys, _)| SnapTargets { xs: xs.clone(), ys: ys.clone() });
            let snap = targets.as_ref().zip(snap.as_ref().map(|s| s.2));
            for (r, _) in [
                move_rect(start, dx, dy, mw, mh, snap),
                resize_rect(start, handle, dx, dy, mw, mh, snap),
            ] {
                prop_assert!(r.x >= 0 && r.y >= 0 && r.right() <= mw && r.bottom() <= mh, "{:?}", r);
                prop_assert!(r.w >= MIN_SIZE && r.h >= MIN_SIZE, "{:?}", r);
            }
        }
    }
}
