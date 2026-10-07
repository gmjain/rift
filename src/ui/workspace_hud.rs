//! The workspace HUD overlay: one click-through WindowServer window per
//! display with a compositor-bound layer tree:
//!
//! ```text
//! root        whole window; its opacity is the fade
//! └ container the card's frame; casts the card shadow (fixed path), pops on show
//!   └ card    rounded corners, background, border; clips the blur
//!     ├ backdrop + tint   only with blur_radius > 0
//!     └ text  CATextLayer, vertically centered; casts the text shadow without a background
//! ```
//!
//! The window is the card plus room for its shadow, or for an auto-width card
//! the display's whole width at the card's height, so its frame is fixed for
//! the overlay's life. Everything style-dependent (font, colors, corner radius,
//! border, shadow, blur, level, tags) is set when the overlay is built for a
//! config and a display set, never on a show. A show sets the text (cached
//! `NSString`), moves the card within the window when an auto-width name
//! changed its width, fades the root in inside one `CATransaction` (geometry
//! and text with implicit animations disabled) and orders the window in once:
//! no WindowServer call but that order-in. Hiding fades the root out and
//! orders the window out.

use std::ptr;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSFont, NSFontAttributeName, NSFontDescriptor, NSFontFamilyAttribute, NSFontTraitsAttribute,
    NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSFontWeightTrait, NSStringDrawing,
};
use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGMutablePath, kCGStatusWindowLevel};
use objc2_foundation::{NSAttributedStringKey, NSDictionary, NSNumber, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CATextLayer, CATransaction,
    kCAAlignmentCenter, kCATruncationEnd,
};
use tracing::warn;

use crate::common::config::Color;
use crate::model::workspace_hud::{HudDisplay, HudFontWeight, HudShadow, HudStyle, TextMetrics};
use crate::sys::backdrop_layer::backdrop_blur;
use crate::sys::cgs_window::{CgsWindow, CgsWindowError};
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_surface::WindowSurface;
use crate::ui::common::with_disabled_actions;

/// Above app windows, floating panels, the Dock and the menu bar; below pop-up
/// menus and the screen saver.
const HUD_LEVEL: i32 = kCGStatusWindowLevel;
const POP_FROM: f64 = 0.95;
const POP_MIN_SECONDS: f64 = 0.12;

/// The HUD font and what measuring text with it needs, resolved per config.
pub struct HudFont {
    font: Retained<NSFont>,
    attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    size: f64,
    /// Ascent + descent + leading.
    pub line_height: f64,
}

impl HudFont {
    pub fn resolve(style: &HudStyle) -> Self {
        let size = style.font_size;
        let weight = unsafe {
            match style.font_weight {
                HudFontWeight::Regular => NSFontWeightRegular,
                HudFontWeight::Medium => NSFontWeightMedium,
                HudFontWeight::Semibold => NSFontWeightSemibold,
                HudFontWeight::Bold => NSFontWeightBold,
            }
        };
        let font = style
            .font_family
            .as_deref()
            .and_then(|family| {
                let font = installed_font(family, weight, size);
                if font.is_none() {
                    warn!(
                        family,
                        "workspace HUD font is not installed; using the system font"
                    );
                }
                font
            })
            .unwrap_or_else(|| NSFont::systemFontOfSize_weight(size, weight));
        let attributes = NSDictionary::<NSAttributedStringKey, AnyObject>::from_slices(
            &[unsafe { NSFontAttributeName }],
            &[font.as_ref()],
        );
        let line_height = font.ascender() - font.descender() + font.leading();
        Self {
            font,
            attributes,
            size,
            line_height,
        }
    }

    /// Typographic width of `text` in this font.
    pub fn measure(&self, text: &NSString) -> f64 {
        unsafe { text.sizeWithAttributes(Some(&self.attributes)) }.width
    }

    pub fn metrics(&self, text_width: f64) -> TextMetrics {
        TextMetrics {
            width: text_width,
            line_height: self.line_height,
        }
    }
}

/// `family` at `weight` if that family is installed, else a font named
/// `family` ("Menlo-Bold"), else `None`.
fn installed_font(family: &str, weight: f64, size: f64) -> Option<Retained<NSFont>> {
    let family_name = NSString::from_str(family);
    let weight_number = NSNumber::new_f64(weight);
    let traits =
        NSDictionary::<NSString, AnyObject>::from_slices(&[unsafe { NSFontWeightTrait }], &[
            weight_number.as_ref(),
        ]);
    let attributes = NSDictionary::<NSString, AnyObject>::from_slices(
        &[unsafe { NSFontFamilyAttribute }, unsafe {
            NSFontTraitsAttribute
        }],
        &[family_name.as_ref(), traits.as_ref()],
    );
    let descriptor =
        unsafe { NSFontDescriptor::fontDescriptorWithFontAttributes(Some(&attributes)) };
    // Font matching falls back to some other family rather than failing.
    if let Some(font) = NSFont::fontWithDescriptor_size(&descriptor, size)
        && font
            .familyName()
            .is_some_and(|name| name.to_string().eq_ignore_ascii_case(family))
    {
        return Some(font);
    }
    NSFont::fontWithName_size(&family_name, size)
}

fn cg_color(color: Color) -> CFRetained<CGColor> {
    CGColor::new_generic_rgb(color.r, color.g, color.b, color.a)
}

fn bounds(size: CGSize) -> CGRect { CGRect::new(CGPoint::new(0.0, 0.0), size) }

fn rounded_rect_path(rect: CGRect, radius: f64) -> CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    if radius > 0.0 {
        unsafe { CGMutablePath::add_rounded_rect(Some(&path), ptr::null(), rect, radius, radius) };
    } else {
        unsafe { CGMutablePath::add_rect(Some(&path), ptr::null(), rect) };
    }
    path
}

/// The HUD overlay of one display.
pub struct HudWindow {
    // The surface must be detached before its owning WindowServer window drops.
    surface: WindowSurface,
    window: CgsWindow,
    root: Retained<CALayer>,
    container: Retained<CALayer>,
    card: Retained<CALayer>,
    /// Backdrop blur and the background color above it, with blur only.
    blur: Option<(Retained<CALayer>, Retained<CALayer>)>,
    text: Retained<CATextLayer>,
    /// Pre-built pop animation (`scale_in`) and its key; added again on every show.
    pop: Option<(Retained<CABasicAnimation>, Retained<NSString>)>,
    /// Window frame, global CG coordinates; fixed for the overlay's life.
    frame: CGRect,
    /// Where the card is, global CG coordinates.
    card_rect: CGRect,
    shown_text: Option<Retained<NSString>>,
    ordered_in: bool,
}

impl HudWindow {
    /// Build the overlay for `display`, not ordered in. The window frame is
    /// final here (an auto-width card's window spans the display's width).
    pub fn new(
        display: &HudDisplay,
        style: &HudStyle,
        font: &HudFont,
    ) -> Result<Self, CgsWindowError> {
        let scale = display.backing_scale;
        let card_size = style.card_size(font.metrics(0.0), display.frame.size);
        let card_rect = style.place(card_size, display.frame);
        let frame = style.window_frame(card_rect, display.frame);

        let root = CALayer::layer();
        let container = CALayer::layer();
        let card = CALayer::layer();
        let text = CATextLayer::layer();
        let blur = (style.blur_radius > 0.0)
            .then(|| match backdrop_blur(style.blur_radius) {
                Some(backdrop) => Some((backdrop, CALayer::layer())),
                None => {
                    warn!("workspace HUD blur is unavailable on this macOS; drawing without it");
                    None
                }
            })
            .flatten();
        let background = cg_color(style.background_color);
        let shadow_color = cg_color(Color::new(0.0, 0.0, 0.0, 1.0));
        with_disabled_actions(|| {
            root.setAnchorPoint(CGPoint::new(0.0, 0.0));
            root.setPosition(CGPoint::new(0.0, 0.0));
            root.setContentsScale(scale);
            root.setOpaque(false);
            root.setOpacity(0.0);

            container.setContentsScale(scale);
            card.setContentsScale(scale);
            card.setCornerRadius(style.corner_radius_for(card_size));
            if style.border_width > 0.0 {
                card.setBorderWidth(style.border_width);
                card.setBorderColor(Some(&cg_color(style.border_color)));
            }
            match &blur {
                Some((backdrop, tint)) => {
                    card.setMasksToBounds(true);
                    card.addSublayer(backdrop);
                    tint.setBackgroundColor(Some(&background));
                    card.addSublayer(tint);
                }
                None => card.setBackgroundColor(Some(&background)),
            }

            text.setContentsScale(scale);
            unsafe { text.setFont(Some(&*(Retained::as_ptr(&font.font) as *const CFType))) };
            text.setFontSize(font.size);
            text.setForegroundColor(Some(&cg_color(style.text_color)));
            text.setAlignmentMode(unsafe { kCAAlignmentCenter });
            text.setTruncationMode(unsafe { kCATruncationEnd });
            card.addSublayer(&text);

            if let Some(HudShadow { radius, opacity }) = style.shadow {
                // With a background the card casts it along a fixed path (no
                // offscreen pass); without one the glyphs cast it.
                let caster: &CALayer = if style.has_background() {
                    &container
                } else {
                    &text
                };
                caster.setShadowColor(Some(&shadow_color));
                caster.setShadowOpacity(opacity as f32);
                caster.setShadowRadius(radius);
                caster.setShadowOffset(CGSize::new(0.0, -HudShadow::DROP));
            }
            container.addSublayer(&card);
            root.addSublayer(&container);
        });

        let pop = style.scale_in.then(|| {
            let key = NSString::from_str("transform.scale");
            let pop = CABasicAnimation::animationWithKeyPath(Some(&key));
            let from = NSNumber::new_f64(POP_FROM);
            let to = NSNumber::new_f64(1.0);
            unsafe {
                pop.setFromValue(Some(from.as_ref()));
                pop.setToValue(Some(to.as_ref()));
            }
            pop.setDuration((style.fade_ms / 1000.0).max(POP_MIN_SECONDS));
            pop.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
                0.2, 0.8, 0.2, 1.0,
            )));
            (pop, NSString::from_str("pop"))
        });

        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.set_resolution(scale)?;
        window.set_level(HUD_LEVEL)?;
        // Click-through from creation (new_compositor); our shadow is drawn in
        // the layer tree, so WindowServer's would only double it.
        window.set_tags(SLSWindowTags::DISABLE_SHADOW.bits())?;
        let surface = WindowSurface::new_scaled(window.id(), bounds(frame.size), &root, scale)?;
        let mut hud = Self {
            surface,
            window,
            root,
            container,
            card,
            blur,
            text,
            pop,
            frame,
            card_rect,
            shown_text: None,
            ordered_in: false,
        };
        with_disabled_actions(|| hud.apply_geometry(style, font, card_rect, true));
        hud.surface.flush();
        Ok(hud)
    }

    /// Lay the layers out for the card at `card` (global CG) in the window at
    /// `self.frame`; `resized` when the card's size changed too. Call inside a
    /// transaction with implicit animations disabled.
    fn apply_geometry(&mut self, style: &HudStyle, font: &HudFont, card: CGRect, resized: bool) {
        let size = card.size;
        self.root.setBounds(bounds(self.frame.size));
        // Layer space is y-up from the window's bottom-left corner.
        let origin = CGPoint::new(
            card.origin.x - self.frame.origin.x,
            self.frame.origin.y + self.frame.size.height - (card.origin.y + size.height),
        );
        self.container.setFrame(CGRect::new(origin, size));
        if resized {
            let card_bounds = bounds(size);
            let radius = style.corner_radius_for(size);
            self.card.setFrame(card_bounds);
            self.card.setCornerRadius(radius);
            if let Some((backdrop, tint)) = &self.blur {
                backdrop.setFrame(card_bounds);
                tint.setFrame(card_bounds);
            }
            // CATextLayer draws from the top of its bounds: a one-line-tall
            // layer centered in the card centers the text.
            let line = font.line_height.min(size.height);
            self.text.setFrame(CGRect::new(
                CGPoint::new(style.padding.x, ((size.height - line) / 2.0).max(0.0)),
                CGSize::new((size.width - 2.0 * style.padding.x).max(0.0), line),
            ));
            if style.shadow.is_some() && style.has_background() {
                self.container.setShadowPath(Some(&rounded_rect_path(card_bounds, radius)));
            }
        }
        self.card_rect = card;
    }

    /// Show `text` (measured `text_width`) on `display`: fade in from wherever
    /// the fade is, restart the pop, order in if not on screen.
    pub fn present(
        &mut self,
        display: &HudDisplay,
        style: &HudStyle,
        font: &HudFont,
        text: &Retained<NSString>,
        text_width: f64,
    ) -> Result<(), CgsWindowError> {
        // Only an auto-width card moves, and only in the layer tree: its window
        // already spans the display.
        let card = if style.is_auto_sized() {
            style.place(
                style.card_size(font.metrics(text_width), display.frame.size),
                display.frame,
            )
        } else {
            self.card_rect
        };

        let text_changed = !self
            .shown_text
            .as_ref()
            .is_some_and(|shown| ptr::eq(Retained::as_ptr(shown), Retained::as_ptr(text)));
        let fade = (style.fade_ms / 1000.0).max(0.0);
        CATransaction::begin();
        CATransaction::setAnimationDuration(fade);
        with_disabled_actions(|| {
            if card != self.card_rect {
                self.apply_geometry(style, font, card, card.size != self.card_rect.size);
            }
            if text_changed {
                let string: &AnyObject = (**text).as_ref();
                unsafe { self.text.setString(Some(string)) };
            }
        });
        if fade == 0.0 {
            with_disabled_actions(|| self.root.setOpacity(1.0));
        } else {
            // Implicit: starts from the on-screen opacity, so a show during the
            // fade-out turns it around instead of jumping.
            self.root.setOpacity(1.0);
        }
        if let Some((pop, key)) = &self.pop {
            self.container.addAnimation_forKey(pop, Some(key));
        }
        CATransaction::commit();
        self.surface.flush();
        if text_changed {
            self.shown_text = Some(text.clone());
        }
        if !self.ordered_in {
            // Joins the display's current Space at the HUD level; nothing else
            // is ordered.
            self.window.order_above(None)?;
            self.ordered_in = true;
        }
        Ok(())
    }

    /// Start fading out over `fade_ms`; `order_out` ends it.
    pub fn fade_out(&self, fade_ms: f64) {
        if fade_ms > 0.0 {
            CATransaction::begin();
            CATransaction::setAnimationDuration(fade_ms / 1000.0);
            self.root.setOpacity(0.0);
            CATransaction::commit();
        } else {
            with_disabled_actions(|| self.root.setOpacity(0.0));
        }
        self.surface.flush();
    }

    /// Take the overlay off screen now.
    pub fn order_out(&mut self) {
        if !self.ordered_in {
            return;
        }
        with_disabled_actions(|| self.root.setOpacity(0.0));
        self.surface.flush();
        if let Err(error) = self.window.order_out() {
            warn!(?error, "failed to order out the workspace HUD");
        }
        self.ordered_in = false;
    }
}

impl Drop for HudWindow {
    fn drop(&mut self) { self.order_out(); }
}
