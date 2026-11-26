//! Preview of the desktop for the Islas view and the island editor: pure geometry (monitor
//! rectangles scaled to fit a box, hit-testing, labels) plus the [`MonitorPreview`] component
//! that draws monitors and islands to scale.
//!
//! One scale is shared by every monitor (a 5120x1440 ultrawide next to a 1080p panel keeps its
//! real proportions). Physical pixels are the unit of the engine; preview pixels are GPUI
//! logical pixels relative to the preview's top-left corner.

use std::rc::Rc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, pattern_slash, px,
};

use super::theme::{
    BadgeKind, R_SM, SP_1, TEXT_SM, accent, badge, monitor_frame, tabular, text, text_muted, warn,
};
use crate::i18n::{fmt_decimal, tf, tn};
use crate::window_state::Rect;

// ------------------------------------------------------------------ pure geometry

/// Rectangle in preview (logical) pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PRect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Half-open: the left/top edge belongs to the rectangle, the right/bottom one does not.
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }
}

/// Monitors scaled into a box. `monitors` are relative to the virtual-desktop bounding box.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopLayout {
    /// Preview px per physical px; never above 1.0.
    pub scale: f64,
    /// Top-left corner of the bounding box in physical desktop coordinates.
    pub origin: (i32, i32),
    /// Size of the preview in logical px.
    pub size: (f32, f32),
    pub monitors: Vec<PRect>,
}

/// Fits the bounding box of `monitors` in `max_w` x `max_h` keeping the aspect ratio. Empty or
/// degenerate input gives scale 1.0.
pub fn desktop_layout(monitors: &[Rect], max_w: f32, max_h: f32) -> DesktopLayout {
    let (Some(left), Some(top), Some(right), Some(bottom)) = (
        monitors.iter().map(|m| m.left).min(),
        monitors.iter().map(|m| m.top).min(),
        monitors.iter().map(|m| m.right).max(),
        monitors.iter().map(|m| m.bottom).max(),
    ) else {
        return DesktopLayout {
            scale: 1.0,
            origin: (0, 0),
            size: (0.0, 0.0),
            monitors: Vec::new(),
        };
    };
    let (bw, bh) = (
        f64::from(right) - f64::from(left),
        f64::from(bottom) - f64::from(top),
    );
    let scale = if bw <= 0.0 || bh <= 0.0 {
        1.0
    } else {
        // The floor keeps `point_to_phys` finite when the box is degenerate.
        (f64::from(max_w) / bw)
            .min(f64::from(max_h) / bh)
            .clamp(1e-6, 1.0)
    };
    let mut layout = DesktopLayout {
        scale,
        origin: (left, top),
        size: ((bw * scale) as f32, (bh * scale) as f32),
        monitors: Vec::new(),
    };
    layout.monitors = monitors
        .iter()
        .map(|m| layout.rect_to_preview(*m))
        .collect();
    layout
}

impl DesktopLayout {
    /// Physical desktop rectangle to preview px.
    pub fn rect_to_preview(&self, r: Rect) -> PRect {
        let s = self.scale;
        PRect {
            x: ((f64::from(r.left) - f64::from(self.origin.0)) * s) as f32,
            y: ((f64::from(r.top) - f64::from(self.origin.1)) * s) as f32,
            w: ((f64::from(r.right) - f64::from(r.left)) * s) as f32,
            h: ((f64::from(r.bottom) - f64::from(r.top)) * s) as f32,
        }
    }

    /// Preview point to physical desktop coordinates.
    pub fn point_to_phys(&self, px: f32, py: f32) -> (f64, f64) {
        (
            f64::from(self.origin.0) + f64::from(px) / self.scale,
            f64::from(self.origin.1) + f64::from(py) / self.scale,
        )
    }

    /// Preview length to physical px.
    pub fn len_to_phys(&self, len: f32) -> f64 {
        f64::from(len) / self.scale
    }
}

/// Widest the preview may be inside the shell: viewport minus sidebar (232), shell padding and
/// gap (24 + 12), content padding (64) and card padding (32); never under 320.
pub fn preview_max_width(viewport_w: f32) -> f32 {
    (viewport_w - 232.0 - 24.0 - 12.0 - 64.0 - 32.0).max(320.0)
}

/// Snap distance in desktop physical px for `screen_px` physical px of the panel's monitor:
/// GPUI logical px = physical / window scale factor, then preview px -> desktop px.
pub fn snap_tolerance_phys(screen_px: f64, window_scale_factor: f64, preview_scale: f64) -> f64 {
    screen_px / (window_scale_factor * preview_scale)
}

/// Intersection of two preview rectangles when its area is positive.
pub fn overlap_rect(a: PRect, b: PRect) -> Option<PRect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (r, bt) = (a.right().min(b.right()), a.bottom().min(b.bottom()));
    (r > x && bt > y).then_some(PRect {
        x,
        y,
        w: r - x,
        h: bt - y,
    })
}

// ------------------------------------------------------------------ hit-testing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    NW,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Handle(Handle),
    Body,
}

/// Resize handles in priority order: corners first, then edge midpoints.
const HANDLES: [Handle; 8] = [
    Handle::NW,
    Handle::NE,
    Handle::SE,
    Handle::SW,
    Handle::N,
    Handle::E,
    Handle::S,
    Handle::W,
];

fn handle_center(r: PRect, h: Handle) -> (f32, f32) {
    let (mx, my) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    match h {
        Handle::N => (mx, r.y),
        Handle::NE => (r.right(), r.y),
        Handle::E => (r.right(), my),
        Handle::SE => (r.right(), r.bottom()),
        Handle::S => (mx, r.bottom()),
        Handle::SW => (r.x, r.bottom()),
        Handle::W => (r.x, my),
        Handle::NW => (r.x, r.y),
    }
}

/// Handles are squares of half-side `half` centred on the corners and edge midpoints (they
/// straddle the border). Corners beat edges, handles beat the body.
pub fn hit_test(rect: PRect, p: (f32, f32), half: f32) -> Option<Hit> {
    HANDLES
        .iter()
        .find(|&&h| {
            let c = handle_center(rect, h);
            (p.0 - c.0).abs() <= half && (p.1 - c.1).abs() <= half
        })
        .map(|&h| Hit::Handle(h))
        .or_else(|| rect.contains(p).then_some(Hit::Body))
}

/// Island under the pointer: handles only on the `selected` island; otherwise bodies from the
/// topmost (last drawn) to the bottom one.
pub fn pick(
    islands: &[PRect],
    selected: Option<usize>,
    p: (f32, f32),
    half: f32,
) -> Option<(usize, Hit)> {
    if let Some(i) = selected
        && let Some(r) = islands.get(i)
        && let Some(hit @ Hit::Handle(_)) = hit_test(*r, p, half)
    {
        return Some((i, hit));
    }
    islands
        .iter()
        .rposition(|r| r.contains(p))
        .map(|i| (i, Hit::Body))
}

// ------------------------------------------------------------------ labels and colours

/// Island colour: the accent with its hue rotated per index (no invented palette).
pub fn island_color(idx: usize) -> Hsla {
    let a = accent();
    Hsla {
        h: (a.h + idx as f32 * 0.17).rem_euclid(1.0),
        ..a
    }
}

/// Odd islands get a dashed border so colour is never the only difference.
pub fn island_dashed(idx: usize) -> bool {
    idx % 2 == 1
}

fn percent(v: f64) -> String {
    let p = (v * 1000.0).round() / 10.0;
    if (p - p.round()).abs() < 1e-9 {
        format!("{p:.0}")
    } else {
        fmt_decimal(p, 1)
    }
}

/// "50% × 100%" (es: "50 % × 100 %"): the island's size as a fraction of its monitor.
pub fn fraction_label(rect: Rect, monitor: Rect) -> String {
    let frac = |n: i32, d: i32| {
        if d > 0 {
            f64::from(n) / f64::from(d)
        } else {
            0.0
        }
    };
    tf(
        "preview.fraction",
        &[
            ("w", &percent(frac(rect.width(), monitor.width()))),
            ("h", &percent(frac(rect.height(), monitor.height()))),
        ],
    )
}

/// "2560×1440 px".
pub fn format_size(w: i32, h: i32) -> String {
    format!("{w}×{h} px")
}

/// Case-insensitive substring match on process or title; an empty query matches everything.
pub fn window_matches(query: &str, process: &str, title: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || process.to_lowercase().contains(&q) || title.to_lowercase().contains(&q)
}

/// Occupant row text: "process — title", whichever is known, else "Window {hwnd}".
pub fn occupant_label(hwnd: isize, process: Option<&str>, title: Option<&str>) -> String {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    match (clean(process), clean(title)) {
        (Some(p), Some(t)) => format!("{p} — {t}"),
        (Some(s), None) | (None, Some(s)) => s.to_owned(),
        (None, None) => tf("preview.window_fallback", &[("hwnd", &hwnd)]),
    }
}

/// Processes shown on an island: first three, then "+n".
pub fn occupant_summary(processes: &[SharedString]) -> String {
    let shown: Vec<&str> = processes.iter().take(3).map(|s| s.as_ref()).collect();
    match processes.len().saturating_sub(3) {
        0 => shown.join(", "),
        n => format!("{} +{n}", shown.join(", ")),
    }
}

fn occupants_text(n: usize) -> String {
    tn("preview.occupants", n as u64, &[])
}

/// How much text fits in an island of this preview size: 0 none, 1 name, 2 + size, 3 + occupants.
fn detail_level(r: PRect) -> u8 {
    match (r.w >= 48.0, r.h) {
        (false, _) => 0,
        (true, h) if h < 20.0 => 0,
        (true, h) if h < 40.0 => 1,
        (true, h) if h < 64.0 => 2,
        _ => 3,
    }
}

// ------------------------------------------------------------------ component

/// Monitor to draw (physical desktop rectangle).
#[derive(Debug, Clone)]
pub struct PreviewMonitor {
    pub rect: Rect,
    pub label: SharedString,
}

/// Island to draw (physical desktop rectangle, already resolved on its monitor).
#[derive(Debug, Clone)]
pub struct PreviewIsland {
    pub rect: Rect,
    pub name: SharedString,
    /// Process of each occupant window.
    pub occupants: Vec<SharedString>,
    /// Colour/border index: islands of one layout use their own position.
    pub color_idx: usize,
}

type ClickFn = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Draws monitors and islands to scale. Islands are clickable (and Enter/Space-activable) when
/// `on_click` is set; the editor leaves it unset and lays its own `overlay` on top.
#[derive(IntoElement)]
pub struct MonitorPreview {
    layout: DesktopLayout,
    monitors: Vec<PreviewMonitor>,
    islands: Vec<PreviewIsland>,
    selected: Option<usize>,
    grid: bool,
    /// Snap guides: desktop physical x / y of the guide lines.
    guides: (Option<i32>, Option<i32>),
    on_click: Option<ClickFn>,
    overlay: Option<AnyElement>,
}

impl MonitorPreview {
    /// `layout` must come from [`desktop_layout`] over the same monitors.
    pub fn new(
        layout: DesktopLayout,
        monitors: Vec<PreviewMonitor>,
        islands: Vec<PreviewIsland>,
    ) -> Self {
        Self {
            layout,
            monitors,
            islands,
            selected: None,
            grid: false,
            guides: (None, None),
            on_click: None,
            overlay: None,
        }
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    /// Thin lines at 1/2, 1/3, 2/3, 1/4 and 3/4 of every monitor.
    pub fn grid(mut self, grid: bool) -> Self {
        self.grid = grid;
        self
    }

    pub fn guides(mut self, x: Option<i32>, y: Option<i32>) -> Self {
        self.guides = (x, y);
        self
    }

    pub fn on_click(mut self, f: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    /// Element laid over everything (absolute, full size), e.g. the editor's mouse canvas.
    pub fn overlay(mut self, overlay: impl IntoElement) -> Self {
        self.overlay = Some(overlay.into_any_element());
        self
    }
}

fn line(color: Hsla) -> gpui_kit::Div {
    div().absolute().bg(color)
}

impl RenderOnce for MonitorPreview {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            layout,
            monitors,
            islands,
            selected,
            grid,
            guides,
            on_click,
            overlay,
        } = self;
        let prects: Vec<PRect> = islands
            .iter()
            .map(|i| layout.rect_to_preview(i.rect))
            .collect();
        let grid_color = text_muted().opacity(0.18);

        let mut root = div().relative().w(px(layout.size.0)).h(px(layout.size.1));

        for (m, r) in monitors.into_iter().zip(&layout.monitors) {
            root = root.child(
                monitor_frame(m.label)
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h)),
            );
            if grid {
                for f in [1.0 / 4.0, 1.0 / 3.0, 1.0 / 2.0, 2.0 / 3.0, 3.0 / 4.0] {
                    root = root
                        .child(
                            line(grid_color)
                                .left(px(r.x + r.w * f))
                                .top(px(r.y))
                                .w(px(1.))
                                .h(px(r.h)),
                        )
                        .child(
                            line(grid_color)
                                .left(px(r.x))
                                .top(px(r.y + r.h * f))
                                .w(px(r.w))
                                .h(px(1.)),
                        );
                }
            }
        }

        for (i, (isl, r)) in islands.into_iter().zip(&prects).enumerate() {
            let color = island_color(isl.color_idx);
            let is_sel = selected == Some(i);
            let level = detail_level(*r);
            let size = format_size(isl.rect.width(), isl.rect.height());
            let n = isl.occupants.len();
            let click = on_click.clone();
            root = root.child(
                div()
                    .id(("island", i))
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(SP_1)
                    .p(SP_1)
                    .rounded(R_SM)
                    .bg(color.opacity(if is_sel { 0.36 } else { 0.22 }))
                    .border_color(color)
                    .when(is_sel, |d| d.border_2())
                    .when(!is_sel, |d| d.border_1())
                    .when(island_dashed(isl.color_idx), |d| d.border_dashed())
                    .text_size(TEXT_SM)
                    .text_color(text())
                    .when(level >= 1, |d| d.child(div().truncate().child(isl.name)))
                    .when(level >= 2, |d| {
                        d.child(
                            div()
                                .truncate()
                                .text_color(text_muted())
                                .font_features(tabular())
                                .child(size),
                        )
                    })
                    .when(level >= 3 && n > 0, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(SP_1)
                                .child(badge(occupants_text(n), BadgeKind::Info).self_start())
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(text_muted())
                                        .child(occupant_summary(&isl.occupants)),
                                ),
                        )
                    })
                    .when_some(click, |d, f| {
                        d.focusable()
                            .tab_stop(true)
                            .cursor_pointer()
                            .focus_visible(|s| s.border_2().border_color(accent()))
                            .on_click(move |_, window, cx| f(i, window, cx))
                    }),
            );
        }

        // Overlaps: warn tint with a hatch, drawn above every island.
        for (i, a) in prects.iter().enumerate() {
            for b in &prects[i + 1..] {
                if let Some(o) = overlap_rect(*a, *b) {
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(o.x))
                            .top(px(o.y))
                            .w(px(o.w))
                            .h(px(o.h))
                            .rounded(R_SM)
                            .bg(pattern_slash(warn().opacity(0.6), 1.0, 6.0))
                            .border_1()
                            .border_color(warn()),
                    );
                }
            }
        }

        let guide_color = accent().opacity(0.8);
        if let Some(gx) = guides.0 {
            let x = ((f64::from(gx) - f64::from(layout.origin.0)) * layout.scale) as f32;
            root = root.child(
                line(guide_color)
                    .left(px(x))
                    .top(px(0.))
                    .w(px(1.))
                    .h(px(layout.size.1)),
            );
        }
        if let Some(gy) = guides.1 {
            let y = ((f64::from(gy) - f64::from(layout.origin.1)) * layout.scale) as f32;
            root = root.child(
                line(guide_color)
                    .left(px(0.))
                    .top(px(y))
                    .w(px(layout.size.0))
                    .h(px(1.)),
            );
        }

        root.when_some(overlay, |d, o| d.child(o))
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Locale, scoped_locale};
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn pr(x: f32, y: f32, w: f32, h: f32) -> PRect {
        PRect { x, y, w, h }
    }

    #[test]
    fn ultrawide_fills_the_width() {
        let l = desktop_layout(&[rect(0, 0, 5120, 1440)], 900.0, 420.0);
        assert!((l.scale - 900.0 / 5120.0).abs() < 1e-12);
        assert_eq!(l.size.0, 900.0);
        assert!((l.size.1 - 1440.0 * 900.0 / 5120.0).abs() < 1e-3);
        assert_eq!(l.monitors, vec![pr(0.0, 0.0, l.size.0, l.size.1)]);
    }

    #[test]
    fn tall_desktop_is_limited_by_height() {
        let l = desktop_layout(&[rect(0, 0, 1080, 1920)], 900.0, 420.0);
        assert!((l.scale - 420.0 / 1920.0).abs() < 1e-12);
        assert_eq!(l.size.1, 420.0);
    }

    #[test]
    fn scale_never_exceeds_one() {
        let l = desktop_layout(&[rect(0, 0, 400, 300)], 900.0, 420.0);
        assert_eq!(l.scale, 1.0);
        assert_eq!(l.size, (400.0, 300.0));
    }

    #[test]
    fn odd_width_stays_inside_the_box() {
        let l = desktop_layout(&[rect(0, 0, 2561, 1440)], 900.0, 420.0);
        assert!(l.size.0 <= 900.0 + 1e-3);
        assert!(l.monitors[0].right() <= l.size.0 + 1e-3);
        assert!((l.scale - 420.0 / 1440.0).abs() < 1e-12);
    }

    #[test]
    fn negative_x_monitor_is_relative_to_the_bounding_box() {
        let l = desktop_layout(
            &[rect(-1920, 0, 0, 1080), rect(0, 0, 2560, 1440)],
            900.0,
            420.0,
        );
        assert_eq!(l.origin, (-1920, 0));
        assert_eq!(l.monitors[0].x, 0.0);
        let s = l.scale as f32;
        assert!((l.monitors[1].x - 1920.0 * s).abs() < 1e-3);
        assert!((l.monitors[1].w - 2560.0 * s).abs() < 1e-3);
        let (px, _) = l.point_to_phys(l.monitors[1].x, 0.0);
        assert!((px - 0.0).abs() < 1.0 / l.scale);
    }

    #[test]
    fn empty_and_degenerate_desktops_scale_to_one() {
        assert_eq!(desktop_layout(&[], 900.0, 420.0).scale, 1.0);
        assert_eq!(desktop_layout(&[rect(5, 5, 5, 5)], 900.0, 420.0).scale, 1.0);
    }

    #[test]
    fn one_axis_degenerate_desktop_scales_to_one() {
        // Without the guard, 420/1000 (or 900/1000) would give a scale below 1.
        assert_eq!(
            desktop_layout(&[rect(0, 0, 0, 1000)], 900.0, 420.0).scale,
            1.0
        );
        assert_eq!(
            desktop_layout(&[rect(0, 0, 1000, 0)], 900.0, 420.0).scale,
            1.0
        );
    }

    #[test]
    fn len_to_phys_inverts_the_scale() {
        let l = desktop_layout(&[rect(0, 0, 5120, 1440)], 900.0, 420.0);
        assert!((l.len_to_phys(900.0) - 5120.0).abs() < 1e-6);
    }

    #[test]
    fn preview_width_follows_the_shell_and_has_a_floor() {
        assert_eq!(preview_max_width(1600.0), 1600.0 - 364.0);
        assert_eq!(preview_max_width(600.0), 320.0);
    }

    #[test]
    fn snap_tolerance_is_eight_physical_screen_px() {
        // 100 %: 8 logical px; at preview scale 0.5 that is 16 desktop px.
        assert_eq!(snap_tolerance_phys(8.0, 1.0, 0.5), 16.0);
        assert_eq!(snap_tolerance_phys(8.0, 1.25, 0.5), 12.8);
        assert_eq!(snap_tolerance_phys(8.0, 1.5, 0.5), 8.0 / 0.75);
    }

    #[test]
    fn overlap_needs_positive_area() {
        let a = pr(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            overlap_rect(a, pr(5.0, 5.0, 10.0, 10.0)),
            Some(pr(5.0, 5.0, 5.0, 5.0))
        );
        assert_eq!(overlap_rect(a, pr(10.0, 0.0, 5.0, 5.0)), None);
        assert_eq!(overlap_rect(a, pr(20.0, 20.0, 5.0, 5.0)), None);
    }

    #[test]
    fn overlap_rect_touching_contained_disjoint() {
        let a = pr(0.0, 0.0, 10.0, 10.0);
        // Touching on an edge or a corner has zero area.
        assert_eq!(overlap_rect(a, pr(0.0, 10.0, 10.0, 5.0)), None);
        assert_eq!(overlap_rect(a, pr(-5.0, 0.0, 5.0, 10.0)), None);
        assert_eq!(overlap_rect(a, pr(10.0, 10.0, 5.0, 5.0)), None);
        // Contained (either way round) gives the inner rectangle.
        let inner = pr(2.0, 3.0, 4.0, 5.0);
        assert_eq!(overlap_rect(a, inner), Some(inner));
        assert_eq!(overlap_rect(inner, a), Some(inner));
        assert_eq!(overlap_rect(a, a), Some(a));
        // Disjoint on one axis only still has no overlap.
        assert_eq!(overlap_rect(a, pr(2.0, 11.0, 4.0, 4.0)), None);
    }

    #[test]
    fn contains_is_half_open() {
        let r = pr(10.0, 10.0, 5.0, 5.0);
        assert!(r.contains((10.0, 12.0)));
        assert!(r.contains((12.0, 10.0)));
        assert!(!r.contains((r.right(), 12.0)));
        assert!(!r.contains((12.0, r.bottom())));
    }

    #[test]
    fn pick_follows_half_open_edges() {
        let islands = [pr(10.0, 10.0, 5.0, 5.0)];
        // Half 0.5 keeps the edge points out of every handle.
        assert_eq!(
            pick(&islands, None, (10.0, 12.0), 0.5),
            Some((0, Hit::Body))
        );
        assert_eq!(
            pick(&islands, None, (12.0, 10.0), 0.5),
            Some((0, Hit::Body))
        );
        assert_eq!(pick(&islands, None, (15.0, 12.0), 0.5), None);
        assert_eq!(pick(&islands, None, (12.0, 15.0), 0.5), None);
    }

    const HALF: f32 = 8.0;

    #[test]
    fn all_eight_handles_hit() {
        let r = pr(100.0, 100.0, 200.0, 100.0);
        let cases = [
            ((100.0, 100.0), Handle::NW),
            ((300.0, 100.0), Handle::NE),
            ((300.0, 200.0), Handle::SE),
            ((100.0, 200.0), Handle::SW),
            ((200.0, 100.0), Handle::N),
            ((300.0, 150.0), Handle::E),
            ((200.0, 200.0), Handle::S),
            ((100.0, 150.0), Handle::W),
        ];
        for (p, h) in cases {
            assert_eq!(hit_test(r, p, HALF), Some(Hit::Handle(h)), "{h:?}");
        }
    }

    #[test]
    fn handles_straddle_the_border_and_stop_at_half() {
        let r = pr(100.0, 100.0, 200.0, 100.0);
        assert_eq!(
            hit_test(r, (92.0, 92.0), HALF),
            Some(Hit::Handle(Handle::NW))
        );
        assert_eq!(hit_test(r, (91.9, 100.0), HALF), None);
    }

    #[test]
    fn body_and_outside() {
        let r = pr(100.0, 100.0, 200.0, 100.0);
        assert_eq!(hit_test(r, (200.0, 150.0), HALF), Some(Hit::Body));
        assert_eq!(hit_test(r, (50.0, 50.0), HALF), None);
    }

    #[test]
    fn corner_beats_edge_when_handles_overlap() {
        // 12 px wide: the N handle (centre x=106) overlaps the NW corner handle.
        let r = pr(100.0, 100.0, 12.0, 100.0);
        assert_eq!(
            hit_test(r, (104.0, 100.0), HALF),
            Some(Hit::Handle(Handle::NW))
        );
    }

    #[test]
    fn pick_prefers_handle_of_selected_over_island_above() {
        let islands = [pr(0.0, 0.0, 100.0, 100.0), pr(50.0, 50.0, 100.0, 100.0)];
        // (100,100) is the SE corner of island 0 and inside the body of island 1 (on top).
        assert_eq!(
            pick(&islands, Some(0), (100.0, 100.0), HALF),
            Some((0, Hit::Handle(Handle::SE)))
        );
        assert_eq!(
            pick(&islands, None, (100.0, 100.0), HALF),
            Some((1, Hit::Body))
        );
        assert_eq!(
            pick(&islands, Some(1), (100.0, 100.0), HALF),
            Some((1, Hit::Body))
        );
    }

    #[test]
    fn pick_topmost_body_wins_and_misses_are_none() {
        let islands = [pr(0.0, 0.0, 100.0, 100.0), pr(50.0, 50.0, 100.0, 100.0)];
        assert_eq!(
            pick(&islands, None, (75.0, 75.0), HALF),
            Some((1, Hit::Body))
        );
        assert_eq!(
            pick(&islands, None, (10.0, 10.0), HALF),
            Some((0, Hit::Body))
        );
        assert_eq!(pick(&islands, None, (400.0, 400.0), HALF), None);
        assert_eq!(
            pick(&islands, Some(7), (10.0, 10.0), HALF),
            Some((0, Hit::Body))
        );
    }

    #[test]
    fn colors_rotate_the_accent_hue() {
        assert_eq!(island_color(0), accent());
        let c1 = island_color(1);
        assert_ne!(c1.h, accent().h);
        assert_eq!((c1.s, c1.l, c1.a), (accent().s, accent().l, accent().a));
        assert!((0.0..1.0).contains(&island_color(40).h));
        assert!(!island_dashed(0) && island_dashed(1) && !island_dashed(2));
    }

    #[test]
    fn fraction_labels() {
        let _g = scoped_locale(Locale::En);
        let mon = rect(0, 0, 2560, 1440);
        assert_eq!(fraction_label(rect(0, 0, 1280, 1440), mon), "50% × 100%");
        assert_eq!(fraction_label(rect(0, 0, 853, 360), mon), "33.3% × 25%");
        assert_eq!(
            fraction_label(rect(0, 0, 10, 10), rect(0, 0, 0, 0)),
            "0% × 0%"
        );
    }

    #[test]
    fn fraction_labels_in_spanish_keep_the_decimal_comma_and_spaced_percent() {
        let _g = scoped_locale(Locale::Es);
        let mon = rect(0, 0, 2560, 1440);
        assert_eq!(fraction_label(rect(0, 0, 1280, 1440), mon), "50 % × 100 %");
        assert_eq!(fraction_label(rect(0, 0, 853, 360), mon), "33,3 % × 25 %");
    }

    #[test]
    fn size_label() {
        assert_eq!(format_size(2560, 1440), "2560×1440 px");
    }

    #[test]
    fn window_match_is_case_insensitive_on_process_and_title() {
        assert!(window_matches("NOTE", "notepad.exe", "Untitled"));
        assert!(window_matches("unt", "notepad.exe", "Untitled"));
        assert!(window_matches("  ", "a", "b"));
        assert!(!window_matches("zzz", "notepad.exe", "Untitled"));
    }

    #[test]
    fn occupant_labels() {
        let _g = scoped_locale(Locale::En);
        assert_eq!(
            occupant_label(7, Some("game.exe"), Some("Game")),
            "game.exe — Game"
        );
        assert_eq!(occupant_label(7, Some("game.exe"), Some(" ")), "game.exe");
        assert_eq!(occupant_label(7, None, Some("Game")), "Game");
        assert_eq!(occupant_label(7, None, None), "Window 7");
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        let es = occupant_label(7, None, None);
        assert_ne!(es, "Window 7");
        assert!(es.ends_with(" 7"), "{es}");
    }

    #[test]
    fn occupant_summary_caps_at_three() {
        let _g = scoped_locale(Locale::En);
        let v: Vec<SharedString> = ["a", "b", "c", "d", "e"].map(Into::into).to_vec();
        assert_eq!(occupant_summary(&v[..2]), "a, b");
        assert_eq!(occupant_summary(&v), "a, b, c +2");
        assert_eq!(occupants_text(1), "1 window");
        assert_eq!(occupants_text(3), "3 windows");
        drop(_g);
        let _g = scoped_locale(Locale::Es);
        let (one, many) = (occupants_text(1), occupants_text(3));
        assert!(one.starts_with("1 ") && many.starts_with("3 "));
        assert!(!one.contains("window") && !many.contains("window"));
        assert_ne!(one, many);
    }

    #[test]
    fn detail_levels_follow_the_size() {
        assert_eq!(detail_level(pr(0.0, 0.0, 30.0, 100.0)), 0);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 10.0)), 0);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 30.0)), 1);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 50.0)), 2);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 80.0)), 3);
    }

    #[test]
    fn detail_level_boundaries() {
        assert_eq!(detail_level(pr(0.0, 0.0, 48.0, 100.0)), 3);
        assert_eq!(detail_level(pr(0.0, 0.0, 47.99, 100.0)), 0);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 19.99)), 0);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 20.0)), 1);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 40.0)), 2);
        assert_eq!(detail_level(pr(0.0, 0.0, 100.0, 64.0)), 3);
    }

    fn cfg() -> ProptestConfig {
        ProptestConfig {
            rng_seed: RngSeed::Fixed(0xB05EED),
            failure_persistence: None,
            ..ProptestConfig::default()
        }
    }

    fn rect_strategy() -> impl Strategy<Value = Rect> {
        (-8000i32..8000, -4000i32..4000, 200i32..8000, 200i32..4000)
            .prop_map(|(l, t, w, h)| rect(l, t, l + w, t + h))
    }

    proptest! {
        #![proptest_config(cfg())]

        // (1) a monitor's top-left survives the round trip to preview and back.
        #[test]
        fn origin_round_trips(
            mons in prop::collection::vec(rect_strategy(), 1..4),
            max_w in 100f32..2000.0,
            max_h in 100f32..2000.0,
        ) {
            let l = desktop_layout(&mons, max_w, max_h);
            for (m, p) in mons.iter().zip(&l.monitors) {
                let (x, y) = l.point_to_phys(p.x, p.y);
                let tol = 1.0 / l.scale;
                prop_assert!((x - f64::from(m.left)).abs() <= tol, "x {x} vs {}", m.left);
                prop_assert!((y - f64::from(m.top)).abs() <= tol, "y {y} vs {}", m.top);
            }
        }

        // (2) monitors stay inside the preview, which stays inside the box.
        #[test]
        fn monitors_fit_the_box(
            mons in prop::collection::vec(rect_strategy(), 1..4),
            max_w in 100f32..2000.0,
            max_h in 100f32..2000.0,
        ) {
            let l = desktop_layout(&mons, max_w, max_h);
            let eps = 1e-2;
            prop_assert!(l.size.0 <= max_w + eps && l.size.1 <= max_h + eps);
            for p in &l.monitors {
                prop_assert!(p.x >= -eps && p.y >= -eps);
                prop_assert!(p.right() <= l.size.0 + eps && p.bottom() <= l.size.1 + eps);
            }
        }

        // (3) handles only ever come back for the selected island.
        #[test]
        fn handles_only_on_the_selected_island(
            rects in prop::collection::vec((0f32..300.0, 0f32..300.0, 1f32..200.0, 1f32..200.0), 1..6),
            selected in prop::option::of(0usize..8),
            px in -20f32..520.0,
            py in -20f32..520.0,
        ) {
            let islands: Vec<PRect> = rects.iter().map(|&(x, y, w, h)| pr(x, y, w, h)).collect();
            if let Some((i, Hit::Handle(_))) = pick(&islands, selected, (px, py), HALF) {
                prop_assert_eq!(Some(i), selected);
            }
        }
    }
}
