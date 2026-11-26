//! Islands: named sub-rectangles of a monitor (pure geometry, no Win32 types).
//!
//! A [`Layout`] binds a list of [`Island`]s to one monitor ([`MonitorSel`]). An island is
//! addressed as `"layout/island"`. Rectangles are either fractions of the monitor (`0..1`, at
//! most 6 decimals) or physical pixels ([`Unit`]). Resolution never panics: absurd values are
//! clamped to the monitor and adjacent islands share an edge exactly. The error variants of
//! [`IslandError`] map to stable error codes; match those, never the message text.
//!
//! ```
//! use borderless_shatter::islands::{presets, resolve, MonitorInfo};
//! use borderless_shatter::window_state::Rect;
//!
//! let monitors = [MonitorInfo {
//!     index: 0,
//!     device: r"\\.\DISPLAY1".into(),
//!     rect: Rect { left: 0, top: 0, right: 2560, bottom: 1440 },
//!     dpi: 96,
//!     primary: true,
//! }];
//! let right = resolve(&presets(), "halves-16-9/right", &monitors).unwrap();
//! assert_eq!((right.left, right.right), (1280, 2560));
//! ```

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::window_state::Rect;

/// Why an island reference or monitor selector could not be resolved.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum IslandError {
    /// The reference is not of the form `layout/island` (exactly one `/`, both parts non-empty).
    #[error("invalid island reference \"{0}\": expected \"layout/island\"")]
    BadRef(String),
    /// No layout or island matches the reference, even through [`LEGACY_ALIASES`].
    #[error("island not found: {0}")]
    IslandNotFound(String),
    /// The layout's monitor selector matches no connected monitor.
    #[error("monitor not available: {0}")]
    MonitorUnavailable(String),
}

/// Monitor selector. JSON: integer index | device name | `"primary"`.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorSel {
    /// 0-based monitor index.
    Index(usize),
    /// Device name such as `\\.\DISPLAY1`, matched case-insensitively.
    Device(String),
    /// The primary monitor (`"primary"`, any case).
    Primary,
}

impl Serialize for MonitorSel {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MonitorSel::Index(i) => s.serialize_u64(*i as u64),
            MonitorSel::Device(d) => s.serialize_str(d),
            MonitorSel::Primary => s.serialize_str("primary"),
        }
    }
}

impl<'de> Deserialize<'de> for MonitorSel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Index(usize),
            Name(String),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Index(i) => MonitorSel::Index(i),
            Raw::Name(n) if n.eq_ignore_ascii_case("primary") => MonitorSel::Primary,
            Raw::Name(n) => MonitorSel::Device(n),
        })
    }
}

impl JsonSchema for MonitorSel {
    fn schema_name() -> Cow<'static, str> {
        "MonitorSel".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "description": "0-based index, device name (\\\\.\\DISPLAY1) or \"primary\"",
            "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "string" }]
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Unit of an [`IslandRect`].
pub enum Unit {
    /// Fractions of the monitor size (`0..1`, at most 6 decimals). The default.
    #[default]
    Fraction,
    /// Physical pixels, offset from the monitor's top-left corner.
    Px,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
/// Island rectangle relative to its monitor.
pub struct IslandRect {
    /// Left edge, in `unit`.
    pub x: f64,
    /// Top edge, in `unit`.
    pub y: f64,
    /// Width, in `unit`.
    pub w: f64,
    /// Height, in `unit`.
    pub h: f64,
    /// How the other fields are measured; defaults to [`Unit::Fraction`].
    #[serde(default)]
    pub unit: Unit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// A named rectangular zone of a monitor.
pub struct Island {
    /// Id, unique within its layout.
    pub id: String,
    /// Where the island sits on the monitor.
    pub rect: IslandRect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// A set of islands on one monitor.
pub struct Layout {
    /// Layout id, used as the first half of `layout/island`.
    pub id: String,
    /// Monitor the islands are resolved against.
    pub monitor: MonitorSel,
    /// The islands of this layout.
    pub islands: Vec<Island>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// A connected monitor as reported by the OS.
pub struct MonitorInfo {
    /// 0-based index.
    pub index: usize,
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub device: String,
    /// Monitor rectangle in virtual-desktop physical pixels.
    pub rect: Rect,
    /// Effective DPI (96 = 100 %).
    pub dpi: u32,
    /// Whether this is the primary monitor.
    pub primary: bool,
}

/// Declared game resolution in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Size {
    /// Width in physical pixels.
    pub width: i32,
    /// Height in physical pixels.
    pub height: i32,
}

/// `(legacy layout, canonical layout, [(legacy island, canonical island)])`.
pub type LegacyAlias = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// Legacy preset ids, still accepted wherever a reference or layout id is resolved.
#[rustfmt::skip]
pub const LEGACY_ALIASES: &[LegacyAlias] = &[("mitades-16-9", "halves-16-9", &[("izquierda", "left"), ("derecha", "right")])];

/// The other name of `id` in `pairs`, in either direction.
fn counterpart<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    id: &str,
) -> Option<&'a str> {
    pairs.into_iter().find_map(|(a, b)| {
        if id == a {
            Some(b)
        } else if id == b {
            Some(a)
        } else {
            None
        }
    })
}

fn layout_counterpart(id: &str) -> Option<&'static str> {
    counterpart(LEGACY_ALIASES.iter().map(|&(l, c, _)| (l, c)), id)
}

/// Built-in layouts: currently `halves-16-9` (`left`/`right` halves of the primary monitor).
pub fn presets() -> Vec<Layout> {
    let half = |id: &str, x: f64| Island {
        id: id.into(),
        rect: IslandRect {
            x,
            y: 0.0,
            w: 0.5,
            h: 1.0,
            unit: Unit::Fraction,
        },
    };
    vec![Layout {
        id: "halves-16-9".into(),
        monitor: MonitorSel::Primary,
        islands: vec![half("left", 0.0), half("right", 0.5)],
    }]
}

/// Presets first, replaced in place by user layouts with the same id (or a legacy alias of it,
/// unless the user also has a layout with the preset's own id); extra user layouts appended.
pub fn merged_layouts(user: &[Layout]) -> Vec<Layout> {
    let mut out = presets();
    for u in user {
        let alias = layout_counterpart(&u.id).filter(|c| !user.iter().any(|x| x.id == *c));
        let slot = out
            .iter()
            .position(|l| l.id == u.id)
            .or_else(|| alias.and_then(|c| out.iter().position(|l| l.id == c)));
        match slot {
            Some(i) => out[i] = u.clone(),
            None => out.push(u.clone()),
        }
    }
    out
}

/// Finds `layout/island`: exact ids first, then each missing id through [`LEGACY_ALIASES`] in
/// either direction (island aliases only inside their own layout pair).
pub fn find_island<'a>(
    layouts: &'a [Layout],
    island_ref: &str,
) -> Result<(&'a Layout, &'a Island), IslandError> {
    let (lid, iid) = parse_ref(island_ref)?;
    let not_found = || IslandError::IslandNotFound(island_ref.to_string());
    let by_id = |id: &str| layouts.iter().find(|l| l.id == id);
    let layout = by_id(&lid)
        .or_else(|| layout_counterpart(&lid).and_then(by_id))
        .ok_or_else(not_found)?;
    let island_pairs = LEGACY_ALIASES
        .iter()
        .filter(|(l, c, _)| *l == layout.id || *c == layout.id)
        .flat_map(|(_, _, pairs)| pairs.iter().copied());
    let island = layout
        .islands
        .iter()
        .find(|i| i.id == iid)
        .or_else(|| {
            let other = counterpart(island_pairs, &iid)?;
            layout.islands.iter().find(|i| i.id == other)
        })
        .ok_or_else(not_found)?;
    Ok((layout, island))
}

/// The `layout/island` ids `island_ref` resolves to in `layouts`; `None` if it does not resolve.
pub fn canonical_ref(layouts: &[Layout], island_ref: &str) -> Option<String> {
    find_island(layouts, island_ref)
        .ok()
        .map(|(l, i)| format!("{}/{}", l.id, i.id))
}

/// Splits `layout/island` into its two ids; exactly one `/` and both parts non-empty.
///
/// ```
/// use borderless_shatter::islands::{parse_ref, IslandError};
///
/// assert_eq!(parse_ref("halves-16-9/left").unwrap(), ("halves-16-9".into(), "left".into()));
/// assert!(matches!(parse_ref("a/b/c"), Err(IslandError::BadRef(_))));
/// ```
pub fn parse_ref(s: &str) -> Result<(String, String), IslandError> {
    match s.split_once('/') {
        Some((l, i)) if !l.is_empty() && !i.is_empty() && !i.contains('/') => {
            Ok((l.to_string(), i.to_string()))
        }
        _ => Err(IslandError::BadRef(s.to_string())),
    }
}

/// Picks the monitor a [`MonitorSel`] designates; device names compare case-insensitively.
pub fn select_monitor<'a>(
    sel: &MonitorSel,
    monitors: &'a [MonitorInfo],
) -> Result<&'a MonitorInfo, IslandError> {
    let found = match sel {
        MonitorSel::Index(i) => monitors.iter().find(|m| m.index == *i),
        MonitorSel::Device(d) => monitors.iter().find(|m| m.device.eq_ignore_ascii_case(d)),
        MonitorSel::Primary => monitors.iter().find(|m| m.primary),
    };
    found.ok_or_else(|| {
        IslandError::MonitorUnavailable(match sel {
            MonitorSel::Index(i) => format!("index {i}"),
            MonitorSel::Device(d) => d.clone(),
            MonitorSel::Primary => "primary".into(),
        })
    })
}

/// Rectangle of `r` on `monitor`, in physical pixels.
///
/// Edges are rounded, not sizes, so adjacent islands share an edge exactly.
/// Absurd values (f64 `as i32` saturates; NaN is 0) are clamped to the monitor.
pub fn island_rect_on(monitor: Rect, r: &IslandRect) -> Rect {
    let (w, h) = (monitor.width() as f64, monitor.height() as f64);
    let (sx, sy) = match r.unit {
        Unit::Fraction => (w, h),
        Unit::Px => (1.0, 1.0),
    };
    let edge =
        |base: i32, v: f64, lo: i32, hi: i32| base.saturating_add(v.round() as i32).clamp(lo, hi);
    let (lo_x, hi_x) = (monitor.left, monitor.right.max(monitor.left));
    let (lo_y, hi_y) = (monitor.top, monitor.bottom.max(monitor.top));
    Rect {
        left: edge(monitor.left, r.x * sx, lo_x, hi_x),
        top: edge(monitor.top, r.y * sy, lo_y, hi_y),
        right: edge(monitor.left, (r.x + r.w) * sx, lo_x, hi_x),
        bottom: edge(monitor.top, (r.y + r.h) * sy, lo_y, hi_y),
    }
}

/// Resolves `layout/island` (legacy aliases accepted) to a physical-pixel rectangle on the
/// layout's monitor.
pub fn resolve(
    layouts: &[Layout],
    island_ref: &str,
    monitors: &[MonitorInfo],
) -> Result<Rect, IslandError> {
    let (layout, island) = find_island(layouts, island_ref)?;
    let mon = select_monitor(&layout.monitor, monitors)?;
    Ok(island_rect_on(mon.rect, &island.rect))
}

/// Centres `size` (physical pixels) in `island`. The flag is true when clipping happened.
pub fn center_in(island: Rect, size: Size) -> (Rect, bool) {
    let w = size.width.min(island.width());
    let h = size.height.min(island.height());
    let left = island
        .left
        .saturating_add(island.width().saturating_sub(w) / 2);
    let top = island
        .top
        .saturating_add(island.height().saturating_sub(h) / 2);
    let clipped = w != size.width || h != size.height;
    (
        Rect {
            left,
            top,
            right: left.saturating_add(w),
            bottom: top.saturating_add(h),
        },
        clipped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn r(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn mon(index: usize, device: &str, rect: Rect, primary: bool) -> MonitorInfo {
        MonitorInfo {
            index,
            device: device.into(),
            rect,
            dpi: 96,
            primary,
        }
    }

    fn mons() -> Vec<MonitorInfo> {
        vec![
            mon(0, r"\\.\DISPLAY1", r(0, 0, 5120, 1440), false),
            mon(1, r"\\.\DISPLAY2", r(5120, 0, 7040, 1080), true),
        ]
    }

    #[test]
    fn extreme_values_clamp_and_never_panic() {
        let m = r(i32::MAX - 10, i32::MIN, i32::MAX, i32::MIN + 10);
        for (x, w, unit) in [
            (1e9, 1e9, Unit::Fraction),
            (1e12, 1e12, Unit::Px),
            (-1e12, 1.0, Unit::Px),
            (f64::NAN, f64::INFINITY, Unit::Fraction),
        ] {
            let rect = IslandRect {
                x,
                y: x,
                w,
                h: w,
                unit,
            };
            let _ = island_rect_on(m, &rect);
            let big = island_rect_on(r(0, 0, 1920, 1080), &rect);
            assert!(big.left >= 0 && big.right <= 1920 && big.bottom <= 1080);
        }
        let _ = center_in(
            r(i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            Size {
                width: i32::MAX,
                height: i32::MAX,
            },
        );
    }

    fn frac(x: f64, y: f64, w: f64, h: f64) -> IslandRect {
        IslandRect {
            x,
            y,
            w,
            h,
            unit: Unit::Fraction,
        }
    }

    #[test]
    fn halves_on_ultrawide() {
        let m = r(0, 0, 5120, 1440);
        assert_eq!(
            island_rect_on(m, &frac(0.0, 0.0, 0.5, 1.0)),
            r(0, 0, 2560, 1440)
        );
        assert_eq!(
            island_rect_on(m, &frac(0.5, 0.0, 0.5, 1.0)),
            r(2560, 0, 5120, 1440)
        );
    }

    #[test]
    fn odd_width_is_contiguous() {
        let m = r(100, 0, 2661, 1000); // width 2561
        let a = island_rect_on(m, &frac(0.0, 0.0, 0.5, 1.0));
        let b = island_rect_on(m, &frac(0.5, 0.0, 0.5, 1.0));
        assert_eq!(a.left, 100);
        assert_eq!(a.right, b.left);
        assert_eq!(b.right, 2661);
    }

    #[test]
    fn px_unit_is_offset_from_origin() {
        let m = r(5120, 0, 7040, 1080);
        let p = IslandRect {
            x: 10.0,
            y: 20.0,
            w: 800.0,
            h: 600.0,
            unit: Unit::Px,
        };
        assert_eq!(island_rect_on(m, &p), r(5130, 20, 5930, 620));
    }

    #[test]
    fn monitor_selection() {
        let ms = mons();
        assert_eq!(select_monitor(&MonitorSel::Index(1), &ms).unwrap().index, 1);
        assert_eq!(
            select_monitor(&MonitorSel::Device(r"\\.\DISPLAY1".into()), &ms)
                .unwrap()
                .index,
            0
        );
        assert_eq!(select_monitor(&MonitorSel::Primary, &ms).unwrap().index, 1);
        assert!(matches!(
            select_monitor(&MonitorSel::Index(5), &ms),
            Err(IslandError::MonitorUnavailable(_))
        ));
        assert!(matches!(
            select_monitor(&MonitorSel::Device("x".into()), &ms),
            Err(IslandError::MonitorUnavailable(_))
        ));
    }

    #[test]
    fn parse_ref_cases() {
        assert_eq!(parse_ref("a/b").unwrap(), ("a".into(), "b".into()));
        for bad in ["", "a", "/b", "a/", "a/b/c"] {
            assert!(
                matches!(parse_ref(bad), Err(IslandError::BadRef(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn merged_layouts_override() {
        let user = vec![
            Layout {
                id: "halves-16-9".into(),
                monitor: MonitorSel::Index(0),
                islands: vec![],
            },
            Layout {
                id: "other".into(),
                monitor: MonitorSel::Primary,
                islands: vec![],
            },
        ];
        let merged = merged_layouts(&user);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].monitor, MonitorSel::Index(0));
        assert!(merged[0].islands.is_empty());
        assert_eq!(merged_layouts(&[]), presets());
    }

    #[test]
    fn preset_is_halves_with_left_and_right() {
        let p = presets();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "halves-16-9");
        assert_eq!(p[0].monitor, MonitorSel::Primary);
        let ids: Vec<&str> = p[0].islands.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["left", "right"]);
    }

    fn user_legacy_layout() -> Layout {
        Layout {
            id: "mitades-16-9".into(), // legacy alias test
            monitor: MonitorSel::Index(0),
            islands: vec![
                Island {
                    id: "izquierda".into(), // legacy alias test
                    rect: frac(0.0, 0.0, 0.4, 1.0),
                },
                Island {
                    id: "derecha".into(), // legacy alias test
                    rect: frac(0.4, 0.0, 0.6, 1.0),
                },
            ],
        }
    }

    #[test]
    fn user_layout_with_legacy_id_replaces_preset_slot() {
        let merged = merged_layouts(&[user_legacy_layout()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], user_legacy_layout());
    }

    #[test]
    fn user_layouts_with_both_ids_are_both_kept() {
        let canonical = Layout {
            id: "halves-16-9".into(),
            monitor: MonitorSel::Index(1),
            islands: vec![],
        };
        for user in [
            vec![canonical.clone(), user_legacy_layout()],
            vec![user_legacy_layout(), canonical.clone()],
        ] {
            let merged = merged_layouts(&user);
            assert_eq!(merged.len(), 2);
            assert_eq!(merged[0], canonical);
            assert_eq!(merged[1], user_legacy_layout());
        }
    }

    fn found(layouts: &[Layout], reference: &str) -> (String, String) {
        let (l, i) = find_island(layouts, reference).unwrap();
        (l.id.clone(), i.id.clone())
    }

    #[test]
    fn find_island_exact() {
        let ls = merged_layouts(&[]);
        assert_eq!(
            found(&ls, "halves-16-9/right"),
            ("halves-16-9".into(), "right".into())
        );
    }

    #[test]
    fn find_island_legacy_alias_reaches_preset() {
        let ls = merged_layouts(&[]);
        assert_eq!(
            found(&ls, "mitades-16-9/derecha"), // legacy alias test
            ("halves-16-9".into(), "right".into())
        );
        assert_eq!(
            found(&ls, "mitades-16-9/izquierda"), // legacy alias test
            ("halves-16-9".into(), "left".into())
        );
        assert_eq!(
            found(&ls, "halves-16-9/izquierda"), // legacy alias test
            ("halves-16-9".into(), "left".into())
        );
    }

    #[test]
    fn find_island_reverse_alias_reaches_legacy_user_layout() {
        let ls = merged_layouts(&[user_legacy_layout()]);
        assert_eq!(
            found(&ls, "halves-16-9/right"),
            ("mitades-16-9".into(), "derecha".into()) // legacy alias test
        );
        assert_eq!(
            found(&ls, "mitades-16-9/izquierda"),        // legacy alias test
            ("mitades-16-9".into(), "izquierda".into())  // legacy alias test
        );
    }

    #[test]
    fn find_island_prefers_exact_ids() {
        let canonical = Layout {
            id: "halves-16-9".into(),
            monitor: MonitorSel::Primary,
            islands: vec![Island {
                id: "right".into(),
                rect: frac(0.0, 0.0, 1.0, 1.0),
            }],
        };
        let ls = merged_layouts(&[canonical, user_legacy_layout()]);
        assert_eq!(
            found(&ls, "mitades-16-9/derecha"),        // legacy alias test
            ("mitades-16-9".into(), "derecha".into())  // legacy alias test
        );
        assert_eq!(
            found(&ls, "halves-16-9/right"),
            ("halves-16-9".into(), "right".into())
        );
    }

    #[test]
    fn island_aliases_only_apply_inside_their_layout() {
        let other = Layout {
            id: "other".into(),
            monitor: MonitorSel::Primary,
            islands: vec![Island {
                id: "right".into(),
                rect: frac(0.0, 0.0, 1.0, 1.0),
            }],
        };
        let ls = merged_layouts(&[other]);
        assert!(matches!(
            find_island(&ls, "other/derecha"), // legacy alias test
            Err(IslandError::IslandNotFound(_))
        ));
    }

    #[test]
    fn every_alias_pair_names_a_preset_island() {
        let ls = presets();
        for (legacy, canonical, isl) in LEGACY_ALIASES {
            for (li, ci) in *isl {
                let a = found(&ls, &format!("{legacy}/{li}"));
                assert_eq!(a, found(&ls, &format!("{canonical}/{ci}")));
                assert_eq!(a, (canonical.to_string(), ci.to_string()));
            }
        }
    }

    #[test]
    fn canonical_ref_uses_ids_of_current_layouts() {
        let ls = merged_layouts(&[]);
        assert_eq!(
            canonical_ref(&ls, "mitades-16-9/derecha").as_deref(), // legacy alias test
            Some("halves-16-9/right")
        );
        assert_eq!(
            canonical_ref(&ls, "halves-16-9/left").as_deref(),
            Some("halves-16-9/left")
        );
        assert_eq!(canonical_ref(&ls, "halves-16-9/centre"), None);
        assert_eq!(canonical_ref(&ls, "bad"), None);
        let user = merged_layouts(&[user_legacy_layout()]);
        assert_eq!(
            canonical_ref(&user, "halves-16-9/right").as_deref(),
            Some("mitades-16-9/derecha") // legacy alias test
        );
    }

    #[test]
    fn resolve_ok_and_errors() {
        let ms = mons();
        let ls = merged_layouts(&[]);
        // preset targets the primary monitor (index 1, 1920x1080)
        assert_eq!(
            resolve(&ls, "halves-16-9/right", &ms).unwrap(),
            r(6080, 0, 7040, 1080)
        );
        assert_eq!(
            resolve(&ls, "mitades-16-9/derecha", &ms).unwrap(), // legacy alias test
            r(6080, 0, 7040, 1080)
        );
        assert!(matches!(
            resolve(&ls, "nope", &ms),
            Err(IslandError::BadRef(_))
        ));
        assert!(matches!(
            resolve(&ls, "x/right", &ms),
            Err(IslandError::IslandNotFound(_))
        ));
        assert!(matches!(
            resolve(&ls, "halves-16-9/centre", &ms),
            Err(IslandError::IslandNotFound(_))
        ));
        assert!(matches!(
            resolve(
                &ls,
                "halves-16-9/right",
                &[mon(0, "d", r(0, 0, 10, 10), false)]
            ),
            Err(IslandError::MonitorUnavailable(_))
        ));
    }

    #[test]
    fn center_in_cases() {
        let isl = r(2560, 0, 5120, 1440);
        assert_eq!(
            center_in(
                isl,
                Size {
                    width: 1280,
                    height: 720
                }
            ),
            (r(3200, 360, 4480, 1080), false)
        );
        let (c, clipped) = center_in(
            isl,
            Size {
                width: 3000,
                height: 720,
            },
        );
        assert!(clipped);
        assert_eq!(c, r(2560, 360, 5120, 1080));
    }

    #[test]
    fn monitor_sel_json() {
        assert_eq!(
            serde_json::from_str::<MonitorSel>("2").unwrap(),
            MonitorSel::Index(2)
        );
        assert_eq!(
            serde_json::from_str::<MonitorSel>("\"primary\"").unwrap(),
            MonitorSel::Primary
        );
        assert_eq!(
            serde_json::from_str::<MonitorSel>(r#""\\\\.\\DISPLAY1""#).unwrap(),
            MonitorSel::Device(r"\\.\DISPLAY1".into())
        );
        assert_eq!(serde_json::to_string(&MonitorSel::Index(0)).unwrap(), "0");
        assert_eq!(
            serde_json::to_string(&MonitorSel::Primary).unwrap(),
            "\"primary\""
        );
        assert!(serde_json::from_str::<MonitorSel>("-1").is_err());
        let _ = schemars::schema_for!(Layout);
    }

    #[test]
    fn unit_defaults_to_fraction() {
        let ir: IslandRect = serde_json::from_str(r#"{"x":0,"y":0,"w":1,"h":1}"#).unwrap();
        assert_eq!(ir.unit, Unit::Fraction);
        assert_eq!(serde_json::to_string(&Unit::Px).unwrap(), "\"px\"");
    }
}
