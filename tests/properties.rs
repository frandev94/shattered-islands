//! Property tests for the pure core: islands, window state, guard, profiles and IPC framing.
//!
//! Deterministic by construction: fixed RNG seed, no `proptest-regressions` files. The case count
//! is proptest's default (256) and honours `PROPTEST_CASES`, e.g.
//! `PROPTEST_CASES=2000 cargo test --test properties`.
//!
//! f64 values that travel through JSON text are drawn from a dyadic grid (`k / 1024`): serde_json
//! without `float_roundtrip` does not promise exact round trips for arbitrary f64.

use borderless_shatter::guard::{Action, Guard, GuardConfig, Observation};
use borderless_shatter::ipc::{
    self, ApplyParams, DiagnoseParams, ErrorCode, Id, IpcError, ListWindowsParams, PlaceParams,
    Request, RestoreParams, SetProfilesParams, WindowRef,
};
use borderless_shatter::islands::{
    self, Island, IslandRect, Layout, MonitorInfo, MonitorSel, Size, Unit, center_in,
    island_rect_on,
};
use borderless_shatter::profiles::{
    self, MonitorChoice, Profile, ProfileError, ProfileFile, ProfileKey, SUPPORTED_VERSION,
};
use borderless_shatter::window_state::{
    EXSTYLE_REMOVE, Offsets, Rect, STYLE_REMOVE, borderless_styles, target_rect,
};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn cfg() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(0xB0_5EED),
        ..ProptestConfig::default()
    }
}

// ------------------------------------------------------------------ shared generators

fn rect4(l: i32, t: i32, r: i32, b: i32) -> Rect {
    Rect {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

fn within(inner: Rect, outer: Rect) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

fn any_rect() -> impl Strategy<Value = Rect> {
    (any::<i32>(), any::<i32>(), any::<i32>(), any::<i32>())
        .prop_map(|(l, t, r, b)| rect4(l, t, r, b))
}

fn any_offsets() -> impl Strategy<Value = Offsets> {
    (any::<i32>(), any::<i32>(), any::<i32>(), any::<i32>()).prop_map(
        |(left, top, right, bottom)| Offsets {
            left,
            top,
            right,
            bottom,
        },
    )
}

fn any_size() -> impl Strategy<Value = Size> {
    (any::<i32>(), any::<i32>()).prop_map(|(width, height)| Size { width, height })
}

fn unit() -> impl Strategy<Value = Unit> {
    prop_oneof![Just(Unit::Fraction), Just(Unit::Px)]
}

/// Valid rectangle on the grid; pixel units are scaled by 64 (exact, far below the 100 000 cap).
fn valid_rect() -> impl Strategy<Value = IslandRect> {
    let span = (0u32..1024).prop_flat_map(|a| (Just(a), a + 1..=1024));
    (span.clone(), span, unit()).prop_map(|((xa, xb), (ya, yb), unit)| {
        let k = if unit == Unit::Px { 64.0 } else { 1.0 };
        let f = |v: u32| f64::from(v) / 1024.0 * k;
        IslandRect {
            x: f(xa),
            y: f(ya),
            w: f(xb - xa),
            h: f(yb - ya),
            unit,
        }
    })
}

fn valid_layouts() -> impl Strategy<Value = Vec<Layout>> {
    vec(vec(valid_rect(), 1..4), 1..4).prop_map(|ls| {
        ls.into_iter()
            .enumerate()
            .map(|(i, rects)| Layout {
                id: format!("l{i}"),
                monitor: MonitorSel::Primary,
                islands: rects
                    .into_iter()
                    .enumerate()
                    .map(|(j, rect)| Island {
                        id: format!("i{j}"),
                        rect,
                        extra: Default::default(),
                    })
                    .collect(),
                extra: Default::default(),
            })
            .collect()
    })
}

fn arb_monitor_sel() -> impl Strategy<Value = MonitorSel> {
    prop_oneof![
        any::<usize>().prop_map(MonitorSel::Index),
        // "primary" is not bijective by design: it deserialises to `Primary`.
        any::<String>()
            .prop_filter("primary", |s| !s.eq_ignore_ascii_case("primary"))
            .prop_map(MonitorSel::Device),
        Just(MonitorSel::Primary),
    ]
}

fn arb_island_rect_any() -> impl Strategy<Value = IslandRect> {
    let g = || (-4096i32..=4096).prop_map(|k| f64::from(k) / 1024.0);
    (g(), g(), g(), g(), unit()).prop_map(|(x, y, w, h, unit)| IslandRect { x, y, w, h, unit })
}

/// Unknown fields: no known field at any level starts with `_`; no floats (see the file header).
/// Arrays and objects too: the strict load must not reject them inside an unknown field.
fn arb_extra() -> impl Strategy<Value = serde_json::Map<String, Value>> {
    let value = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<String>().prop_map(Value::from),
        vec(any::<i64>(), 0..3).prop_map(Value::from),
        proptest::collection::btree_map("[a-z]{1,3}", any::<i64>(), 0..3)
            .prop_map(|m| Value::Object(m.into_iter().map(|(k, v)| (k, v.into())).collect())),
    ];
    proptest::collection::btree_map("_[a-z0-9]{1,6}", value, 0..3)
        .prop_map(|m| m.into_iter().collect())
}

fn arb_layout_any() -> impl Strategy<Value = Layout> {
    (
        any::<String>(),
        arb_monitor_sel(),
        vec((any::<String>(), arb_island_rect_any(), arb_extra()), 0..4),
        arb_extra(),
    )
        .prop_map(|(id, monitor, islands, extra)| Layout {
            id,
            monitor,
            islands: islands
                .into_iter()
                .map(|(id, rect, extra)| Island { id, rect, extra })
                .collect(),
            extra,
        })
}

fn arb_profile() -> impl Strategy<Value = Profile> {
    (
        (
            any::<String>(),
            proptest::option::of(any::<String>()),
            proptest::option::of(any::<String>()),
        ),
        any::<bool>(),
        prop_oneof![
            Just(MonitorChoice::Current),
            any::<usize>().prop_map(MonitorChoice::Index)
        ],
        any_offsets(),
        any::<u64>(),
        any::<bool>(),
        proptest::option::of(any::<String>()),
        proptest::option::of(any_size()),
        arb_extra(),
    )
        .prop_map(
            |(
                (process, title, title_regex),
                enabled,
                monitor,
                offsets,
                delay_ms,
                clip_cursor,
                island,
                game_resolution,
                extra,
            )| {
                Profile {
                    process,
                    title,
                    title_regex,
                    enabled,
                    monitor,
                    offsets,
                    delay_ms,
                    clip_cursor,
                    island,
                    game_resolution,
                    extra,
                }
            },
        )
}

fn arb_file() -> impl Strategy<Value = ProfileFile> {
    (
        0u32..=SUPPORTED_VERSION,
        vec(arb_layout_any(), 0..3),
        vec(arb_profile(), 0..3),
        arb_extra(),
    )
        .prop_map(|(version, layouts, profiles, extra)| ProfileFile {
            version,
            layouts,
            profiles,
            extra,
        })
}

/// Largest char boundary that is a proper prefix of `s` (`s` non-empty).
fn proper_prefix(s: &str, cut: usize) -> &str {
    let mut c = cut % s.len();
    while !s.is_char_boundary(c) {
        c -= 1;
    }
    &s[..c]
}

// ------------------------------------------------------------------------------ islands

fn monitor() -> impl Strategy<Value = Rect> {
    (
        -100_000i32..100_000,
        -100_000i32..100_000,
        1i32..100_000,
        1i32..100_000,
    )
        .prop_map(|(l, t, w, h)| rect4(l, t, l + w, t + h))
}

fn frac() -> impl Strategy<Value = f64> {
    0.0f64..=1.0
}

fn extreme_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(1e12),
        Just(-1e12),
        Just(f64::MAX),
        Just(f64::MIN),
        Just(f64::MIN_POSITIVE),
        any::<f64>(),
        -2.0f64..2.0,
    ]
}

fn frac_rect(x: f64, y: f64, w: f64, h: f64) -> IslandRect {
    IslandRect {
        x,
        y,
        w,
        h,
        unit: Unit::Fraction,
    }
}

fn mon_info(rect: Rect) -> MonitorInfo {
    MonitorInfo {
        index: 0,
        device: "X".into(),
        rect,
        dpi: 96,
        primary: true,
    }
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn island_inside_monitor(m in monitor(), x in frac(), y in frac(), w in frac(), h in frac()) {
        let r = island_rect_on(m, &frac_rect(x, y, w, h));
        prop_assert!(within(r, m), "{r:?} not inside {m:?}");
        prop_assert!(r.left <= r.right && r.top <= r.bottom, "{r:?} inverted");
    }

    #[test]
    fn island_size_is_proportional(m in monitor(), a in frac(), b in frac()) {
        let (lo, hi) = (a.min(b), a.max(b));
        let r = island_rect_on(m, &frac_rect(lo, 0.0, hi - lo, 1.0));
        let expected = (hi - lo) * f64::from(m.width());
        prop_assert!((f64::from(r.width()) - expected).abs() <= 1.0 + 1e-6, "{r:?} vs {expected}");
        prop_assert_eq!((r.top, r.bottom), (m.top, m.bottom));
    }

    #[test]
    fn full_island_is_monitor(m in monitor()) {
        prop_assert_eq!(island_rect_on(m, &frac_rect(0.0, 0.0, 1.0, 1.0)), m);
    }

    /// Edges are rounded, not sizes: A's right edge and B's left edge are the same f64 expression,
    /// so they coincide unconditionally. (With decimal literals such as 0.06 + 0.59 vs 0.65 the
    /// f64 sum can differ by 1 ULP and open a 1 px gap on tiny monitors; not asserted here.)
    #[test]
    fn adjacent_islands_share_edge(m in monitor(), x in frac(), w1 in frac(), w2 in frac()) {
        let a = island_rect_on(m, &frac_rect(x, 0.0, w1, 1.0));
        let b = island_rect_on(m, &frac_rect(x + w1, 0.0, w2, 1.0));
        prop_assert_eq!(a.right, b.left);
    }

    #[test]
    fn presets_tile_any_monitor(m in monitor(), odd in any::<bool>()) {
        let m = if odd && m.width() % 2 == 0 { rect4(m.left, m.top, m.right + 1, m.bottom) } else { m };
        let mons = [mon_info(m)];
        let layouts = islands::presets();
        let l = islands::resolve(&layouts, "halves-16-9/left", &mons).unwrap();
        let r = islands::resolve(&layouts, "halves-16-9/right", &mons).unwrap();
        prop_assert_eq!(l.right, r.left);
        prop_assert_eq!(l.left, m.left);
        prop_assert_eq!(r.right, m.right);
        prop_assert_eq!(l.width() + r.width(), m.width());
        prop_assert!((l.width() - r.width()).abs() <= 1);
    }

    /// Every legacy/canonical name combination of a preset island resolves to the same rect,
    /// on the presets and on a user override saved under either layout id and island naming.
    #[test]
    fn legacy_aliases_resolve_like_canonical_ids(
        m in monitor(),
        user in proptest::option::of((any::<bool>(), any::<bool>(), valid_rect(), valid_rect())),
    ) {
        let mons = [mon_info(m)];
        for &(legacy, canonical, pairs) in islands::LEGACY_ALIASES {
            let user_layouts: Vec<Layout> = user
                .iter()
                .map(|&(legacy_layout, legacy_islands, a, b)| Layout {
                    id: if legacy_layout { legacy } else { canonical }.to_string(),
                    monitor: MonitorSel::Primary,
                    islands: pairs
                        .iter()
                        .zip([a, b])
                        .map(|(&(li, ci), rect)| Island {
                            id: if legacy_islands { li } else { ci }.to_string(),
                            rect,
                            extra: Default::default(),
                        })
                        .collect(),
                    extra: Default::default(),
                })
                .collect();
            let layouts = islands::merged_layouts(&user_layouts);
            prop_assert_eq!(layouts.len(), 1);
            for &(li, ci) in pairs {
                let expected = islands::resolve(&layouts, &format!("{canonical}/{ci}"), &mons).unwrap();
                for l in [legacy, canonical] {
                    for i in [li, ci] {
                        let got = islands::resolve(&layouts, &format!("{l}/{i}"), &mons).unwrap();
                        prop_assert_eq!(got, expected, "{}/{}", l, i);
                    }
                }
            }
        }
    }

    #[test]
    fn extreme_inputs_stay_inside(
        m in prop_oneof![monitor(), any_rect()],
        x in extreme_f64(), y in extreme_f64(), w in extreme_f64(), h in extreme_f64(),
        unit in unit(),
    ) {
        let r = island_rect_on(m, &IslandRect { x, y, w, h, unit });
        let (hx, hy) = (m.right.max(m.left), m.bottom.max(m.top));
        for e in [r.left, r.right] {
            prop_assert!(e >= m.left && e <= hx, "x edge {e} outside [{}, {hx}]", m.left);
        }
        for e in [r.top, r.bottom] {
            prop_assert!(e >= m.top && e <= hy, "y edge {e} outside [{}, {hy}]", m.top);
        }
    }

    #[test]
    fn center_in_inside_and_centred(
        l in -1_000_000i32..1_000_000, t in -1_000_000i32..1_000_000,
        iw in 0i32..1_000_000, ih in 0i32..1_000_000,
        sw in 0i32..2_000_000, sh in 0i32..2_000_000,
    ) {
        let isl = rect4(l, t, l + iw, t + ih);
        let (r, clipped) = center_in(isl, Size { width: sw, height: sh });
        prop_assert!(within(r, isl), "{r:?} not inside {isl:?}");
        prop_assert_eq!(r.width(), sw.min(iw));
        prop_assert_eq!(r.height(), sh.min(ih));
        prop_assert_eq!(clipped, sw > iw || sh > ih);
        let (ml, mr) = (r.left - isl.left, isl.right - r.right);
        prop_assert!(mr - ml == 0 || mr - ml == 1, "x margins {ml}/{mr}");
        let (mt, mb) = (r.top - isl.top, isl.bottom - r.bottom);
        prop_assert!(mb - mt == 0 || mb - mt == 1, "y margins {mt}/{mb}");
    }

    #[test]
    fn center_in_extremes_never_panics(isl in any_rect(), sw in any::<i32>(), sh in any::<i32>()) {
        let (r, _) = center_in(isl, Size { width: sw, height: sh });
        if isl.right >= isl.left && isl.bottom >= isl.top && sw >= 0 && sh >= 0 {
            prop_assert!(within(r, isl), "{r:?} not inside {isl:?}");
        }
    }
}

// ----------------------------------------------------------------------- window_state

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn styles_clear_exactly_mask(style in any::<u32>(), ex in any::<u32>()) {
        let (s, e) = borderless_styles(style, ex);
        prop_assert_eq!(s & STYLE_REMOVE, 0);
        prop_assert_eq!(style & !s, style & STYLE_REMOVE);
        prop_assert_eq!(s & !STYLE_REMOVE, style & !STYLE_REMOVE);
        prop_assert_eq!(s | (style & STYLE_REMOVE), style);
        prop_assert_eq!(e & EXSTYLE_REMOVE, 0);
        prop_assert_eq!(e & !EXSTYLE_REMOVE, ex & !EXSTYLE_REMOVE);
        prop_assert_eq!(e | (ex & EXSTYLE_REMOVE), ex);
        prop_assert_eq!(borderless_styles(s, e), (s, e));
    }

    /// Masks spelled out from the Win32 constants, so editing the production constants is caught.
    #[test]
    fn styles_match_win32_constants(style in any::<u32>(), ex in any::<u32>()) {
        // CAPTION | SYSMENU | THICKFRAME | MINIMIZEBOX | MAXIMIZEBOX
        let style_mask = 0x00C0_0000u32 | 0x0008_0000 | 0x0004_0000 | 0x0002_0000 | 0x0001_0000;
        // DLGMODALFRAME | WINDOWEDGE | CLIENTEDGE | STATICEDGE | TOOLWINDOW | APPWINDOW | LAYERED | COMPOSITED
        let ex_mask = 0x1u32 | 0x100 | 0x200 | 0x2_0000 | 0x80 | 0x4_0000 | 0x8_0000 | 0x0200_0000;
        prop_assert_eq!(borderless_styles(style, ex), (style & !style_mask, ex & !ex_mask));
    }

    #[test]
    fn target_rect_never_panics(m in any_rect(), o in any_offsets()) {
        let _ = target_rect(m, o);
    }

    #[test]
    fn target_rect_arithmetic(
        m in monitor(),
        (a, b, c, d) in (-1_000_000i32..1_000_000, -1_000_000i32..1_000_000, -1_000_000i32..1_000_000, -1_000_000i32..1_000_000),
    ) {
        let o = Offsets { left: a, top: b, right: c, bottom: d };
        let t = target_rect(m, o);
        prop_assert_eq!(t.left, m.left + a);
        prop_assert_eq!(t.top, m.top + b);
        prop_assert_eq!(t.width(), m.width() - a + c);
        prop_assert_eq!(t.height(), m.height() - b + d);
        prop_assert_eq!(target_rect(m, Offsets::default()), m);
    }
}

#[test]
fn style_masks_are_the_documented_values() {
    assert_eq!(STYLE_REMOVE, 0x00CF_0000);
    assert_eq!(EXSTYLE_REMOVE, 0x020E_0381);
}

// ------------------------------------------------------------------------------- guard

#[derive(Debug, Clone, Copy)]
enum Op {
    AtTarget,
    Deviated(u8, bool),
    Tick,
    Reassert,
    Reset,
}

fn guard_rect(i: i32) -> Rect {
    rect4(i, 0, 100 + i, 100)
}

fn guard_cfg() -> impl Strategy<Value = GuardConfig> {
    (
        0u64..2000,
        0u32..=5,
        [0u64..1000, 0u64..1000, 0u64..1000],
        0u32..=5,
    )
        .prop_map(
            |(settle_ms, max_reasserts, backoff_ms, cycles_per_minute)| GuardConfig {
                settle_ms,
                max_reasserts,
                backoff_ms,
                cycles_per_minute,
            },
        )
}

fn dt() -> impl Strategy<Value = u64> {
    prop_oneof![5 => 0u64..100, 3 => 0u64..2000, 1 => 55_000u64..130_000]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => Just(Op::AtTarget),
        5 => (0u8..3, any::<bool>()).prop_map(|(r, f)| Op::Deviated(r, f)),
        3 => Just(Op::Tick),
        4 => Just(Op::Reassert),
        1 => Just(Op::Reset),
    ]
}

/// Drives a guard like the engine does and checks the guard's contract from the outside.
fn run_guard(cfg: GuardConfig, start: u64, steps: &[(u64, Op)]) -> Result<(), TestCaseError> {
    let max_backoff = cfg.backoff_ms.iter().copied().max().unwrap_or(0);
    let mut g = Guard::new(cfg, guard_rect(0), start);
    let mut now = start;
    // Last `ReassertAt` the caller has not executed yet.
    let mut announced: Option<u64> = None;
    let mut gave_up = false;
    // Re-asserts closer than `settle_ms` belong to the same cycle.
    let mut last_event: Option<u64> = None;
    let mut streak = 0u32;

    for &(d, op) in steps {
        now += d;
        let was_gave_up = gave_up;
        let was_announced = announced;
        let mut from_tick = false;
        let mut obs_rect = None;
        let action = match op {
            Op::AtTarget => {
                announced = None;
                Some(g.on_observation(now, Observation::AtTarget))
            }
            Op::Deviated(id, frame_back) => {
                let rect = guard_rect(i32::from(id));
                obs_rect = Some(rect);
                Some(g.on_observation(now, Observation::Deviated { rect, frame_back }))
            }
            Op::Tick => {
                from_tick = true;
                Some(g.on_tick(now))
            }
            Op::Reassert => {
                if gave_up {
                    // Must be ignored once abandoned.
                    g.on_reasserted(now);
                } else if let Some(at) = announced
                    && now >= at
                {
                    g.on_reasserted(now);
                    announced = None;
                    streak = match last_event {
                        Some(t) if now - t < cfg.settle_ms => streak + 1,
                        _ => 1,
                    };
                    last_event = Some(now);
                    prop_assert!(
                        streak <= cfg.max_reasserts,
                        "{streak} re-asserts in one cycle, max {}",
                        cfg.max_reasserts
                    );
                }
                None
            }
            Op::Reset => {
                g.reset(guard_rect(1), now);
                announced = None;
                gave_up = false;
                streak = 0;
                last_event = None;
                prop_assert!(!g.fights_back());
                if cfg.max_reasserts >= 1 && cfg.cycles_per_minute >= 1 {
                    let mut probe = g.clone();
                    let a = probe.on_observation(
                        now,
                        Observation::Deviated {
                            rect: guard_rect(2),
                            frame_back: false,
                        },
                    );
                    prop_assert_eq!(a, Action::ReassertAt(now + cfg.backoff_ms[0]));
                }
                None
            }
        };

        if let Some(a) = action {
            if was_gave_up {
                prop_assert_eq!(a, Action::None);
            }
            match a {
                Action::ReassertAt(at) => {
                    if was_announced != Some(at) {
                        prop_assert!(at >= now, "fresh re-assert {at} in the past (now {now})");
                        prop_assert!(
                            at - now <= max_backoff,
                            "backoff {} > {max_backoff}",
                            at - now
                        );
                    }
                    if from_tick {
                        prop_assert!(at <= now, "tick returned a future re-assert {at} at {now}");
                    }
                    announced = Some(at);
                }
                Action::GiveUp { forced } => {
                    prop_assert_eq!(Some(forced), obs_rect);
                    gave_up = true;
                    announced = None;
                }
                Action::Settled | Action::None => {}
            }
        }
        prop_assert_eq!(g.fights_back(), gave_up);
    }
    Ok(())
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn guard_contract_holds_for_any_timeline(
        cfg in guard_cfg(),
        start in 0u64..1_000_000_000,
        steps in vec((dt(), op()), 0..80),
    ) {
        run_guard(cfg, start, &steps)?;
    }
}

// ----------------------------------------------------------------------------- profiles

#[derive(Debug, Clone)]
enum Defect {
    BadRect(IslandRect),
    BadLayoutId(String),
    BadIslandId(String),
    DuplicateLayout,
    DuplicateIsland,
}

fn bad_id() -> impl Strategy<Value = String> {
    prop_oneof![Just(String::new()), "[a-z]{0,4}/[a-z]{0,4}"]
}

/// Rectangles `rect_ok` must reject, each with exactly one broken field.
fn invalid_rect() -> impl Strategy<Value = IslandRect> {
    let neg = prop_oneof![
        Just(-1e-9),
        Just(-1.0),
        Just(-1e300),
        Just(f64::NAN),
        Just(f64::NEG_INFINITY),
        Just(f64::INFINITY),
    ];
    let nonpos = prop_oneof![
        Just(0.0),
        Just(-0.0),
        Just(-0.5),
        Just(-1e300),
        Just(f64::NAN),
        Just(f64::NEG_INFINITY),
        Just(f64::INFINITY),
    ];
    (0u8..6, neg, nonpos, unit()).prop_map(|(which, neg, nonpos, unit)| {
        let mut r = IslandRect {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
            unit,
        };
        match which {
            0 => r.w = nonpos,
            1 => r.h = nonpos,
            2 => r.x = neg,
            3 => r.y = neg,
            4 => match unit {
                Unit::Fraction => (r.x, r.w) = (0.75, 0.5),
                Unit::Px => (r.x, r.w) = (99_999.0, 2.0),
            },
            _ => r.w = 1e300,
        }
        r
    })
}

fn defect() -> impl Strategy<Value = Defect> {
    prop_oneof![
        invalid_rect().prop_map(Defect::BadRect),
        bad_id().prop_map(Defect::BadLayoutId),
        bad_id().prop_map(Defect::BadIslandId),
        Just(Defect::DuplicateLayout),
        Just(Defect::DuplicateIsland),
    ]
}

fn file_with(layouts: Vec<Layout>, profiles: Vec<Profile>) -> ProfileFile {
    ProfileFile {
        version: SUPPORTED_VERSION,
        layouts,
        profiles,
        extra: Default::default(),
    }
}

fn one_island_layout(id: &str, island: &str, rect: IslandRect) -> Layout {
    Layout {
        id: id.into(),
        monitor: MonitorSel::Primary,
        islands: vec![Island {
            id: island.into(),
            rect,
            extra: Default::default(),
        }],
        extra: Default::default(),
    }
}

fn plain_profile(process: &str) -> Profile {
    Profile {
        process: process.into(),
        title: None,
        title_regex: None,
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

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn parse_never_panics(s in any::<String>()) {
        if let Ok(f) = profiles::parse(&s) {
            prop_assert!(f.version <= SUPPORTED_VERSION);
        }
    }

    #[test]
    fn parse_never_panics_on_corrupted_valid_text(file in arb_file(), at in any::<usize>(), ch in any::<char>()) {
        let text = serde_json::to_string(&file).unwrap();
        let idx = text.char_indices().map(|(i, _)| i).nth(at % text.chars().count()).unwrap();
        let mut corrupted = String::with_capacity(text.len() + 4);
        corrupted.push_str(&text[..idx]);
        corrupted.push(ch);
        corrupted.extend(text[idx..].chars().skip(1));
        if let Ok(f) = profiles::parse(&corrupted) {
            prop_assert!(f.version <= SUPPORTED_VERSION);
        }
    }

    #[test]
    fn profile_file_roundtrips(file in arb_file()) {
        let compact = serde_json::to_string(&file).unwrap();
        let pretty = serde_json::to_string_pretty(&file).unwrap();
        prop_assert_eq!(&profiles::parse(&compact).unwrap(), &file);
        prop_assert_eq!(&profiles::parse(&pretty).unwrap(), &file);
    }

    #[test]
    fn duplicate_top_level_key_is_rejected(file in arb_file()) {
        let text = serde_json::to_string(&file).unwrap().replacen('{', r#"{"_dup":0,"_dup":0,"#, 1);
        prop_assert!(matches!(profiles::parse(&text), Err(ProfileError::Parse(_))), "{text}");
    }

    #[test]
    fn upsert_extras_carry_over_or_replace(
        mut file in arb_file(),
        mut layout in arb_layout_any(),
        mut profile in arb_profile(),
        replace in any::<bool>(),
    ) {
        for (j, i) in layout.islands.iter_mut().enumerate() {
            i.id = format!("i{j}");
        }
        profile.process = "target.exe".into();
        file.layouts.retain(|l| l.id != layout.id);
        file.profiles.retain(|p| {
            !p.process.rsplit(['\\', '/']).next().unwrap_or("").eq_ignore_ascii_case("target.exe")
        });
        file.layouts.push(layout.clone());
        file.profiles.push(profile.clone());
        let mut sent_layout = layout;
        sent_layout.extra.clear();
        for i in &mut sent_layout.islands {
            i.extra.clear();
        }
        let mut sent_profile = profile;
        sent_profile.extra.clear();
        let mut expected = file.clone();
        if replace {
            *expected.layouts.last_mut().unwrap() = sent_layout.clone();
            *expected.profiles.last_mut().unwrap() = sent_profile.clone();
        }
        profiles::upsert(&mut file, vec![sent_profile], vec![sent_layout], vec![], vec![], replace);
        prop_assert_eq!(file, expected);
    }

    #[test]
    fn truncated_file_is_rejected(file in arb_file(), cut in any::<usize>()) {
        let pretty = serde_json::to_string_pretty(&file).unwrap();
        let prefix = proper_prefix(&pretty, cut);
        prop_assert!(profiles::parse(prefix).is_err(), "accepted truncated file: {prefix:?}");
    }

    #[test]
    fn version_gate(v in any::<u32>()) {
        let r = profiles::parse(&format!(r#"{{"version":{v}}}"#));
        if v <= SUPPORTED_VERSION {
            let f = r.unwrap();
            prop_assert_eq!(f.version, v);
            prop_assert!(f.layouts.is_empty() && f.profiles.is_empty());
        } else {
            prop_assert!(matches!(r, Err(ProfileError::UnsupportedVersion(x)) if x == v));
        }
    }

    #[test]
    fn valid_layouts_raise_no_warnings(layouts in valid_layouts()) {
        let w = profiles::validate(&file_with(layouts, vec![]));
        prop_assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn defect_is_flagged_exactly(mut base in valid_layouts(), defect in defect()) {
        let (code, data) = match defect {
            Defect::BadRect(rect) => {
                base.push(one_island_layout("bad", "x", rect));
                ("bad_island_rect", json!({"layout": "bad", "island": "x"}))
            }
            Defect::BadLayoutId(id) => {
                base.push(one_island_layout(&id, "i0", IslandRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0, unit: Unit::Fraction }));
                ("bad_layout_id", json!({"layout": id}))
            }
            Defect::BadIslandId(id) => {
                base.push(one_island_layout("bad", &id, IslandRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0, unit: Unit::Fraction }));
                ("bad_island_id", json!({"layout": "bad", "island": id}))
            }
            Defect::DuplicateLayout => {
                let dup = base[0].clone();
                base.push(dup);
                ("duplicate_layout", json!({"layout": "l0"}))
            }
            Defect::DuplicateIsland => {
                let dup = base[0].islands[0].clone();
                base[0].islands.push(dup);
                ("duplicate_island", json!({"layout": "l0", "island": "i0"}))
            }
        };
        let w = profiles::validate(&file_with(base, vec![]));
        prop_assert_eq!(w.len(), 1, "{:?}", w);
        prop_assert_eq!(w[0].code.as_str(), code);
        prop_assert_eq!(&w[0].data, &data);
        prop_assert!(profiles::is_blocking(code));
    }

    #[test]
    fn game_resolution_flagged_iff_not_positive(w in any::<i32>(), h in any::<i32>(), process in any::<String>()) {
        let mut p = plain_profile(&process);
        p.game_resolution = Some(Size { width: w, height: h });
        let warnings = profiles::validate(&file_with(vec![], vec![p]));
        if w <= 0 || h <= 0 {
            prop_assert_eq!(warnings.len(), 1, "{:?}", warnings);
            prop_assert_eq!(warnings[0].code.as_str(), "bad_game_resolution");
            prop_assert_eq!(&warnings[0].data, &json!({"process": process, "width": w, "height": h}));
        } else {
            prop_assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[test]
    fn revision_is_a_stable_digest(bytes in vec(any::<u8>(), 0..256), at in any::<usize>(), flip in 1u8..=255) {
        let r = profiles::revision(&bytes);
        prop_assert_eq!(r.len(), 64);
        prop_assert!(r.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{r}");
        prop_assert_eq!(&profiles::revision(&bytes), &r);
        if !bytes.is_empty() {
            let mut other = bytes.clone();
            other[at % bytes.len()] ^= flip;
            prop_assert_ne!(&profiles::revision(&other), &r);
        }
        let mut longer = bytes.clone();
        longer.push(0);
        prop_assert_ne!(&profiles::revision(&longer), &r);
    }

    /// Property 1: `find` returns a profile whose rule matches, of the lowest level (exact <
    /// regex < process only) among the matching ones, the first in file order at that level;
    /// `None` only if none matches or the process is excluded.
    #[test]
    fn find_picks_the_most_specific_match_in_file_order(
        ps in vec(ruled_profile(), 0..8),
        process in prop::sample::select(PROCESSES),
        title in prop::sample::select(TITLES),
    ) {
        let expected = if profiles::is_excluded(process) {
            None
        } else {
            ps.iter()
                .enumerate()
                .filter(|(_, p)| p.process.eq_ignore_ascii_case(process))
                .filter_map(|(i, p)| level(p, title).map(|l| (l, i)))
                .min()
                .map(|(_, i)| i)
        };
        let got = profiles::find(&ps, process, title)
            .map(|p| ps.iter().position(|q| std::ptr::eq(p, q)).unwrap());
        prop_assert_eq!(got, expected);
    }

    /// Property 2: after `upsert` no two profiles share a key (on a file without duplicates),
    /// and sending the same profiles again changes nothing.
    #[test]
    fn upsert_keeps_keys_unique_and_is_idempotent(
        old in vec(ruled_profile(), 0..8),
        sent in vec(ruled_profile(), 0..6),
    ) {
        let mut file = file_with(vec![], unique_keys(old));
        profiles::upsert(&mut file, sent.clone(), vec![], vec![], vec![], false);
        let keys: Vec<ProfileKey> = file.profiles.iter().map(ProfileKey::of).collect();
        let unique: std::collections::HashSet<&ProfileKey> = keys.iter().collect();
        prop_assert_eq!(unique.len(), keys.len());
        let once = file.clone();
        profiles::upsert(&mut file, sent, vec![], vec![], vec![], false);
        prop_assert_eq!(file, once);
    }

    /// Property 3: removing a key leaves every other profile intact and in order; a process-name
    /// `remove` never deletes a profile with a title rule.
    #[test]
    fn remove_drops_exactly_that_key(
        ps in vec(ruled_profile(), 0..8),
        key in ruled_profile().prop_map(|p| ProfileKey::of(&p)),
        name in prop::sample::select(PROCESSES),
    ) {
        let mut file = file_with(vec![], ps.clone());
        profiles::upsert(&mut file, vec![], vec![], vec![key.clone()], vec![], false);
        let kept: Vec<Profile> = ps.iter().filter(|p| ProfileKey::of(p) != key).cloned().collect();
        prop_assert_eq!(&file.profiles, &kept);
        let mut file = file_with(vec![], ps.clone());
        let by_name: ProfileKey = serde_json::from_value(json!(name)).unwrap();
        profiles::upsert(&mut file, vec![], vec![], vec![by_name], vec![], false);
        let ruled = |v: &[Profile]| -> Vec<Profile> {
            v.iter().filter(|p| p.title.is_some() || p.title_regex.is_some()).cloned().collect()
        };
        prop_assert_eq!(ruled(&file.profiles), ruled(&ps));
    }
}

const PROCESSES: &[&str] = &["a.exe", "A.EXE", "b.exe", "explorer.exe"];
const TITLES: &[&str] = &["Main", " main ", "MAIN 2", "Lev3", "", "("];

/// Profiles over a small alphabet, so keys, titles and processes collide often.
fn ruled_profile() -> impl Strategy<Value = Profile> {
    let rule = prop_oneof![
        Just((None, None)),
        prop::sample::select(vec!["Main", " MAIN", "main 2", "", " "])
            .prop_map(|t| (Some(t.to_owned()), None)),
        prop::sample::select(vec!["^Main$", "(?i)main", "\\d", "(", ""])
            .prop_map(|r| (None, Some(r.to_owned()))),
        Just((Some("Main".to_owned()), Some("Main".to_owned()))),
    ];
    (
        prop::sample::select(PROCESSES),
        rule,
        any::<bool>(),
        0u64..3,
    )
        .prop_map(
            |(process, (title, title_regex), enabled, delay_ms)| Profile {
                title,
                title_regex,
                enabled,
                delay_ms,
                ..plain_profile(process)
            },
        )
}

/// Oracle for the precedence level of `p` for `title`, written from the README rules.
fn level(p: &Profile, title: &str) -> Option<u8> {
    let norm = |s: &str| s.trim().to_lowercase();
    match (&p.title, &p.title_regex) {
        (None, None) => Some(2),
        (Some(t), None) => (!t.trim().is_empty() && norm(t) == norm(title)).then_some(0),
        (None, Some(r)) => regex::Regex::new(r)
            .is_ok_and(|re| !r.is_empty() && re.is_match(title))
            .then_some(1),
        (Some(_), Some(_)) => None,
    }
}

/// `ps` without the profiles whose key an earlier one already has.
fn unique_keys(ps: Vec<Profile>) -> Vec<Profile> {
    let mut seen = std::collections::HashSet::new();
    ps.into_iter()
        .filter(|p| seen.insert(ProfileKey::of(p)))
        .collect()
}

#[test]
fn revision_known_vectors() {
    assert_eq!(
        profiles::revision(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        profiles::revision(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn deeply_nested_profile_file_is_rejected_without_overflow() {
    let n = 100_000;
    let text = format!(
        r#"{{"version":2,"profiles":{}{}}}"#,
        "[".repeat(n),
        "]".repeat(n)
    );
    assert!(profiles::parse(&text).is_err());
}

// ---------------------------------------------------------------------------------- ipc

fn text() -> impl Strategy<Value = String> {
    prop_oneof![any::<String>(), "[a-z\n\r ]{0,8}"]
}

/// JSON without floats, nested up to four levels.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        text().prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..4).prop_map(Value::Array),
            vec((text(), inner), 0..4).prop_map(|kv| Value::Object(kv.into_iter().collect())),
        ]
    })
}

/// Objects that sometimes carry well-typed `id` / `method`, so both outcomes are exercised.
fn maybe_request() -> impl Strategy<Value = Value> {
    let opt = || proptest::option::of(json_value());
    (opt(), opt(), opt()).prop_map(|(id, method, params)| {
        let mut o = serde_json::Map::new();
        for (k, v) in [("id", id), ("method", method), ("params", params)] {
            if let Some(v) = v {
                o.insert(k.into(), v);
            }
        }
        Value::Object(o)
    })
}

fn arb_id() -> impl Strategy<Value = Id> {
    prop_oneof![any::<i64>().prop_map(Id::Num), text().prop_map(Id::Str)]
}

fn arb_request() -> impl Strategy<Value = Request> {
    (arb_id(), text(), json_value()).prop_map(|(id, method, params)| Request { id, method, params })
}

fn arb_window_ref() -> impl Strategy<Value = WindowRef> {
    (
        proptest::option::of(any::<isize>()),
        proptest::option::of(any::<String>()),
        proptest::option::of(any::<String>()),
    )
        .prop_map(|(hwnd, process, title)| WindowRef {
            hwnd,
            process,
            title,
        })
}

fn arb_list_windows() -> impl Strategy<Value = ListWindowsParams> {
    (
        proptest::option::of(any::<String>()),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(process, include_hidden, only_managed)| ListWindowsParams {
                process,
                include_hidden,
                only_managed,
            },
        )
}

fn arb_apply() -> impl Strategy<Value = ApplyParams> {
    (
        arb_window_ref(),
        proptest::option::of(any::<String>()),
        proptest::option::of(arb_monitor_sel()),
        proptest::option::of(any_offsets()),
        proptest::option::of(any::<u64>()),
        any::<bool>(),
    )
        .prop_map(
            |(window, island, monitor, offsets, delay_ms, use_profile)| ApplyParams {
                window,
                island,
                monitor,
                offsets,
                delay_ms,
                use_profile,
            },
        )
}

fn arb_place() -> impl Strategy<Value = PlaceParams> {
    (
        arb_window_ref(),
        any::<String>(),
        proptest::option::of(any_size()),
    )
        .prop_map(|(window, island, game_resolution)| PlaceParams {
            window,
            island,
            game_resolution,
        })
}

fn arb_diagnose() -> impl Strategy<Value = DiagnoseParams> {
    (
        arb_window_ref(),
        proptest::option::of(any::<String>()),
        proptest::option::of(arb_monitor_sel()),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(
            |(window, island, monitor, observe_ms, keep)| DiagnoseParams {
                window,
                island,
                monitor,
                observe_ms,
                keep,
            },
        )
}

/// A `remove` item: a process name or a key object, normalized like the resident reads it.
fn arb_key() -> impl Strategy<Value = ProfileKey> {
    prop_oneof![
        any::<String>().prop_map(|s| ProfileKey::from(s.as_str())),
        (
            any::<String>(),
            proptest::option::of(any::<String>()),
            proptest::option::of(any::<String>()),
        )
            .prop_map(|(p, t, r)| ProfileKey::new(&p, t.as_deref(), r.as_deref())),
    ]
}

fn arb_set_profiles() -> impl Strategy<Value = SetProfilesParams> {
    (
        proptest::option::of(vec(arb_profile(), 0..3)),
        proptest::option::of(vec(arb_layout_any(), 0..3)),
        proptest::option::of(vec(arb_key(), 0..3)),
        vec(any::<String>(), 0..3),
        proptest::option::of(any::<String>()),
        any::<bool>(),
    )
        .prop_map(
            |(profiles, layouts, remove, remove_layouts, expected_revision, replace_extras)| {
                SetProfilesParams {
                    profiles,
                    layouts,
                    remove,
                    remove_layouts,
                    expected_revision,
                    replace_extras,
                }
            },
        )
}

/// `p` survives `to_value` → `parse_params`, `encode` → `decode_request` → `parse_params`, and an
/// extra unknown field. Since `parse_params` runs the schema walk, this is also P1: every valid
/// params value passes it.
fn params_roundtrip<T>(p: &T) -> Result<(), TestCaseError>
where
    T: Serialize + DeserializeOwned + JsonSchema + PartialEq + std::fmt::Debug,
{
    let v = serde_json::to_value(p).unwrap();
    prop_assert_eq!(&ipc::parse_params::<T>(v.clone()).unwrap(), p);

    let req = Request {
        id: Id::Num(1),
        method: "m".into(),
        params: v.clone(),
    };
    let back = ipc::decode_request(&ipc::encode(&req).unwrap()).unwrap();
    prop_assert_eq!(&ipc::parse_params::<T>(back.params).unwrap(), p);

    if let Value::Object(mut o) = v {
        o.insert("zz_unknown".into(), json!(1));
        prop_assert_eq!(&ipc::parse_params::<T>(Value::Object(o)).unwrap(), p);
    }
    Ok(())
}

/// JSON Pointers of every object node of `v`, root (`""`) included.
fn object_pointers(v: &Value, at: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            out.push(at.to_owned());
            for (k, c) in o {
                object_pointers(c, &format!("{at}/{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, c) in a.iter().enumerate() {
                object_pointers(c, &format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// P2: replacing one struct node of `p` by the array of its values is `invalid_params`, reported
/// at that node. Objects inside extras (keys starting with `_`) are free-form, so none is picked.
fn positional_struct_is_rejected<T>(p: &T, pick: prop::sample::Index) -> Result<(), TestCaseError>
where
    T: Serialize + DeserializeOwned + JsonSchema + std::fmt::Debug,
{
    let mut v = serde_json::to_value(p).unwrap();
    let mut pointers = Vec::new();
    object_pointers(&v, "", &mut pointers);
    pointers.retain(|at| !at.split('/').any(|s| s.starts_with('_')));
    let at = pick.get(&pointers).clone();
    let node = v.pointer_mut(&at).unwrap();
    let values = node.as_object().unwrap().values().cloned().collect();
    *node = Value::Array(values);
    let e = ipc::parse_params::<T>(v.clone()).unwrap_err();
    prop_assert_eq!(e.code, ErrorCode::InvalidParams, "{}", v);
    let want = format!("unexpected array at \"{at}\"");
    prop_assert!(e.message.contains(&want), "{}: {}", v, e.message);
    Ok(())
}

/// `s` as a JSON string literal with every UTF-16 unit escaped (`\uXXXX`).
fn escaped(s: &str) -> String {
    let units: String = s.encode_utf16().map(|u| format!("\\u{u:04x}")).collect();
    format!("\"{units}\"")
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn decode_never_panics(s in any::<String>()) {
        if let Err(e) = ipc::decode_request(&s) {
            prop_assert!(matches!(e.code, ErrorCode::InvalidJson | ErrorCode::InvalidParams), "{:?}", e.code);
        }
    }

    #[test]
    fn decode_never_panics_on_lossy_bytes(bytes in vec(any::<u8>(), 0..2048)) {
        let s = String::from_utf8_lossy(&bytes);
        if let Err(e) = ipc::decode_request(&s) {
            prop_assert!(matches!(e.code, ErrorCode::InvalidJson | ErrorCode::InvalidParams), "{:?}", e.code);
        }
    }

    #[test]
    fn classification_matches_oracle(v in prop_oneof![json_value(), maybe_request()]) {
        let line = serde_json::to_string(&v).unwrap();
        let is_request = v.as_object().is_some_and(|o| {
            o.get("id").is_some_and(|i| i.is_i64() || i.is_string())
                && o.get("method").is_some_and(Value::is_string)
        });
        match ipc::decode_request(&line) {
            Ok(_) => prop_assert!(is_request, "accepted a non-request: {line}"),
            Err(e) => {
                prop_assert!(!is_request, "rejected a request: {line}");
                prop_assert_eq!(e.code, ErrorCode::InvalidParams);
            }
        }
    }

    #[test]
    fn request_roundtrip(req in arb_request()) {
        let line = ipc::encode(&req).unwrap();
        prop_assert!(line.ends_with('\n'));
        prop_assert_eq!(line.matches('\n').count(), 1);
        let body = line.trim_end_matches('\n');
        prop_assert_eq!(&ipc::decode_request(&line).unwrap(), &req);
        prop_assert_eq!(&ipc::decode_request(body).unwrap(), &req);
        prop_assert_eq!(&ipc::decode_request(&format!("{body}\r\n")).unwrap(), &req);
    }

    #[test]
    fn truncated_request_is_invalid_json(req in arb_request(), cut in any::<usize>()) {
        let line = ipc::encode(&req).unwrap();
        let prefix = proper_prefix(line.trim_end_matches('\n'), cut);
        let e = ipc::decode_request(prefix).unwrap_err();
        prop_assert_eq!(e.code, ErrorCode::InvalidJson, "{}", prefix);
    }

    #[test]
    fn list_windows_params_roundtrip(p in arb_list_windows()) { params_roundtrip(&p)?; }
    #[test]
    fn apply_params_roundtrip(p in arb_apply()) { params_roundtrip(&p)?; }
    #[test]
    fn place_params_roundtrip(p in arb_place()) { params_roundtrip(&p)?; }
    #[test]
    fn restore_params_roundtrip(w in arb_window_ref()) { params_roundtrip(&RestoreParams { window: w })?; }
    #[test]
    fn diagnose_params_roundtrip(p in arb_diagnose()) { params_roundtrip(&p)?; }
    #[test]
    fn set_profiles_params_roundtrip(p in arb_set_profiles()) { params_roundtrip(&p)?; }
    #[test]
    fn window_ref_roundtrip(w in arb_window_ref()) { params_roundtrip(&w)?; }

    #[test]
    fn apply_rejects_a_positional_struct(p in arb_apply(), i in any::<prop::sample::Index>()) {
        positional_struct_is_rejected(&p, i)?;
    }
    #[test]
    fn place_rejects_a_positional_struct(p in arb_place(), i in any::<prop::sample::Index>()) {
        positional_struct_is_rejected(&p, i)?;
    }
    #[test]
    fn restore_rejects_a_positional_struct(w in arb_window_ref(), i in any::<prop::sample::Index>()) {
        positional_struct_is_rejected(&RestoreParams { window: w }, i)?;
    }
    #[test]
    fn diagnose_rejects_a_positional_struct(p in arb_diagnose(), i in any::<prop::sample::Index>()) {
        positional_struct_is_rejected(&p, i)?;
    }
    #[test]
    fn set_profiles_rejects_a_positional_struct(p in arb_set_profiles(), i in any::<prop::sample::Index>()) {
        positional_struct_is_rejected(&p, i)?;
    }

    /// P1 for extras: arrays inside unknown fields of profiles, layouts and islands are kept, and
    /// an unknown top-level array is ignored.
    #[test]
    fn set_profiles_accepts_arrays_in_extras(p in arb_set_profiles(), extra in vec(json_value(), 0..3)) {
        let mut v = serde_json::to_value(&p).unwrap();
        let arr = Value::Array(extra);
        let put = |o: &mut Value| { o["_arr"] = arr.clone(); };
        for profile in v.get_mut("profiles").and_then(Value::as_array_mut).into_iter().flatten() {
            put(profile);
        }
        for layout in v.get_mut("layouts").and_then(Value::as_array_mut).into_iter().flatten() {
            put(layout);
            for island in layout.get_mut("islands").and_then(Value::as_array_mut).into_iter().flatten() {
                put(island);
            }
        }
        let mut sent = v.clone();
        sent["zz_unknown"] = arr.clone();
        let back: SetProfilesParams = ipc::parse_params(sent).unwrap();
        prop_assert_eq!(serde_json::to_value(&back).unwrap(), v);
    }

    /// P3: a key repeated in params or in the envelope is `invalid_params`, also when the second
    /// copy is fully `\u`-escaped; distinct keys are accepted.
    #[test]
    fn repeated_keys_are_invalid_params(
        k in text(), k2 in text(), a in json_value(), b in json_value(), escape in any::<bool>(),
    ) {
        let key = serde_json::to_string(&k).unwrap();
        let dup = if escape { escaped(&k) } else { key.clone() };
        let (a, b) = (a.to_string(), b.to_string());
        for line in [
            format!(r#"{{"id":1,"method":"m","params":{{{key}:{a},{dup}:{b}}}}}"#),
            format!(r#"{{{key}:{a},{dup}:{b},"id":1,"method":"m"}}"#),
        ] {
            let e = ipc::decode_request(&line).unwrap_err();
            prop_assert_eq!(e.code, ErrorCode::InvalidParams, "{}", line);
        }
        prop_assume!(k != k2);
        let key2 = serde_json::to_string(&k2).unwrap();
        let line = format!(r#"{{"id":1,"method":"m","params":{{{key}:{a},{key2}:{b}}}}}"#);
        prop_assert!(ipc::decode_request(&line).is_ok(), "{}", line);
    }

    #[test]
    fn hex_is_fixed_width_lowercase(v in any::<u32>()) {
        let h = ipc::hex(v);
        prop_assert_eq!(h.len(), 10);
        prop_assert!(h.starts_with("0x"));
        prop_assert!(!h.bytes().any(|b| b.is_ascii_uppercase()), "{h}");
        prop_assert_eq!(u32::from_str_radix(&h[2..], 16).unwrap(), v);
    }
}

#[test]
fn empty_params_decode_from_null() {
    use ipc::{GetProfilesParams, HelloParams, ListIslandsParams, RestoreAllParams};
    assert_eq!(
        ipc::parse_params::<HelloParams>(Value::Null).unwrap(),
        HelloParams::default()
    );
    assert_eq!(
        ipc::parse_params::<RestoreAllParams>(Value::Null).unwrap(),
        RestoreAllParams::default()
    );
    assert_eq!(
        ipc::parse_params::<ListIslandsParams>(Value::Null).unwrap(),
        ListIslandsParams::default()
    );
    assert_eq!(
        ipc::parse_params::<GetProfilesParams>(Value::Null).unwrap(),
        GetProfilesParams::default()
    );
    assert_eq!(
        ipc::parse_params::<ListWindowsParams>(Value::Null).unwrap(),
        ListWindowsParams::default()
    );
}

#[test]
fn params_defaults() {
    let a: ApplyParams = ipc::parse_params(json!({"window": {}})).unwrap();
    assert!(a.use_profile);
    assert_eq!(
        (a.island, a.monitor, a.offsets, a.delay_ms),
        (None, None, None, None)
    );
    let d: DiagnoseParams = ipc::parse_params(json!({"window": {}})).unwrap();
    assert_eq!(d.observe_ms, 3000);
    assert!(!d.keep);
}

/// A request line of exactly `len` bytes (without the line break).
fn request_of_len(len: usize) -> String {
    let (head, tail) = (r#"{"id":1,"method":""#, r#""}"#);
    format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
}

#[test]
fn line_length_limit_is_exact() {
    let ok = request_of_len(ipc::MAX_LINE);
    assert_eq!(ok.len(), ipc::MAX_LINE);
    assert!(ipc::decode_request(&ok).is_ok());
    assert!(ipc::decode_request(&format!("{ok}\r\n")).is_ok());

    let long = request_of_len(ipc::MAX_LINE + 1);
    assert_eq!(
        ipc::decode_request(&long).unwrap_err().code,
        ErrorCode::InvalidJson
    );
    assert_eq!(
        ipc::decode_request(&"a".repeat(ipc::MAX_LINE + 1))
            .unwrap_err()
            .code,
        ErrorCode::InvalidJson
    );
}

#[test]
fn deeply_nested_params_are_rejected_without_overflow() {
    let n = 100_000;
    let line = format!(
        r#"{{"id":1,"method":"m","params":{}{}}}"#,
        "[".repeat(n),
        "]".repeat(n)
    );
    assert!(line.len() < ipc::MAX_LINE);
    assert_eq!(
        ipc::decode_request(&line).unwrap_err().code,
        ErrorCode::InvalidJson
    );
}

#[test]
fn encode_limit_is_exact() {
    let resp = |n: usize| ipc::Response::ok(Id::Num(1), Value::String("x".repeat(n)));
    let overhead = serde_json::to_string(&resp(0)).unwrap().len();
    let line = ipc::encode(&resp(ipc::MAX_LINE - overhead)).unwrap();
    assert_eq!(line.len(), ipc::MAX_LINE + 1);
    assert!(matches!(
        ipc::encode(&resp(ipc::MAX_LINE - overhead + 1)),
        Err(IpcError::LineTooLong)
    ));
}
