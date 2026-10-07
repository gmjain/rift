//! Per-display dim overlay: a translucent window with a hole around the kept
//! window, ordered just below it so everything under it is dimmed and
//! anything above it is not.

use std::ptr;

use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{CGColor, CGMutablePath, kCGNormalWindowLevel};
use objc2_quartz_core::{CALayer, CAShapeLayer, CATransaction, kCAFillRuleEvenOdd};

use crate::common::config::{Color, DimSettings};
use crate::model::dim::DimPlan;
use crate::sys::cgs_window::{CgsWindow, CgsWindowError};
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_surface::WindowSurface;
use crate::ui::border::flip;
use crate::ui::common::with_disabled_actions;

pub struct DimOverlay {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    root: Retained<CALayer>,
    shape: Retained<CAShapeLayer>,
    bounds: CGRect,
    /// The window this overlay was last ordered below, if any.
    below: Option<u32>,
    shown: bool,
}

impl DimOverlay {
    pub fn new(frame: CGRect, scale: f64) -> Result<Self, CgsWindowError> {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), frame.size);
        let root = CALayer::layer();
        let shape = CAShapeLayer::layer();
        with_disabled_actions(|| {
            root.setBounds(bounds);
            root.setPosition(CGPoint::new(0.0, 0.0));
            root.setAnchorPoint(CGPoint::new(0.0, 0.0));
            root.setContentsScale(scale);
            root.setOpaque(false);
            root.setMasksToBounds(true);
            root.setOpacity(0.0);
            shape.setFrame(bounds);
            shape.setContentsScale(scale);
            shape.setFillRule(unsafe { kCAFillRuleEvenOdd });
            root.addSublayer(&shape);
        });
        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.set_resolution(scale)?;
        window.set_level(kCGNormalWindowLevel)?;
        // Same tags as the stack line: click-through from creation, no shadow.
        window.set_tags(SLSWindowTags::DISABLE_SHADOW.bits())?;
        let surface = WindowSurface::new_scaled(window.id(), bounds, &root, scale)?;
        surface.flush();
        Ok(Self {
            surface,
            window,
            root,
            shape,
            bounds,
            below: None,
            shown: false,
        })
    }

    /// Show `plan` (or fade out for `None`), fading opacity over `fade_ms`.
    pub fn apply(
        &mut self,
        plan: Option<DimPlan>,
        settings: &DimSettings,
        reorder: bool,
    ) -> Result<(), CgsWindowError> {
        let Some(plan) = plan else {
            if self.shown {
                self.fade_to(0.0, settings.fade_ms);
                self.shown = false;
            }
            return Ok(());
        };
        with_disabled_actions(|| {
            self.shape.setFillColor(Some(&cg_color(settings.color)));
            self.shape.setPath(Some(&hole_path(self.bounds, plan.hole)));
        });
        let anchor = plan.keep.map(|(_, server_id)| server_id);
        if !self.shown || reorder || anchor != self.below {
            // Our own window only: just below the kept window, or on top of
            // the display's windows when everything there is dimmed.
            match anchor {
                Some(server_id) => self.window.order_below(Some(server_id))?,
                None => self.window.order_above(None)?,
            }
            self.below = anchor;
        }
        if !self.shown {
            self.fade_to(settings.opacity as f32, settings.fade_ms);
            self.shown = true;
        } else {
            with_disabled_actions(|| self.root.setOpacity(settings.opacity as f32));
            self.surface.flush();
        }
        Ok(())
    }

    fn fade_to(&self, opacity: f32, fade_ms: f64) {
        CATransaction::begin();
        CATransaction::setAnimationDuration(fade_ms.max(0.0) / 1000.0);
        self.root.setOpacity(opacity);
        CATransaction::commit();
        self.surface.flush();
    }
}

impl Drop for DimOverlay {
    fn drop(&mut self) {
        with_disabled_actions(|| self.root.setOpacity(0.0));
        let _ = self.window.order_out();
    }
}

fn cg_color(color: Color) -> objc2_core_foundation::CFRetained<CGColor> {
    CGColor::new_generic_rgb(color.r, color.g, color.b, color.a)
}

/// The display minus the hole, as an even-odd path in layer coordinates.
fn hole_path(
    bounds: CGRect,
    hole: Option<CGRect>,
) -> objc2_core_foundation::CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    unsafe { CGMutablePath::add_rect(Some(&path), ptr::null(), bounds) };
    if let Some(hole) = hole
        && hole.size.width > 0.0
        && hole.size.height > 0.0
    {
        unsafe {
            CGMutablePath::add_rect(Some(&path), ptr::null(), flip(hole, bounds.size.height))
        };
    }
    path
}
