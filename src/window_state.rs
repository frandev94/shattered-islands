//! Pure window-state model and geometry math (no Win32 types).
//!
//! All coordinates are physical pixels in virtual-desktop space; `right`/`bottom` are exclusive.
//! Arithmetic saturates instead of overflowing.
//!
//! ```
//! use borderless_shatter::window_state::{Offsets, Rect, target_rect};
//!
//! let monitor = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };
//! // Crop 8 px off the left edge, extend 8 px past the right edge.
//! let offsets = Offsets { left: 8, top: 0, right: 8, bottom: 0 };
//! let r = target_rect(monitor, offsets);
//! assert_eq!((r.left, r.right), (8, 1928));
//! ```

/// Style bits removed to make a window borderless:
/// CAPTION|SYSMENU|THICKFRAME|MINIMIZEBOX|MAXIMIZEBOX
pub const STYLE_REMOVE: u32 = 0x00CF_0000;
/// Extended-style bits removed to make a window borderless:
/// DLGMODALFRAME|WINDOWEDGE|CLIENTEDGE|STATICEDGE|TOOLWINDOW|APPWINDOW|LAYERED|COMPOSITED
pub const EXSTYLE_REMOVE: u32 = 0x020E_0381;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
/// Rectangle in physical pixels; `right` and `bottom` are exclusive.
pub struct Rect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge (exclusive).
    pub right: i32,
    /// Bottom edge (exclusive).
    pub bottom: i32,
}

impl Rect {
    /// `right - left`, saturating.
    pub fn width(&self) -> i32 {
        self.right.saturating_sub(self.left)
    }
    /// `bottom - top`, saturating.
    pub fn height(&self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(default)]
/// Per-edge adjustment of a monitor rectangle, in physical pixels. Missing fields default to 0.
///
/// `left`/`top` move the origin inward and shrink the size; `right`/`bottom` grow the size.
pub struct Offsets {
    /// Pixels cropped from the left edge.
    pub left: i32,
    /// Pixels cropped from the top edge.
    pub top: i32,
    /// Pixels added past the right edge.
    pub right: i32,
    /// Pixels added past the bottom edge.
    pub bottom: i32,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
/// Snapshot of a window taken before it was made borderless; restoring replays it verbatim.
pub struct WindowState {
    /// Window handle value.
    pub hwnd: isize,
    /// Original `GWL_STYLE` bits.
    pub style: u32,
    /// Original `GWL_EXSTYLE` bits.
    pub exstyle: u32,
    /// Original outer rectangle.
    pub rect: Rect,
    /// Whether the window was maximized.
    pub maximized: bool,
}

/// Returns `(style, exstyle)` with [`STYLE_REMOVE`] / [`EXSTYLE_REMOVE`] bits cleared.
/// Idempotent; other bits are untouched.
///
/// ```
/// use borderless_shatter::window_state::{borderless_styles, STYLE_REMOVE};
///
/// let (style, ex) = borderless_styles(0x1000_0000 | STYLE_REMOVE, 0);
/// assert_eq!((style, ex), (0x1000_0000, 0));
/// ```
pub fn borderless_styles(style: u32, exstyle: u32) -> (u32, u32) {
    (style & !STYLE_REMOVE, exstyle & !EXSTYLE_REMOVE)
}

/// Rectangle a window occupies on `monitor` after applying `offsets` (see [`Offsets`]).
pub fn target_rect(monitor: Rect, offsets: Offsets) -> Rect {
    let x = monitor.left.saturating_add(offsets.left);
    let y = monitor.top.saturating_add(offsets.top);
    let w = monitor
        .width()
        .saturating_sub(offsets.left)
        .saturating_add(offsets.right);
    let h = monitor
        .height()
        .saturating_sub(offsets.top)
        .saturating_add(offsets.bottom);
    Rect {
        left: x,
        top: y,
        right: x.saturating_add(w),
        bottom: y.saturating_add(h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: fn(i32, i32, i32, i32) -> Rect = |left, top, right, bottom| Rect {
        left,
        top,
        right,
        bottom,
    };

    #[test]
    fn borderless_clears_only_mask_bits() {
        let style = 0x10000000 | 0x02000000 | STYLE_REMOVE;
        let ex = 0x8 | EXSTYLE_REMOVE;
        let (s, e) = borderless_styles(style, ex);
        assert_eq!(s, 0x12000000);
        assert_eq!(e, 0x8);
    }

    #[test]
    fn borderless_is_idempotent_and_restore_is_saved_value() {
        for (style, ex) in [
            (0x14CF_0000u32, 0x0000_0110u32),
            (0x16CF_0000, 0x0004_0100),
            (0x14CF_0000, 0x0004_0100),
            (0x16CF_0000, 0x0000_0110),
        ] {
            let once = borderless_styles(style, ex);
            assert_eq!(borderless_styles(once.0, once.1), once);
            assert_eq!(once.0 | (style & STYLE_REMOVE), style);
            assert_eq!(once.1 | (ex & EXSTYLE_REMOVE), ex);
        }
    }

    #[test]
    fn target_rect_zero_offsets_is_monitor() {
        let m = R(0, 0, 1920, 1080);
        assert_eq!(target_rect(m, Offsets::default()), m);
    }

    #[test]
    fn target_rect_with_offsets() {
        let o = Offsets {
            left: 10,
            top: 20,
            right: 30,
            bottom: 40,
        };
        let t = target_rect(R(0, 0, 1920, 1080), o);
        assert_eq!((t.left, t.top, t.width(), t.height()), (10, 20, 1940, 1100));
    }

    #[test]
    fn target_rect_negative_monitor() {
        let m = R(-1920, 0, 0, 1080);
        assert_eq!(target_rect(m, Offsets::default()), m);
        let t = target_rect(
            m,
            Offsets {
                left: 5,
                ..Default::default()
            },
        );
        assert_eq!((t.left, t.width()), (-1915, 1915));
    }

    #[test]
    fn extreme_offsets_do_not_panic() {
        for v in [i32::MAX, i32::MIN] {
            let o = Offsets {
                left: v,
                top: v,
                right: v,
                bottom: v,
            };
            let _ = target_rect(R(i32::MIN, i32::MIN, i32::MAX, i32::MAX), o);
            let _ = target_rect(R(0, 0, 1920, 1080), o);
        }
    }

    #[test]
    fn offsets_deserialize() {
        let o: Offsets = serde_json::from_str("{}").unwrap();
        assert_eq!(o, Offsets::default());
        let o: Offsets = serde_json::from_str(r#"{"top":7}"#).unwrap();
        assert_eq!(
            o,
            Offsets {
                top: 7,
                ..Default::default()
            }
        );
    }
}
