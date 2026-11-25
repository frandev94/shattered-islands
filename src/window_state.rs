//! Pure window-state model and geometry math (no Win32 types).

/// CAPTION|SYSMENU|THICKFRAME|MINIMIZEBOX|MAXIMIZEBOX
pub const STYLE_REMOVE: u32 = 0x00CF_0000;
/// DLGMODALFRAME|WINDOWEDGE|CLIENTEDGE|STATICEDGE|TOOLWINDOW|APPWINDOW|LAYERED|COMPOSITED
pub const EXSTYLE_REMOVE: u32 = 0x020E_0381;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Offsets {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    pub hwnd: isize,
    pub style: u32,
    pub exstyle: u32,
    pub rect: Rect,
    pub maximized: bool,
}

pub fn has_targetable_style(style: u32) -> bool {
    style & STYLE_REMOVE != 0
}

pub fn borderless_styles(style: u32, exstyle: u32) -> (u32, u32) {
    (style & !STYLE_REMOVE, exstyle & !EXSTYLE_REMOVE)
}

pub fn target_rect(monitor: Rect, offsets: Offsets) -> Rect {
    let x = monitor.left + offsets.left;
    let y = monitor.top + offsets.top;
    let w = monitor.width() - offsets.left + offsets.right;
    let h = monitor.height() - offsets.top + offsets.bottom;
    Rect {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// `None` or an out-of-range index falls back to `current`.
pub fn pick_monitor(monitors: &[Rect], current: Rect, index: Option<usize>) -> Rect {
    index
        .and_then(|i| monitors.get(i).copied())
        .unwrap_or(current)
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
    fn targetable_style() {
        assert!(has_targetable_style(0x14CF0000));
        assert!(!has_targetable_style(0x14000000));
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
    fn pick_monitor_cases() {
        let ms = [R(0, 0, 10, 10), R(10, 0, 20, 10)];
        let cur = R(1, 1, 2, 2);
        assert_eq!(pick_monitor(&ms, cur, None), cur);
        assert_eq!(pick_monitor(&ms, cur, Some(1)), ms[1]);
        assert_eq!(pick_monitor(&ms, cur, Some(2)), cur);
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
