//! Visual system of the panel: one dark palette with a single calm teal accent, a 4-based
//! spacing scale, three radii, soft layered shadows and the small widget kit every view uses.
//!
//! Design rules (keep them when adding widgets):
//! - **One accent** (`ACCENT`); status colours only inside [`badge`].
//! - **Concentric radii**: outer radius = inner radius + padding. Panels are [`R_LG`] (14) and
//!   hold content at 8 px padding with [`R_SM`] (6) children; use [`inner_radius`] otherwise.
//! - **Depth from shadows, not borders**: [`shadow_soft`] / [`shadow_pop`] (layered, with a 1 px
//!   inset hairline). Never `border_1()` on a card.
//! - **Hit areas >= 40x40** ([`HIT`]); hover lightens, press darkens (GPUI divs cannot scale,
//!   so no 0.96 transform); keyboard focus draws an accent ring.
//! - **Three text sizes**: [`TEXT_SM`] 12, [`TEXT_MD`] 14 (default), [`TEXT_XL`] 22 (titles and
//!   stat values). Numbers that change use [`tabular`] so digits do not jitter.

use std::sync::Arc;

use gpui_kit::component::{Icon, IconName, Sizable as _, Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, BoxShadow, ClickEvent, Div, ElementId, FontFeatures, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, Pixels, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, point, px, rgb,
};

// ------------------------------------------------------------------ scale

pub const SP_1: Pixels = px(4.);
pub const SP_2: Pixels = px(8.);
pub const SP_3: Pixels = px(12.);
pub const SP_4: Pixels = px(16.);
pub const SP_6: Pixels = px(24.);
pub const SP_8: Pixels = px(32.);

pub const R_SM: Pixels = px(6.);
pub const R_MD: Pixels = px(10.);
pub const R_LG: Pixels = px(14.);

/// Minimum interactive size (both axes).
pub const HIT: Pixels = px(40.);

pub const TEXT_SM: Pixels = px(12.);
pub const TEXT_MD: Pixels = px(14.);
pub const TEXT_XL: Pixels = px(22.);

/// Concentric radius rule: radius of a child sitting `padding` inside a container of radius `outer`.
pub fn inner_radius(outer: Pixels, padding: Pixels) -> Pixels {
    (outer - padding).max(px(0.))
}

// ---------------------------------------------------------------- palette

const BG: u32 = 0x0e1316;
const SURFACE: u32 = 0x151c20;
const RAISED: u32 = 0x1c262b;
const TEXT: u32 = 0xe7eef1;
const MUTED: u32 = 0x8b9aa2;
const ACCENT_HEX: u32 = 0x45c4b0;
const ON_ACCENT: u32 = 0x07211e;
const OK_HEX: u32 = 0x5bc99a;
const WARN_HEX: u32 = 0xe3b25c;
const ERR_HEX: u32 = 0xe8776c;
const INFO_HEX: u32 = 0x6cb4e4;

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn white(alpha: f32) -> Hsla {
    c(0xffffff).opacity(alpha)
}

fn black(alpha: f32) -> Hsla {
    c(0x000000).opacity(alpha)
}

/// Window background (darkest).
pub fn bg() -> Hsla {
    c(BG)
}
/// Raised panels: sidebar, cards.
pub fn surface() -> Hsla {
    c(SURFACE)
}
/// Controls and wells sitting on a [`surface`].
pub fn raised() -> Hsla {
    c(RAISED)
}
pub fn text() -> Hsla {
    c(TEXT)
}
pub fn text_muted() -> Hsla {
    c(MUTED)
}
pub fn accent() -> Hsla {
    c(ACCENT_HEX)
}
/// Status colours (also used by [`badge`]); for dots and text, never as the only signal.
pub fn ok() -> Hsla {
    c(OK_HEX)
}
pub fn warn() -> Hsla {
    c(WARN_HEX)
}
pub fn danger() -> Hsla {
    c(ERR_HEX)
}

/// Install the dark palette into `gpui_component`'s theme so its widgets (inputs, lists,
/// scrollbars…) match the helpers below. Call once after `gpui_kit::init`.
pub fn install(cx: &mut gpui_kit::App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |t| {
        let accent = accent();
        let hover = |base: Hsla| base.blend(white(0.08));
        let press = |base: Hsla| base.blend(black(0.14));
        let tint = |base: Hsla| base.opacity(0.16);

        t.background = bg();
        t.foreground = text();
        t.border = white(0.07);
        t.input = white(0.10);
        t.ring = accent;
        t.caret = accent;
        t.selection = accent.opacity(0.30);
        t.link = accent;
        t.link_hover = hover(accent);
        t.link_active = press(accent);
        t.muted = raised();
        t.muted_foreground = text_muted();
        t.accent = white(0.06);
        t.accent_foreground = text();
        t.popover = raised();
        t.popover_foreground = text();
        t.overlay = black(0.5);
        t.window_border = white(0.07);

        t.primary = accent;
        t.primary_hover = hover(accent);
        t.primary_active = press(accent);
        t.primary_foreground = c(ON_ACCENT);
        t.button_primary = accent;
        t.button_primary_hover = hover(accent);
        t.button_primary_active = press(accent);
        t.button_primary_foreground = c(ON_ACCENT);
        t.progress_bar = accent;
        t.slider_bar = accent;
        t.slider_thumb = accent;
        t.secondary = raised();
        t.secondary_hover = hover(raised());
        t.secondary_active = press(raised());
        t.secondary_foreground = text();
        t.button = raised();
        t.button_hover = hover(raised());
        t.button_active = press(raised());
        t.button_foreground = text();
        t.button_secondary = raised();
        t.button_secondary_hover = hover(raised());
        t.button_secondary_active = press(raised());
        t.button_secondary_foreground = text();

        t.switch = white(0.18);
        t.switch_thumb = text();
        t.success = c(OK_HEX);
        t.success_hover = hover(c(OK_HEX));
        t.success_active = press(c(OK_HEX));
        t.success_foreground = c(ON_ACCENT);
        t.warning = c(WARN_HEX);
        t.warning_hover = hover(c(WARN_HEX));
        t.warning_active = press(c(WARN_HEX));
        t.warning_foreground = c(0x2a1d05);
        t.danger = c(ERR_HEX);
        t.danger_hover = hover(c(ERR_HEX));
        t.danger_active = press(c(ERR_HEX));
        t.danger_foreground = c(0x2e0b07);
        t.info = c(INFO_HEX);
        t.info_hover = hover(c(INFO_HEX));
        t.info_active = press(c(INFO_HEX));
        t.info_foreground = c(0x07202e);

        t.sidebar = surface();
        t.sidebar_foreground = text();
        t.sidebar_border = white(0.0);
        t.sidebar_accent = tint(accent);
        t.sidebar_accent_foreground = accent;
        t.sidebar_primary = accent;
        t.sidebar_primary_foreground = c(ON_ACCENT);
        t.title_bar = bg();
        t.title_bar_border = white(0.0);

        t.colors.list = surface();
        t.list_even = surface();
        t.list_head = surface();
        t.list_hover = white(0.05);
        t.list_active = tint(accent);
        t.list_active_border = accent;
        t.table = surface();
        t.table_even = surface();
        t.table_head = surface();
        t.table_head_foreground = text_muted();
        t.table_hover = white(0.05);
        t.table_active = tint(accent);
        t.table_active_border = accent;
        t.table_row_border = white(0.05);
        t.tab = bg();
        t.tab_bar = bg();
        t.tab_active = surface();
        t.tab_foreground = text_muted();
        t.tab_active_foreground = text();
        t.scrollbar = white(0.0);
        t.scrollbar_thumb = white(0.20);
        t.scrollbar_thumb_hover = white(0.35);

        t.radius = R_SM;
        t.radius_lg = R_MD;
        t.font_size = TEXT_MD;
    });
}

// ----------------------------------------------------------------- shadows

fn shadow(color: Hsla, y: f32, blur: f32, spread: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset,
    }
}

/// Hairline ring (inset, 1 px, 7 % white): the "border" of a surface without a hard line.
fn hairline() -> BoxShadow {
    shadow(white(0.07), 0., 0., 1., true)
}

/// Resting elevation for cards and the sidebar: contact shadow + wide ambient shadow + hairline.
pub fn shadow_soft() -> Vec<BoxShadow> {
    vec![
        shadow(black(0.28), 1., 2., 0., false),
        shadow(black(0.22), 8., 24., 0., false),
        hairline(),
    ]
}

/// Higher elevation for floating things (menus, dialogs).
pub fn shadow_pop() -> Vec<BoxShadow> {
    vec![
        shadow(black(0.30), 2., 4., 0., false),
        shadow(black(0.30), 16., 40., 0., false),
        hairline(),
    ]
}

fn focus_ring() -> Vec<BoxShadow> {
    // Inset: an outer shadow would show through transparent elements (nav items).
    vec![shadow(accent(), 0., 0., 2., true)]
}

/// Tabular figures: digits keep their width so changing numbers do not jitter.
pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

// ------------------------------------------------------------------ helpers

/// Surface container for a group of related content. Radius 14, padding 16, soft shadow.
/// Nest filled shapes only with `R_SM` radius at `SP_2` inset (14 = 6 + 8), or via [`inner_radius`].
pub fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_3)
        .p(SP_4)
        .rounded(R_LG)
        .bg(surface())
        .shadow(shadow_soft())
}

/// Title (22 px, semibold) with a muted one-line subtitle. Put it at the top of every view.
pub fn section_header(title: impl Into<SharedString>, subtitle: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_1)
        .mb(SP_6)
        .child(
            div()
                .text_size(TEXT_XL)
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(TEXT_MD)
                .text_color(text_muted())
                .child(subtitle.into()),
        )
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnKind {
    Primary,
    Secondary,
}

/// Button with a >= 40x40 hit area, hover/press shading and an accent focus ring. Build it
/// with [`primary_button`] or [`secondary_button`]; chain `.icon(..)` and `.disabled(..)`.
#[derive(IntoElement)]
pub struct Btn {
    id: ElementId,
    label: SharedString,
    kind: BtnKind,
    icon: Option<IconName>,
    disabled: bool,
    on_click: ClickHandler,
}

impl Btn {
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// A disabled button is dimmed, not focusable and ignores clicks (use it while an IPC call
    /// is in flight).
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for Btn {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let (base, fg) = match self.kind {
            BtnKind::Primary => (accent(), c(ON_ACCENT)),
            BtnKind::Secondary => (raised(), text()),
        };
        let mut shadows = vec![shadow(black(0.25), 1., 2., 0., false)];
        shadows.push(hairline());
        let on_click = self.on_click;
        div()
            .id(self.id)
            .min_h(HIT)
            .min_w(HIT)
            .px(SP_4)
            .flex()
            .items_center()
            .justify_center()
            .gap(SP_2)
            .rounded(R_SM)
            .bg(base)
            .text_color(fg)
            .text_size(TEXT_MD)
            .font_weight(FontWeight::MEDIUM)
            .shadow(shadows)
            .when_some(self.icon, |d, icon| d.child(Icon::new(icon).small()))
            .child(self.label)
            .when(self.disabled, |d| d.opacity(0.45).cursor_default())
            .when(!self.disabled, |d| {
                d.focusable()
                    .tab_stop(true)
                    .cursor_pointer()
                    .hover(move |s| s.bg(base.blend(white(0.09))))
                    .active(move |s| s.bg(base.blend(black(0.16))))
                    .focus_visible(|s| s.shadow(focus_ring()))
                    .on_click(on_click)
            })
    }
}

fn button(
    kind: BtnKind,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Btn {
    Btn {
        id: id.into(),
        label: label.into(),
        kind,
        icon: None,
        disabled: false,
        on_click: Box::new(on_click),
    }
}

/// The one main action of a view or card (accent fill). At most one per screen area.
pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Btn {
    button(BtnKind::Primary, id, label, on_click)
}

/// Every other action (neutral raised fill).
pub fn secondary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Btn {
    button(BtnKind::Secondary, id, label, on_click)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Ok,
    Warn,
    Error,
    Info,
    Neutral,
}

/// Small status pill. Always carries text (status is never colour-only): "Gestionada",
/// "Elevada", a verdict…
pub fn badge(text: impl Into<SharedString>, kind: BadgeKind) -> Div {
    let color = match kind {
        BadgeKind::Ok => c(OK_HEX),
        BadgeKind::Warn => c(WARN_HEX),
        BadgeKind::Error => c(ERR_HEX),
        BadgeKind::Info => c(INFO_HEX),
        BadgeKind::Neutral => text_muted(),
    };
    div()
        .px(SP_2)
        .py(px(2.))
        .rounded(R_SM)
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(TEXT_SM)
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

/// Centered placeholder for a view or list with nothing to show: what is missing and what to do.
pub fn empty_state(title: impl Into<SharedString>, body: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(SP_2)
        .p(SP_8)
        .rounded(R_LG)
        .bg(surface())
        .shadow(shadow_soft())
        .child(
            // Accent glyph: a rounded square inside a disc, concentric (disc 14 = square 6 + 8 padding).
            div()
                .size(px(40.))
                .mb(SP_2)
                .flex()
                .items_center()
                .justify_center()
                .rounded(R_LG)
                .bg(accent().opacity(0.14))
                .child(
                    div()
                        .size(px(24.))
                        .rounded(inner_radius(R_LG, SP_2))
                        .bg(accent().opacity(0.55)),
                ),
        )
        .child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_size(TEXT_MD)
                .text_color(text_muted())
                .text_center()
                .child(body.into()),
        )
}

/// Big number with a small label under it (tabular figures). `value` is already formatted.
pub fn stat(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(SP_1)
        .child(
            div()
                .text_size(TEXT_XL)
                .font_weight(FontWeight::SEMIBOLD)
                .font_features(tabular())
                .child(value.into()),
        )
        .child(
            div()
                .text_size(TEXT_SM)
                .text_color(text_muted())
                .child(label.into()),
        )
}

/// Dark bezel for drawing one monitor to scale (Islas / Editor). The caller sets the size,
/// makes it `relative()` and adds absolutely positioned islands as children.
pub fn monitor_frame(label: impl Into<SharedString>) -> Div {
    div()
        .relative()
        .rounded(R_MD)
        .bg(bg())
        .shadow(vec![
            shadow(white(0.10), 0., 0., 2., true),
            shadow(black(0.35), 8., 20., 0., false),
        ])
        .child(
            div()
                .absolute()
                .top(SP_2)
                .left(SP_3)
                .text_size(TEXT_SM)
                .text_color(text_muted())
                .child(label.into()),
        )
}

/// Sidebar entry of the shell: icon + label, >= 40 px high, accent tint when selected.
pub fn nav_item(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let fg = if selected { accent() } else { text() };
    div()
        .id(id)
        .focusable()
        .tab_stop(true)
        .h(HIT)
        .px(SP_3)
        .flex()
        .items_center()
        .gap(SP_3)
        .rounded(R_SM)
        .cursor_pointer()
        .text_color(fg)
        .text_size(TEXT_MD)
        .font_weight(if selected {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .when(selected, |d| d.bg(accent().opacity(0.14)))
        .when(!selected, |d| {
            d.hover(|s| s.bg(white(0.05))).active(|s| s.bg(white(0.09)))
        })
        .focus_visible(|s| s.shadow(focus_ring()))
        .child(Icon::new(icon).text_color(fg))
        .child(label.into())
        .on_click(on_click)
}
