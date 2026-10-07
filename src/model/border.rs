//! Pure window border model: which strokes a display's overlay draws.
//!
//! The reactor describes the windows it manages on a display; this module turns
//! that into stroke rectangles, colors and widths, independent of SkyLight and
//! Core Animation so the policy is unit-testable.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::actor::app::WindowId;
use crate::common::config::{BorderSettings, Color, Settings};

/// How an overlay moves its strokes to new frames: in the compositor, over the
/// same time and curve as rift's own window animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderAnimation {
    pub duration: f64,
}

impl BorderAnimation {
    /// Cubic-bezier control points approximating rift's ease-in-out-circ
    /// window easing, for a `CAMediaTimingFunction`.
    pub const CONTROL_POINTS: [f32; 4] = [0.85, 0.0, 0.15, 1.0];

    /// The animation a layout pass runs, under the same conditions rift uses
    /// to animate the windows themselves (`AnimationManager::animate_layout`).
    pub fn for_layout(
        settings: &Settings,
        layout_animate: Option<bool>,
        is_resize: bool,
        low_power: bool,
    ) -> Option<Self> {
        let animate = !is_resize
            && layout_animate.unwrap_or(settings.animate)
            && !(layout_animate.is_none() && low_power);
        (animate && settings.animation_duration > 0.0).then_some(Self {
            duration: settings.animation_duration,
        })
    }
}

/// Which rift fullscreen command a window is under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FullscreenKind {
    /// `toggle_fullscreen`: the window covers the whole display, no gap left to
    /// draw in.
    Full,
    /// `toggle_fullscreen_within_gaps`: the outer gaps remain.
    WithinGaps,
}

/// One managed window as the reactor sees it after a layout pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderWindow {
    pub id: WindowId,
    /// The WindowServer id, when known; other overlays order against it.
    pub server_id: Option<u32>,
    /// Target frame in global CG (top-left origin) coordinates.
    pub frame: CGRect,
    pub focused: bool,
    pub floating: bool,
    pub fullscreen: Option<FullscreenKind>,
    /// False for windows parked in an inactive workspace or otherwise hidden.
    pub visible: bool,
}

/// A stroke around one window, in display-local CG coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub id: WindowId,
    /// The window frame, relative to the display origin.
    pub frame: CGRect,
    pub width: f64,
    /// Corner radius of the window edge the stroke follows.
    pub radius: f64,
    pub color: Color,
}

/// The two rounded rectangles whose difference is the stroke ring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    pub outer: CGRect,
    pub outer_radius: f64,
    pub inner: CGRect,
    pub inner_radius: f64,
}

impl Stroke {
    /// The ring drawn outside the window frame, in the gap.
    pub fn outside_ring(&self) -> Ring {
        Ring {
            outer: inset(self.frame, -self.width),
            outer_radius: self.radius + self.width,
            inner: self.frame,
            inner_radius: self.radius,
        }
    }

    /// The ring drawn just inside the window frame, for the fullscreen exception.
    pub fn inline_ring(&self) -> Ring {
        Ring {
            outer: self.frame,
            outer_radius: self.radius,
            inner: inset(self.frame, self.width),
            inner_radius: (self.radius - self.width).max(0.0),
        }
    }
}

/// What one display's overlays draw.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DisplayBorders {
    /// Strokes in the overlay below all app windows, sorted by window id.
    pub below: Vec<Stroke>,
    /// The single stroke drawn above a `Full` fullscreen window.
    pub inline: Option<Stroke>,
}

impl DisplayBorders {
    pub fn is_empty(&self) -> bool { self.below.is_empty() && self.inline.is_none() }
}

/// Compute the strokes for one display.
///
/// `display` is the display frame in global CG coordinates; `windows` are the
/// windows the reactor manages on it. Hidden windows get no stroke. While any
/// window is in rift fullscreen only fullscreen windows are stroked: the
/// others sit under it, and their rings would otherwise peek out around its
/// edges and corners. A window that covers the display (no gap anywhere) gets
/// no outside stroke since none of it could be seen; `Full` fullscreen windows
/// instead get the inline stroke.
pub fn compute(
    display: CGRect,
    windows: &[BorderWindow],
    settings: &BorderSettings,
) -> DisplayBorders {
    let visible: Vec<&BorderWindow> = windows.iter().filter(|window| window.visible).collect();
    let any_fullscreen = visible.iter().any(|window| window.fullscreen.is_some());

    let mut below = Vec::new();
    let mut inline: Option<Stroke> = None;
    for window in visible {
        if any_fullscreen && window.fullscreen.is_none() {
            continue;
        }
        let Some(stroke) = stroke_for(display, window, settings) else {
            continue;
        };
        match window.fullscreen {
            Some(FullscreenKind::Full) => {
                let replace = match &inline {
                    None => true,
                    Some(current) => window.focused && !is_focused(windows, current.id),
                };
                if replace {
                    inline = Some(stroke);
                }
            }
            _ => {
                if covers(window.frame, display) {
                    continue;
                }
                below.push(stroke);
            }
        }
    }
    below.sort_by_key(|stroke| stroke.id);
    DisplayBorders { below, inline }
}

fn stroke_for(display: CGRect, window: &BorderWindow, settings: &BorderSettings) -> Option<Stroke> {
    let fullscreen = window.fullscreen.is_some();
    let width = if fullscreen {
        settings.fullscreen_width()
    } else {
        settings.width
    };
    if width <= 0.0 || width.is_nan() {
        return None;
    }
    let color = match (fullscreen, settings.fullscreen_color()) {
        (true, Some(color)) => color,
        _ if window.focused => settings.active_color,
        _ if window.floating => settings.floating_color(),
        _ => settings.inactive_color,
    };
    Some(Stroke {
        id: window.id,
        frame: CGRect::new(
            CGPoint::new(
                window.frame.origin.x - display.origin.x,
                window.frame.origin.y - display.origin.y,
            ),
            window.frame.size,
        ),
        width,
        radius: settings.radius.max(0.0),
        color,
    })
}

fn is_focused(windows: &[BorderWindow], id: WindowId) -> bool {
    windows.iter().any(|window| window.id == id && window.focused)
}

/// `frame` leaves no part of `display` uncovered (within half a point).
fn covers(frame: CGRect, display: CGRect) -> bool {
    const EPS: f64 = 0.5;
    frame.origin.x <= display.origin.x + EPS
        && frame.origin.y <= display.origin.y + EPS
        && frame.origin.x + frame.size.width >= display.origin.x + display.size.width - EPS
        && frame.origin.y + frame.size.height >= display.origin.y + display.size.height - EPS
}

fn inset(rect: CGRect, amount: f64) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.origin.x + amount, rect.origin.y + amount),
        CGSize::new(
            (rect.size.width - 2.0 * amount).max(0.0),
            (rect.size.height - 2.0 * amount).max(0.0),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn settings() -> BorderSettings {
        BorderSettings {
            enabled: true,
            width: 4.0,
            radius: 10.0,
            active_color: Color::new(1.0, 0.0, 0.0, 1.0),
            inactive_color: Color::new(0.5, 0.5, 0.5, 1.0),
            floating_color: Some(Color::new(0.0, 1.0, 0.0, 1.0)),
            fullscreen: Some(crate::common::config::BorderFullscreenSettings {
                color: Some(Color::new(0.0, 0.0, 1.0, 1.0)),
                width: Some(2.0),
            }),
        }
    }

    fn window(idx: u32, frame: CGRect) -> BorderWindow {
        BorderWindow {
            id: WindowId::new(1, idx),
            server_id: None,
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

    #[test]
    fn focused_and_unfocused_windows_get_their_colors_in_display_coordinates() {
        let settings = settings();
        let mut focused = window(2, rect(1508.0, 8.0, 484.0, 784.0));
        focused.focused = true;
        let other = window(1, rect(1008.0, 8.0, 484.0, 784.0));
        let borders = compute(DISPLAY, &[focused, other], &settings);
        assert!(borders.inline.is_none());
        assert_eq!(borders.below.len(), 2);
        assert_eq!(borders.below[0].id, other.id, "sorted by window id");
        assert_eq!(borders.below[0].frame, rect(8.0, 8.0, 484.0, 784.0));
        assert_eq!(borders.below[0].color, settings.inactive_color);
        assert_eq!(borders.below[0].width, 4.0);
        assert_eq!(borders.below[0].radius, 10.0);
        assert_eq!(borders.below[1].id, focused.id);
        assert_eq!(borders.below[1].frame, rect(508.0, 8.0, 484.0, 784.0));
        assert_eq!(borders.below[1].color, settings.active_color);
    }

    #[test]
    fn floating_windows_use_the_floating_color_unless_focused() {
        let settings = settings();
        let mut floating = window(1, rect(1100.0, 100.0, 300.0, 200.0));
        floating.floating = true;
        let borders = compute(DISPLAY, &[floating], &settings);
        assert_eq!(borders.below[0].color, settings.floating_color.unwrap());

        floating.focused = true;
        let borders = compute(DISPLAY, &[floating], &settings);
        assert_eq!(borders.below[0].color, settings.active_color);

        let mut without = settings.clone();
        without.floating_color = None;
        floating.focused = false;
        let borders = compute(DISPLAY, &[floating], &without);
        assert_eq!(borders.below[0].color, without.inactive_color);
    }

    #[test]
    fn fullscreen_within_gaps_keeps_the_outside_stroke_and_hides_the_others() {
        let settings = settings();
        let mut fullscreen = window(1, rect(1005.0, 0.0, 990.0, 795.0));
        fullscreen.fullscreen = Some(FullscreenKind::WithinGaps);
        let other = window(2, rect(1008.0, 8.0, 484.0, 784.0));
        let borders = compute(DISPLAY, &[other, fullscreen], &settings);
        assert!(borders.inline.is_none());
        assert_eq!(borders.below.len(), 1, "{borders:?}");
        let stroke = borders.below[0];
        assert_eq!(stroke.id, fullscreen.id);
        assert_eq!(stroke.frame, rect(5.0, 0.0, 990.0, 795.0));
        assert_eq!(stroke.width, 2.0);
        assert_eq!(stroke.color, Color::new(0.0, 0.0, 1.0, 1.0));
        let ring = stroke.outside_ring();
        assert_eq!(ring.outer, rect(3.0, -2.0, 994.0, 799.0));
        assert_eq!(ring.inner, stroke.frame);
        assert_eq!(ring.outer_radius, 12.0);
        assert_eq!(ring.inner_radius, 10.0);
    }

    #[test]
    fn fullscreen_without_gaps_is_the_single_inline_stroke() {
        let settings = settings();
        let mut fullscreen = window(1, DISPLAY);
        fullscreen.fullscreen = Some(FullscreenKind::Full);
        let other = window(2, rect(1008.0, 8.0, 484.0, 784.0));
        let borders = compute(DISPLAY, &[fullscreen, other], &settings);
        assert!(borders.below.is_empty(), "{borders:?}");
        let inline = borders.inline.expect("inline stroke");
        assert_eq!(inline.id, fullscreen.id);
        assert_eq!(inline.frame, rect(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(inline.width, 2.0);
        let ring = inline.inline_ring();
        assert_eq!(ring.outer, inline.frame);
        assert_eq!(ring.inner, rect(2.0, 2.0, 996.0, 796.0));
        assert_eq!(ring.outer_radius, 10.0);
        assert_eq!(ring.inner_radius, 8.0);

        // Without a fullscreen color the stroke follows focus like any other window.
        let mut plain = settings.clone();
        plain.fullscreen = None;
        let borders = compute(DISPLAY, &[fullscreen, other], &plain);
        let inline = borders.inline.unwrap();
        assert_eq!(inline.color, plain.inactive_color);
        assert_eq!(inline.width, plain.width);

        // Two Full windows: the focused one wins.
        let mut second = window(3, DISPLAY);
        second.fullscreen = Some(FullscreenKind::Full);
        second.focused = true;
        let borders = compute(DISPLAY, &[fullscreen, second], &settings);
        assert_eq!(borders.inline.unwrap().id, second.id);
        let borders = compute(DISPLAY, &[second, fullscreen], &settings);
        assert_eq!(borders.inline.unwrap().id, second.id);
    }

    #[test]
    fn hidden_windows_get_no_stroke() {
        let settings = settings();
        let mut hidden = window(1, rect(1990.0, 790.0, 500.0, 500.0));
        hidden.visible = false;
        hidden.focused = true;
        let shown = window(2, rect(1008.0, 8.0, 984.0, 784.0));
        let borders = compute(DISPLAY, &[hidden, shown], &settings);
        assert_eq!(borders.below.len(), 1);
        assert_eq!(borders.below[0].id, shown.id);

        // A hidden fullscreen window neither draws nor suppresses the others.
        hidden.fullscreen = Some(FullscreenKind::Full);
        let borders = compute(DISPLAY, &[hidden, shown], &settings);
        assert!(borders.inline.is_none());
        assert_eq!(borders.below.len(), 1);
    }

    #[test]
    fn a_window_without_any_gap_gets_no_outside_stroke() {
        let settings = settings();
        let full = window(1, DISPLAY);
        let borders = compute(DISPLAY, &[full], &settings);
        assert!(borders.is_empty(), "{borders:?}");

        // Sub-point overshoot still counts as covering.
        let almost = window(1, rect(1000.3, -0.2, 999.5, 800.4));
        assert!(compute(DISPLAY, &[almost], &settings).is_empty());

        // Touching three edges still leaves a gap on the fourth.
        let three = window(1, rect(1000.0, 0.0, 1000.0, 792.0));
        assert_eq!(compute(DISPLAY, &[three], &settings).below.len(), 1);

        // A zero width disables strokes entirely.
        let mut zero = settings.clone();
        zero.width = 0.0;
        zero.fullscreen = None;
        let shown = window(2, rect(1008.0, 8.0, 984.0, 784.0));
        assert!(compute(DISPLAY, &[shown], &zero).is_empty());
    }

    #[test]
    fn layout_animation_follows_rift_window_animation_rules() {
        let mut settings = crate::common::config::Config::default().settings;
        settings.animate = true;
        settings.animation_duration = 0.25;
        let on = Some(BorderAnimation { duration: 0.25 });

        assert_eq!(BorderAnimation::for_layout(&settings, None, false, false), on);
        // Resizes and low power mode are never animated.
        assert_eq!(BorderAnimation::for_layout(&settings, None, true, false), None);
        assert_eq!(BorderAnimation::for_layout(&settings, None, false, true), None);
        // A layout with its own setting ignores both the global one and low power.
        assert_eq!(
            BorderAnimation::for_layout(&settings, Some(true), false, true),
            on
        );
        assert_eq!(
            BorderAnimation::for_layout(&settings, Some(false), false, false),
            None
        );
        settings.animate = false;
        assert_eq!(BorderAnimation::for_layout(&settings, None, false, false), None);
        assert_eq!(
            BorderAnimation::for_layout(&settings, Some(true), false, false),
            on
        );
        // Nothing to animate over zero time.
        settings.animation_duration = 0.0;
        assert_eq!(
            BorderAnimation::for_layout(&settings, Some(true), false, false),
            None
        );
    }

    #[test]
    fn displays_are_computed_independently() {
        let settings = settings();
        let left = rect(0.0, 0.0, 1000.0, 800.0);
        let mut on_left = window(1, rect(8.0, 8.0, 984.0, 784.0));
        on_left.focused = true;
        let on_right = window(2, rect(1008.0, 8.0, 984.0, 784.0));

        let left_borders = compute(left, &[on_left], &settings);
        let right_borders = compute(DISPLAY, &[on_right], &settings);
        assert_eq!(left_borders.below.len(), 1);
        assert_eq!(left_borders.below[0].frame, rect(8.0, 8.0, 984.0, 784.0));
        assert_eq!(left_borders.below[0].color, settings.active_color);
        assert_eq!(right_borders.below.len(), 1);
        assert_eq!(right_borders.below[0].frame, rect(8.0, 8.0, 984.0, 784.0));
        assert_eq!(right_borders.below[0].color, settings.inactive_color);

        // A fullscreen window on one display does not suppress the other's strokes.
        let mut right_full = on_right;
        right_full.frame = DISPLAY;
        right_full.fullscreen = Some(FullscreenKind::Full);
        assert_eq!(compute(left, &[on_left], &settings).below.len(), 1);
        assert!(compute(DISPLAY, &[right_full], &settings).inline.is_some());
    }
}
