//! Pure model for dimming unfocused windows: what one display's dim overlay
//! shows and which window it sits under.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::actor::app::WindowId;
use crate::common::config::{DimOtherDisplays, DimSettings};
use crate::model::border::{BorderWindow, FullscreenKind};

/// What the dim overlay of one display does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DimPlan {
    /// The window left undimmed, with its WindowServer id: the overlay is
    /// ordered just below it. `None` dims everything on the display.
    pub keep: Option<(WindowId, u32)>,
    /// The undimmed area in display-local CG coordinates: that window's frame
    /// grown by the border width, so its border stays undimmed too.
    pub hole: Option<CGRect>,
}

/// The dim overlay for one display, or `None` to show nothing.
///
/// `focused` is the globally focused window; `last_focused` the window focus
/// last sat on on this display. Nothing shows while a `toggle_fullscreen`
/// window is on the display (it covers everything anyway) or while a window to
/// keep has no WindowServer id to order against.
pub fn compute(
    display: CGRect,
    windows: &[BorderWindow],
    focused: Option<WindowId>,
    last_focused: Option<WindowId>,
    border_width: f64,
    settings: &DimSettings,
) -> Option<DimPlan> {
    if !settings.enabled {
        return None;
    }
    let visible = |id: WindowId| windows.iter().find(|window| window.id == id && window.visible);
    if windows
        .iter()
        .any(|window| window.visible && window.fullscreen == Some(FullscreenKind::Full))
    {
        return None;
    }
    let keep = match focused.and_then(visible) {
        Some(window) => Some(window),
        None => match settings.other_displays {
            DimOtherDisplays::Dim => None,
            DimOtherDisplays::KeepLastFocused => Some(last_focused.and_then(visible)?),
        },
    };
    let (keep, hole) = match keep {
        Some(window) => {
            let frame = window.frame;
            let grow = border_width.max(0.0);
            let hole = CGRect::new(
                CGPoint::new(
                    frame.origin.x - display.origin.x - grow,
                    frame.origin.y - display.origin.y - grow,
                ),
                CGSize::new(frame.size.width + 2.0 * grow, frame.size.height + 2.0 * grow),
            );
            (Some((window.id, window.server_id?)), Some(hole))
        }
        None => (None, None),
    };
    Some(DimPlan { keep, hole })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn window(idx: u32, frame: CGRect) -> BorderWindow {
        BorderWindow {
            id: WindowId::new(1, idx),
            server_id: Some(100 + idx),
            frame,
            focused: false,
            floating: false,
            fullscreen: None,
            visible: true,
        }
    }

    const DISPLAY: CGRect = CGRect {
        origin: CGPoint { x: 1000.0, y: 0.0 },
        size: CGSize { width: 1000.0, height: 800.0 },
    };

    fn settings() -> DimSettings {
        DimSettings {
            enabled: true,
            ..DimSettings::default()
        }
    }

    #[test]
    fn focused_window_gets_a_hole_grown_by_the_border_width() {
        let windows = [
            window(1, rect(1008.0, 8.0, 484.0, 784.0)),
            window(2, rect(1508.0, 8.0, 484.0, 784.0)),
        ];
        let plan = compute(DISPLAY, &windows, Some(windows[1].id), None, 4.0, &settings());
        assert_eq!(
            plan,
            Some(DimPlan {
                keep: Some((windows[1].id, 102)),
                hole: Some(rect(504.0, 4.0, 492.0, 792.0)),
            })
        );
        assert!(
            compute(
                DISPLAY,
                &windows,
                Some(windows[1].id),
                None,
                4.0,
                &DimSettings::default()
            )
            .is_none()
        );
    }

    #[test]
    fn other_displays_keep_their_last_focused_window_or_dim_everything() {
        let windows = [window(1, rect(1008.0, 8.0, 484.0, 784.0))];
        let elsewhere = WindowId::new(2, 1);
        let keep = settings();
        let plan = compute(
            DISPLAY,
            &windows,
            Some(elsewhere),
            Some(windows[0].id),
            0.0,
            &keep,
        );
        assert_eq!(plan.unwrap().keep, Some((windows[0].id, 101)));
        assert_eq!(plan.unwrap().hole, Some(rect(8.0, 8.0, 484.0, 784.0)));
        // No window ever focused here: nothing to keep, so nothing shows.
        assert_eq!(
            compute(DISPLAY, &windows, Some(elsewhere), None, 0.0, &keep),
            None
        );

        let dim = DimSettings {
            other_displays: DimOtherDisplays::Dim,
            ..settings()
        };
        assert_eq!(
            compute(
                DISPLAY,
                &windows,
                Some(elsewhere),
                Some(windows[0].id),
                0.0,
                &dim
            ),
            Some(DimPlan { keep: None, hole: None })
        );
        assert_eq!(
            compute(DISPLAY, &windows, None, None, 0.0, &dim),
            Some(DimPlan { keep: None, hole: None })
        );
    }

    #[test]
    fn fullscreen_hidden_and_unidentified_windows_show_nothing() {
        let mut full = window(1, DISPLAY);
        full.fullscreen = Some(FullscreenKind::Full);
        let other = window(2, rect(1008.0, 8.0, 484.0, 784.0));
        assert_eq!(
            compute(DISPLAY, &[full, other], Some(other.id), None, 4.0, &settings()),
            None
        );

        // Within gaps still leaves the display visible around it: dim as usual.
        full.fullscreen = Some(FullscreenKind::WithinGaps);
        assert!(compute(DISPLAY, &[full, other], Some(full.id), None, 4.0, &settings()).is_some());

        let mut hidden = window(3, rect(1008.0, 8.0, 484.0, 784.0));
        hidden.visible = false;
        assert_eq!(
            compute(DISPLAY, &[hidden], Some(hidden.id), None, 4.0, &settings()),
            None
        );

        let mut unknown = window(4, rect(1008.0, 8.0, 484.0, 784.0));
        unknown.server_id = None;
        assert_eq!(
            compute(DISPLAY, &[unknown], Some(unknown.id), None, 4.0, &settings()),
            None
        );
    }
}
