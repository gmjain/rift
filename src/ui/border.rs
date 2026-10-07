//! Per-display window border overlays.
//!
//! Each display gets one WindowServer window the size of the display, ordered
//! by *level* just above the desktop so every app window is above it without a
//! single per-window ordering call. The strokes are a layer tree bound to the
//! compositor (`WindowSurface`): one `CAShapeLayer` per stroked window holding
//! an even-odd ring path, all changed in one `CATransaction` per update. When
//! rift animates the windows, each changed path gets one explicit
//! `CABasicAnimation` with rift's duration and easing (shape paths never
//! animate implicitly), so the compositor moves the strokes with the windows
//! without per-tick work in rift. The window in `toggle_fullscreen` leaves no
//! gap to draw in, so its stroke goes inline into a second, floating-level
//! overlay that exists only while needed.

use std::ptr;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGColor, CGMutablePath, CGPath, kCGDesktopIconWindowLevel, kCGFloatingWindowLevel,
};
use objc2_foundation::NSString;
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer, CATransaction,
    kCAFillRuleEvenOdd,
};

use crate::actor::app::WindowId;
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::Color;
use crate::model::border::{BorderAnimation, DisplayBorders, Ring};
use crate::sys::cgs_window::{CgsWindow, CgsWindowError};
use crate::sys::screen::SpaceId;
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_surface::WindowSurface;
use crate::ui::common::with_disabled_actions;

/// Just above the desktop picture and its icons: below every app window by
/// level alone, so the overlay is never reordered (per-window ordering is what
/// made per-window border overlays flicker).
const BELOW_LEVEL: i32 = kCGDesktopIconWindowLevel + 1;
/// The inline stroke of a `toggle_fullscreen` window sits above app windows.
const INLINE_LEVEL: i32 = kCGFloatingWindowLevel;

pub struct DisplayOverlay {
    space: SpaceId,
    frame: CGRect,
    scale: f64,
    below: Overlay,
    inline: Option<Overlay>,
}

impl DisplayOverlay {
    pub fn new(space: SpaceId, frame: CGRect, scale: f64) -> Result<Self, CgsWindowError> {
        let below = Overlay::new(frame, scale, BELOW_LEVEL)?;
        Ok(Self {
            space,
            frame,
            scale,
            below,
            inline: None,
        })
    }

    /// Whether this overlay can keep serving the display as described.
    pub fn matches(&self, space: SpaceId, frame: CGRect, scale: f64) -> bool {
        self.space == space && self.frame == frame && self.scale == scale
    }

    /// Replace the strokes: one transaction for the below overlay, and the
    /// inline overlay created or dropped as the fullscreen exception comes and
    /// goes. With `animation`, strokes that move do so over that motion.
    pub fn apply(
        &mut self,
        borders: &DisplayBorders,
        animation: Option<BorderAnimation>,
    ) -> Result<(), CgsWindowError> {
        self.below.apply(
            borders
                .below
                .iter()
                .map(|stroke| (stroke.id, stroke.outside_ring(), stroke.color)),
            animation,
        );
        match &borders.inline {
            Some(stroke) => {
                if self.inline.is_none() {
                    self.inline = Some(Overlay::new(self.frame, self.scale, INLINE_LEVEL)?);
                }
                if let Some(inline) = &mut self.inline {
                    inline.apply(
                        std::iter::once((stroke.id, stroke.inline_ring(), stroke.color)),
                        animation,
                    );
                }
            }
            None => self.inline = None,
        }
        Ok(())
    }
}

/// One WindowServer window with a compositor-bound layer tree of ring paths.
struct Overlay {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    root: Retained<CALayer>,
    layers: HashMap<WindowId, Retained<CAShapeLayer>>,
    bounds: CGRect,
}

impl Overlay {
    fn new(frame: CGRect, scale: f64, level: i32) -> Result<Self, CgsWindowError> {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), frame.size);
        let root = CALayer::layer();
        with_disabled_actions(|| {
            root.setBounds(bounds);
            root.setPosition(CGPoint::new(0.0, 0.0));
            root.setAnchorPoint(CGPoint::new(0.0, 0.0));
            root.setContentsScale(scale);
            root.setOpaque(false);
            root.setMasksToBounds(true);
        });

        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.set_resolution(scale)?;
        window.set_level(level)?;
        // Transparent window: WindowServer would otherwise shade the rings.
        window.set_tags(SLSWindowTags::DISABLE_SHADOW.bits())?;
        let surface = WindowSurface::new_scaled(window.id(), bounds, &root, scale)?;
        surface.flush();
        // The only ordering call this window ever gets: it joins the display's
        // current Space at its level, and the level keeps it under app windows.
        window.order_above(None)?;

        Ok(Self {
            surface,
            window,
            root,
            layers: HashMap::default(),
            bounds,
        })
    }

    fn apply(
        &mut self,
        strokes: impl Iterator<Item = (WindowId, Ring, Color)>,
        animation: Option<BorderAnimation>,
    ) {
        let height = self.bounds.size.height;
        let bounds = self.bounds;
        let root = &self.root;
        let layers = &mut self.layers;
        let mut kept: HashSet<WindowId> = HashSet::default();

        // Explicit animations below still run; this only stops Core Animation
        // from implicitly animating colors and new layers.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (id, ring, color) in strokes {
            let path = ring_path(&ring, height);
            let layer = layers.entry(id).or_insert_with(|| {
                let layer = CAShapeLayer::layer();
                layer.setFrame(bounds);
                layer.setContentsScale(root.contentsScale());
                layer.setFillRule(unsafe { kCAFillRuleEvenOdd });
                root.addSublayer(&layer);
                layer
            });
            layer.setFillColor(Some(&cg_color(color)));
            if let Some(animation) = animation
                && let Some(previous) = layer.path()
                && !CGPath::equal_to_path(Some(&previous), Some(&path))
            {
                animate_path(layer, &previous, &path, animation);
            }
            layer.setPath(Some(&path));
            kept.insert(id);
        }
        layers.retain(|id, layer| {
            let keep = kept.contains(id);
            if !keep {
                layer.removeFromSuperlayer();
            }
            keep
        });
        CATransaction::commit();
        self.surface.flush();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        with_disabled_actions(|| unsafe { self.root.setSublayers(None) });
        let _ = self.window.order_out();
    }
}

fn cg_color(color: Color) -> CFRetained<CGColor> {
    CGColor::new_generic_rgb(color.r, color.g, color.b, color.a)
}

/// Move `layer`'s ring from `previous` to `next` in the compositor, over the
/// same duration and curve as rift moves the window.
fn animate_path(
    layer: &CAShapeLayer,
    previous: &CGPath,
    next: &CGMutablePath,
    animation: BorderAnimation,
) {
    // Start from where the ring is on screen: a change that lands while an
    // earlier one is still in flight continues from there instead of jumping
    // back to the old frame.
    let presented: Option<Retained<CGPath>> = unsafe { layer.presentationLayer() }
        .and_then(|presented| presented.downcast::<CAShapeLayer>().ok())
        .and_then(|presented| presented.path());
    let from: &CGPath = presented.as_deref().unwrap_or(previous);

    let key = NSString::from_str("path");
    let motion = CABasicAnimation::animationWithKeyPath(Some(&key));
    let from_value: &AnyObject = from.as_ref();
    let to_value: &AnyObject = (**next).as_ref();
    unsafe {
        motion.setFromValue(Some(from_value));
        motion.setToValue(Some(to_value));
    }
    motion.setDuration(animation.duration);
    let [c1x, c1y, c2x, c2y] = BorderAnimation::CONTROL_POINTS;
    motion.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
        c1x, c1y, c2x, c2y,
    )));
    // Same key as the property: a new change replaces the running animation.
    layer.addAnimation_forKey(&motion, Some(&key));
}

/// Core Animation's layer space has its origin at the bottom-left; the model
/// speaks CG (top-left) display-local coordinates.
pub(crate) fn flip(rect: CGRect, height: f64) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.origin.x, height - rect.origin.y - rect.size.height),
        rect.size,
    )
}

/// The ring between two rounded rectangles, as an even-odd path.
pub(crate) fn ring_path(ring: &Ring, height: f64) -> CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    add_rounded_rect(&path, flip(ring.outer, height), ring.outer_radius);
    add_rounded_rect(&path, flip(ring.inner, height), ring.inner_radius);
    path
}

fn add_rounded_rect(path: &CGMutablePath, rect: CGRect, radius: f64) {
    let (width, height) = (rect.size.width, rect.size.height);
    if width <= 0.0 || height <= 0.0 || width.is_nan() || height.is_nan() {
        return;
    }
    let radius = radius.max(0.0).min(rect.size.width / 2.0).min(rect.size.height / 2.0);
    unsafe { CGMutablePath::add_rounded_rect(Some(path), ptr::null(), rect, radius, radius) };
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CGSize;

    use super::*;
    use crate::sys::geometry::SameAs;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    #[test]
    fn flip_mirrors_the_y_axis_within_the_display() {
        // A rect 8pt below the top of an 800pt display ends 8pt below the top in
        // CA space too, i.e. its bottom edge sits at 800 - 8 - 784 = 8.
        assert_eq!(
            flip(rect(8.0, 8.0, 484.0, 784.0), 800.0),
            rect(8.0, 8.0, 484.0, 784.0)
        );
        assert_eq!(
            flip(rect(0.0, 0.0, 100.0, 50.0), 800.0),
            rect(0.0, 750.0, 100.0, 50.0)
        );
        assert_eq!(
            flip(rect(10.0, 700.0, 100.0, 50.0), 800.0),
            rect(10.0, 50.0, 100.0, 50.0)
        );
    }

    #[test]
    fn ring_path_spans_the_outer_rect_and_clamps_the_radius() {
        let ring = Ring {
            outer: rect(4.0, 4.0, 492.0, 792.0),
            outer_radius: 14.0,
            inner: rect(8.0, 8.0, 484.0, 784.0),
            inner_radius: 10.0,
        };
        let path = ring_path(&ring, 800.0);
        let bounds = CGPath::bounding_box(Some(&path));
        assert!(bounds.same_as(rect(4.0, 4.0, 492.0, 792.0)), "{bounds:?}");
        assert!(!CGPath::is_empty(Some(&path)));

        // A ring too small for its radius still produces a path.
        let tiny = Ring {
            outer: rect(0.0, 0.0, 6.0, 6.0),
            outer_radius: 14.0,
            inner: rect(2.0, 2.0, 2.0, 2.0),
            inner_radius: 10.0,
        };
        let path = ring_path(&tiny, 800.0);
        let bounds = CGPath::bounding_box(Some(&path));
        assert!(bounds.same_as(rect(0.0, 794.0, 6.0, 6.0)), "{bounds:?}");

        // Degenerate rects contribute nothing.
        let empty = Ring {
            outer: rect(0.0, 0.0, 0.0, 0.0),
            outer_radius: 1.0,
            inner: rect(0.0, 0.0, -2.0, 5.0),
            inner_radius: 1.0,
        };
        assert!(CGPath::is_empty(Some(&ring_path(&empty, 800.0))));
    }
}
