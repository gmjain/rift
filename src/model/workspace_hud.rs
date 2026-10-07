//! Workspace HUD model: the `[settings.ui.workspace_hud]` settings, the style
//! presets they start from, the text template, the card's size and place on a
//! display, which displays show it, and when it appears and goes.
//!
//! Everything here is pure so the policy is unit-testable; the overlay itself
//! lives in `ui::workspace_hud`, its actor in `actor::workspace_hud`.
//!
//! Performance contract: presets, colors and fonts resolve once per config
//! (`WorkspaceHudSettings::resolve`); a show only renders the text template,
//! measures text it has not seen (cached by the actor), and runs the
//! arithmetic below.

use std::fmt;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::common::config::Color;

/// A starting look; every explicit key overrides what the preset sets.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HudStylePreset {
    /// 520x120 rounded card, black at 65 %, white 44 pt semibold, centered.
    #[default]
    Card,
    /// Width from the text, height about 1.8x the font size, fully rounded.
    Pill,
    /// No background: the text alone with a soft shadow.
    Minimal,
    /// Small rounded box near the bottom edge.
    Toast,
}

/// One of nine points on the display's visible frame.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HudAnchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    #[default]
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Which displays show the HUD.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HudDisplayChoice {
    /// The display that shows the workspace.
    #[default]
    Target,
    /// The display that had focus when the switch was asked for.
    Focused,
    /// The display under the mouse cursor.
    Cursor,
    /// Every display.
    All,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HudFontWeight {
    Regular,
    Medium,
    #[default]
    Semibold,
    Bold,
}

/// `{ x, y }` in points: an offset (positive x = right, positive y = down) or
/// a padding (horizontal, vertical).
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(deny_unknown_fields)]
pub struct HudVector {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

impl HudVector {
    pub const fn new(x: f64, y: f64) -> Self { Self { x, y } }
}

/// A width or height: points, or `"auto"` (from the text plus padding).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum HudLength {
    Points(f64),
    Auto,
}

impl Serialize for HudLength {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            HudLength::Points(points) => serializer.serialize_f64(*points),
            HudLength::Auto => serializer.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for HudLength {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LengthVisitor;

        impl Visitor<'_> for LengthVisitor {
            type Value = HudLength;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a size in points or \"auto\"")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<HudLength, E> {
                Ok(HudLength::Points(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<HudLength, E> {
                Ok(HudLength::Points(value as f64))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<HudLength, E> {
                Ok(HudLength::Points(value as f64))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<HudLength, E> {
                if value == "auto" {
                    Ok(HudLength::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_any(LengthVisitor)
    }
}

/// `shadow = true | false | { radius, opacity }`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(untagged)]
pub enum HudShadowSetting {
    Enabled(bool),
    Custom(HudShadowParams),
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(deny_unknown_fields)]
pub struct HudShadowParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
}

/// A drop shadow: under the card when it has a background, under the text
/// when it has none.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct HudShadow {
    pub radius: f64,
    pub opacity: f64,
}

impl HudShadow {
    /// What `shadow = true` means when the preset has none of its own.
    pub const DEFAULT: HudShadow = HudShadow { radius: 16.0, opacity: 0.35 };
    /// The shadow sits this far below what casts it.
    pub const DROP: f64 = 2.0;
}

/// `[settings.ui.workspace_hud]`: an on-screen card naming the workspace,
/// drawn by rift the moment a workspace switch is decided.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHudSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub style: HudStylePreset,
    /// `{name}` (workspace name), `{index}` (1-based position), `{display}`
    /// (display name).
    #[serde(default = "default_hud_text")]
    pub text: String,
    /// How long the HUD stays fully shown, in milliseconds.
    #[serde(default = "default_hud_duration_ms")]
    pub duration_ms: f64,
    /// Fade in and out, in milliseconds.
    #[serde(default = "default_hud_fade_ms")]
    pub fade_ms: f64,
    /// A subtle 0.95 -> 1.0 pop when it appears.
    #[serde(default)]
    pub scale_in: bool,
    #[serde(default)]
    pub display: HudDisplayChoice,

    // Style keys: unset means the preset's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<HudAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<HudVector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<HudLength>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<HudLength>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<HudVector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blur_radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_color: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<HudShadowSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_weight: Option<HudFontWeight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_color: Option<Color>,
}

fn default_hud_text() -> String { "Workspace {name}".to_string() }
fn default_hud_duration_ms() -> f64 { 550.0 }
fn default_hud_fade_ms() -> f64 { 120.0 }

impl Default for WorkspaceHudSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            style: HudStylePreset::default(),
            text: default_hud_text(),
            duration_ms: default_hud_duration_ms(),
            fade_ms: default_hud_fade_ms(),
            scale_in: false,
            display: HudDisplayChoice::default(),
            anchor: None,
            offset: None,
            width: None,
            height: None,
            corner_radius: None,
            padding: None,
            background_color: None,
            blur_radius: None,
            border_color: None,
            border_width: None,
            shadow: None,
            font_size: None,
            font_weight: None,
            font_family: None,
            text_color: None,
        }
    }
}

/// The look and placement of the HUD with the preset applied: what the overlay
/// is built from, resolved once per config.
#[derive(Debug, PartialEq, Clone)]
pub struct HudStyle {
    pub anchor: HudAnchor,
    pub offset: HudVector,
    pub width: HudLength,
    pub height: HudLength,
    /// Clamped to half the card's shorter side when drawn (a large value gives
    /// a pill).
    pub corner_radius: f64,
    pub padding: HudVector,
    pub background_color: Color,
    /// Behind-window blur radius; 0 = off.
    pub blur_radius: f64,
    pub border_color: Color,
    pub border_width: f64,
    pub shadow: Option<HudShadow>,
    pub font_size: f64,
    pub font_weight: HudFontWeight,
    pub font_family: Option<String>,
    pub text_color: Color,
    pub fade_ms: f64,
    pub scale_in: bool,
}

const BLACK_65: Color = Color::new(0.0, 0.0, 0.0, 0.65);
const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);
const FAINT_WHITE: Color = Color::new(1.0, 1.0, 1.0, 0.2);
const CLEAR: Color = Color::new(0.0, 0.0, 0.0, 0.0);

impl HudStylePreset {
    /// The preset table.
    pub fn style(self) -> HudStyle {
        let card = HudStyle {
            anchor: HudAnchor::Center,
            offset: HudVector::new(0.0, 0.0),
            width: HudLength::Points(520.0),
            height: HudLength::Points(120.0),
            corner_radius: 18.0,
            padding: HudVector::new(20.0, 0.0),
            background_color: BLACK_65,
            blur_radius: 0.0,
            border_color: FAINT_WHITE,
            border_width: 0.0,
            shadow: Some(HudShadow::DEFAULT),
            font_size: 44.0,
            font_weight: HudFontWeight::Semibold,
            font_family: None,
            text_color: WHITE,
            fade_ms: default_hud_fade_ms(),
            scale_in: false,
        };
        match self {
            HudStylePreset::Card => card,
            HudStylePreset::Pill => HudStyle {
                width: HudLength::Auto,
                height: HudLength::Auto,
                // Clamped to height / 2.
                corner_radius: 1000.0,
                // SF at 44 pt is ~52.5 pt tall: 52.5 + 2 * 13 = ~79 = 1.8 x 44.
                padding: HudVector::new(32.0, 13.0),
                ..card
            },
            HudStylePreset::Minimal => HudStyle {
                width: HudLength::Auto,
                height: HudLength::Auto,
                corner_radius: 0.0,
                padding: HudVector::new(16.0, 8.0),
                background_color: CLEAR,
                shadow: Some(HudShadow { radius: 8.0, opacity: 0.7 }),
                ..card
            },
            HudStylePreset::Toast => HudStyle {
                anchor: HudAnchor::Bottom,
                offset: HudVector::new(0.0, -48.0),
                width: HudLength::Auto,
                height: HudLength::Auto,
                corner_radius: 12.0,
                padding: HudVector::new(20.0, 10.0),
                background_color: Color::new(0.0, 0.0, 0.0, 0.75),
                shadow: Some(HudShadow { radius: 10.0, opacity: 0.3 }),
                font_size: 22.0,
                font_weight: HudFontWeight::Medium,
                ..card
            },
        }
    }
}

impl WorkspaceHudSettings {
    /// The preset with every explicit key applied.
    pub fn resolve(&self) -> HudStyle {
        let preset = self.style.style();
        let shadow = match self.shadow {
            None => preset.shadow,
            Some(HudShadowSetting::Enabled(false)) => None,
            Some(HudShadowSetting::Enabled(true)) => {
                Some(preset.shadow.unwrap_or(HudShadow::DEFAULT))
            }
            Some(HudShadowSetting::Custom(params)) => {
                let base = preset.shadow.unwrap_or(HudShadow::DEFAULT);
                Some(HudShadow {
                    radius: params.radius.unwrap_or(base.radius),
                    opacity: params.opacity.unwrap_or(base.opacity),
                })
            }
        };
        HudStyle {
            anchor: self.anchor.unwrap_or(preset.anchor),
            offset: self.offset.unwrap_or(preset.offset),
            width: self.width.unwrap_or(preset.width),
            height: self.height.unwrap_or(preset.height),
            corner_radius: self.corner_radius.unwrap_or(preset.corner_radius),
            padding: self.padding.unwrap_or(preset.padding),
            background_color: self.background_color.unwrap_or(preset.background_color),
            blur_radius: self.blur_radius.unwrap_or(preset.blur_radius),
            border_color: self.border_color.unwrap_or(preset.border_color),
            border_width: self.border_width.unwrap_or(preset.border_width),
            shadow,
            font_size: self.font_size.unwrap_or(preset.font_size),
            font_weight: self.font_weight.unwrap_or(preset.font_weight),
            font_family: self.font_family.clone().or(preset.font_family),
            text_color: self.text_color.unwrap_or(preset.text_color),
            fade_ms: self.fade_ms,
            scale_in: self.scale_in,
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let mut non_negative = |name: &str, value: Option<f64>| {
            if let Some(value) = value
                && !(value >= 0.0)
            {
                issues.push(format!(
                    "ui.workspace_hud.{name} must be non-negative, got {value}"
                ));
            }
        };
        non_negative("duration_ms", Some(self.duration_ms));
        non_negative("fade_ms", Some(self.fade_ms));
        non_negative("corner_radius", self.corner_radius);
        non_negative("blur_radius", self.blur_radius);
        non_negative("border_width", self.border_width);
        non_negative("padding.x", self.padding.map(|padding| padding.x));
        non_negative("padding.y", self.padding.map(|padding| padding.y));
        if let Some(HudShadowSetting::Custom(params)) = self.shadow {
            non_negative("shadow.radius", params.radius);
            if let Some(opacity) = params.opacity
                && !(0.0..=1.0).contains(&opacity)
            {
                issues.push(format!(
                    "ui.workspace_hud.shadow.opacity must be between 0.0 and 1.0, got {opacity}"
                ));
            }
        }
        for (name, length) in [("width", self.width), ("height", self.height)] {
            if let Some(HudLength::Points(points)) = length
                && !(points > 0.0)
            {
                issues.push(format!(
                    "ui.workspace_hud.{name} must be positive or \"auto\", got {points}"
                ));
            }
        }
        if let Some(size) = self.font_size
            && !(size > 0.0)
        {
            issues.push(format!(
                "ui.workspace_hud.font_size must be positive, got {size}"
            ));
        }
        issues
    }
}

/// Fill in `{name}`, `{index}` and `{display}`; anything else stays as written.
pub fn render_text(template: &str, name: &str, index: usize, display: &str) -> String {
    let mut out = String::with_capacity(template.len() + name.len() + display.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        if let Some(after) = tail.strip_prefix("{name}") {
            out.push_str(name);
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{index}") {
            use std::fmt::Write;
            let _ = write!(out, "{index}");
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{display}") {
            out.push_str(display);
            rest = after;
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The measured text of one show.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    /// Typographic width of the rendered text.
    pub width: f64,
    /// Ascent + descent + leading of the font.
    pub line_height: f64,
}

impl HudStyle {
    /// Whether the card's size depends on the text.
    pub fn is_auto_sized(&self) -> bool {
        matches!(self.width, HudLength::Auto) || matches!(self.height, HudLength::Auto)
    }

    /// The card's size for `text` on a display whose visible frame is `room`.
    ///
    /// Auto height is the line height plus vertical padding; auto width is the
    /// text width plus horizontal padding, never narrower than the card is
    /// tall (a short name keeps a pill round). Whole points, at most `room`.
    pub fn card_size(&self, text: TextMetrics, room: CGSize) -> CGSize {
        let height = match self.height {
            HudLength::Points(points) => points,
            HudLength::Auto => text.line_height + 2.0 * self.padding.y,
        };
        let width = match self.width {
            HudLength::Points(points) => points,
            HudLength::Auto => (text.width + 2.0 * self.padding.x).max(height),
        };
        CGSize::new(
            width.ceil().min(room.width.max(1.0)).max(1.0),
            height.ceil().min(room.height.max(1.0)).max(1.0),
        )
    }

    /// The corner radius drawn on a card of `size`.
    pub fn corner_radius_for(&self, size: CGSize) -> f64 {
        self.corner_radius.max(0.0).min(size.width / 2.0).min(size.height / 2.0)
    }

    /// Room around the card inside the overlay window for the shadow.
    pub fn shadow_margin(&self) -> f64 {
        self.shadow.map_or(0.0, |shadow| {
            (2.0 * shadow.radius.max(0.0) + HudShadow::DROP).ceil()
        })
    }

    /// Whether the card draws a background (the shadow then goes under the
    /// card; without one it goes under the text).
    pub fn has_background(&self) -> bool { self.background_color.a > 0.0 || self.blur_radius > 0.0 }

    /// Where a card of `size` goes on a display with visible frame `visible`
    /// (global CG coordinates: below the menu bar, clear of the Dock): at the
    /// anchor, moved by the offset, then kept fully on the visible frame.
    pub fn place(&self, size: CGSize, visible: CGRect) -> CGRect {
        let (left, top) = (visible.origin.x, visible.origin.y);
        let (right, bottom) = (left + visible.size.width, top + visible.size.height);
        let (w, h) = (size.width, size.height);
        let x = match self.anchor {
            HudAnchor::TopLeft | HudAnchor::Left | HudAnchor::BottomLeft => left,
            HudAnchor::Top | HudAnchor::Center | HudAnchor::Bottom => {
                left + (visible.size.width - w) / 2.0
            }
            HudAnchor::TopRight | HudAnchor::Right | HudAnchor::BottomRight => right - w,
        };
        let y = match self.anchor {
            HudAnchor::TopLeft | HudAnchor::Top | HudAnchor::TopRight => top,
            HudAnchor::Left | HudAnchor::Center | HudAnchor::Right => {
                top + (visible.size.height - h) / 2.0
            }
            HudAnchor::BottomLeft | HudAnchor::Bottom | HudAnchor::BottomRight => bottom - h,
        };
        let x = (x + self.offset.x).round().min(right - w).max(left);
        let y = (y + self.offset.y).round().min(bottom - h).max(top);
        CGRect::new(CGPoint::new(x, y), size)
    }

    /// The overlay window's frame for a card at `card` on a display with
    /// visible frame `visible`: the card plus the shadow margin on every side.
    /// An auto-width card gets the visible frame's whole width instead, so the
    /// window never changes and a show of a wider or narrower name only moves
    /// layers (auto height depends on the font alone, so it is fixed too).
    pub fn window_frame(&self, card: CGRect, visible: CGRect) -> CGRect {
        let margin = self.shadow_margin();
        let (x, width) = match self.width {
            HudLength::Auto => (visible.origin.x, visible.size.width),
            HudLength::Points(_) => (card.origin.x, card.size.width),
        };
        CGRect::new(
            CGPoint::new(x - margin, card.origin.y - margin),
            CGSize::new(width + 2.0 * margin, card.size.height + 2.0 * margin),
        )
    }
}

/// A display the HUD can show on.
#[derive(Debug, Clone, PartialEq)]
pub struct HudDisplay {
    pub uuid: String,
    /// Visible frame in global CG coordinates (below the menu bar, clear of
    /// the Dock), from rift's cached screen state.
    pub frame: CGRect,
    pub backing_scale: f64,
}

/// Most displays the HUD tracks (one bit each in a mask).
pub const MAX_HUD_DISPLAYS: usize = 64;

fn bit(index: usize) -> u64 {
    if index < MAX_HUD_DISPLAYS {
        1 << index
    } else {
        0
    }
}

/// Bit mask over `displays` of where to show the HUD.
///
/// `target` shows the workspace; `focused` had focus when the switch was asked
/// for; `cursor` is the mouse position (global CG). A display that cannot be
/// found falls back to `target`; a cursor outside every visible frame (say, on
/// a menu bar) picks the nearest display.
pub fn choose_displays(
    choice: HudDisplayChoice,
    displays: &[HudDisplay],
    target: &str,
    focused: Option<&str>,
    cursor: Option<CGPoint>,
) -> u64 {
    let index_of = |uuid: &str| displays.iter().position(|display| display.uuid == uuid);
    let target_mask = index_of(target).map_or(0, bit);
    match choice {
        HudDisplayChoice::Target => target_mask,
        HudDisplayChoice::Focused => focused.and_then(index_of).map_or(target_mask, bit),
        HudDisplayChoice::All => (0..displays.len()).map(bit).fold(0, |mask, b| mask | b),
        HudDisplayChoice::Cursor => {
            let Some(point) = cursor else { return target_mask };
            displays
                .iter()
                .enumerate()
                .map(|(index, display)| (index, distance_to(display.frame, point)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(target_mask, |(index, _)| bit(index))
        }
    }
}

/// Distance from `point` to `rect` (0 inside).
fn distance_to(rect: CGRect, point: CGPoint) -> f64 {
    let dx = (rect.origin.x - point.x)
        .max(point.x - (rect.origin.x + rect.size.width))
        .max(0.0);
    let dy = (rect.origin.y - point.y)
        .max(point.y - (rect.origin.y + rect.size.height))
        .max(0.0);
    dx.hypot(dy)
}

/// Where the HUD is in its show / fade / hide cycle, per display bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HudPhase {
    #[default]
    Hidden,
    Shown,
    FadingOut,
}

/// What a show does to each display's overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowPlan {
    /// Shown before, not chosen now: order out at once (newest replaces older).
    pub order_out: u64,
    /// Chosen and not on screen: order in once after the layer update.
    pub order_in: u64,
    /// Every chosen display: new text, full opacity, timer restarted.
    pub update: u64,
}

/// What the HUD timer firing does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerStep {
    /// The shown time is up: fade these out, then wait `fade_ms`.
    FadeOut(u64),
    /// The fade is over: order these out.
    OrderOut(u64),
    Idle,
}

/// The HUD's show/fade/hide cycle. One timer drives it: re-armed for
/// `duration_ms` on every show, for `fade_ms` when the fade starts. A show
/// during the shown time or the fade keeps the overlay on screen (no order
/// out/in, so no flicker) and restarts the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HudPresenter {
    shown: u64,
    phase: HudPhase,
}

impl HudPresenter {
    pub fn phase(&self) -> HudPhase { self.phase }

    /// Displays whose overlay is on screen.
    pub fn shown(&self) -> u64 { self.shown }

    pub fn show(&mut self, chosen: u64) -> ShowPlan {
        let plan = ShowPlan {
            order_out: self.shown & !chosen,
            order_in: chosen & !self.shown,
            update: chosen,
        };
        self.shown = chosen;
        self.phase = if chosen == 0 {
            HudPhase::Hidden
        } else {
            HudPhase::Shown
        };
        plan
    }

    pub fn timer_fired(&mut self) -> TimerStep {
        match self.phase {
            HudPhase::Shown => {
                self.phase = HudPhase::FadingOut;
                TimerStep::FadeOut(self.shown)
            }
            HudPhase::FadingOut => {
                self.phase = HudPhase::Hidden;
                TimerStep::OrderOut(std::mem::take(&mut self.shown))
            }
            HudPhase::Hidden => TimerStep::Idle,
        }
    }

    /// Forget everything (the overlays are being rebuilt); returns what was on
    /// screen.
    pub fn reset(&mut self) -> u64 {
        self.phase = HudPhase::Hidden;
        std::mem::take(&mut self.shown)
    }
}

/// Indices of the set bits of `mask`, lowest first.
pub fn mask_indices(mask: u64) -> impl Iterator<Item = usize> {
    (0..MAX_HUD_DISPLAYS).filter(move |index| mask & (1 << index) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::Config;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn parse_config(toml: &str) -> anyhow::Result<Config> {
        Config::parse(&format!("[settings.ui.workspace_hud]\n{toml}\n[keys]\n"))
    }

    fn parse(toml: &str) -> WorkspaceHudSettings {
        parse_config(toml)
            .expect("workspace_hud settings should parse")
            .settings
            .ui
            .workspace_hud
    }

    fn style(toml: &str) -> HudStyle { parse(toml).resolve() }

    const TEXT: TextMetrics = TextMetrics {
        width: 300.0,
        line_height: 52.5,
    };

    #[test]
    fn defaults_are_off_and_match_the_external_hud() {
        let settings = Config::default().settings.ui.workspace_hud;
        assert_eq!(settings, WorkspaceHudSettings::default());
        assert!(!settings.enabled);
        assert_eq!(settings.text, "Workspace {name}");
        assert_eq!(settings.duration_ms, 550.0);
        assert_eq!(settings.fade_ms, 120.0);
        assert_eq!(settings.display, HudDisplayChoice::Target);

        let style = settings.resolve();
        assert_eq!(style.width, HudLength::Points(520.0));
        assert_eq!(style.height, HudLength::Points(120.0));
        assert_eq!(style.corner_radius, 18.0);
        assert_eq!(style.background_color, Color::new(0.0, 0.0, 0.0, 0.65));
        assert_eq!(style.text_color, Color::new(1.0, 1.0, 1.0, 1.0));
        assert_eq!(style.font_size, 44.0);
        assert_eq!(style.font_weight, HudFontWeight::Semibold);
        assert_eq!(style.anchor, HudAnchor::Center);
        assert_eq!(style.blur_radius, 0.0);
        assert!(style.shadow.is_some());
        assert!(settings.validate().is_empty());
    }

    #[test]
    fn parses_every_key() {
        let settings = parse(
            r#"
            enabled = true
            style = "pill"
            text = "{index}: {name} on {display}"
            duration_ms = 800
            fade_ms = 90.5
            scale_in = true
            display = "cursor"
            anchor = "top-right"
            offset = { x = -10, y = 24 }
            width = "auto"
            height = 64
            corner_radius = 9
            padding = { x = 12.5, y = 4 }
            background_color = { r = 0.1, g = 0.2, b = 0.3, a = 0.4 }
            blur_radius = 20
            border_color = { r = 1.0, a = 0.5 }
            border_width = 1.5
            shadow = { radius = 4 }
            font_size = 30
            font_weight = "bold"
            font_family = "Menlo"
            text_color = { r = 1.0, g = 1.0 }
            "#,
        );
        assert!(settings.enabled);
        assert_eq!(settings.style, HudStylePreset::Pill);
        assert_eq!(settings.duration_ms, 800.0);
        assert_eq!(settings.fade_ms, 90.5);
        assert!(settings.scale_in);
        assert_eq!(settings.display, HudDisplayChoice::Cursor);
        assert_eq!(settings.anchor, Some(HudAnchor::TopRight));
        assert_eq!(settings.offset, Some(HudVector::new(-10.0, 24.0)));
        assert_eq!(settings.width, Some(HudLength::Auto));
        assert_eq!(settings.height, Some(HudLength::Points(64.0)));
        assert_eq!(settings.padding, Some(HudVector::new(12.5, 4.0)));
        assert_eq!(settings.border_color, Some(Color::new(1.0, 0.0, 0.0, 0.5)));
        assert_eq!(
            settings.shadow,
            Some(HudShadowSetting::Custom(HudShadowParams {
                radius: Some(4.0),
                opacity: None
            }))
        );
        assert_eq!(settings.font_weight, Some(HudFontWeight::Bold));
        assert_eq!(settings.font_family.as_deref(), Some("Menlo"));
        assert!(settings.validate().is_empty());

        for (toml, shadow) in [
            ("shadow = false", HudShadowSetting::Enabled(false)),
            ("shadow = true", HudShadowSetting::Enabled(true)),
        ] {
            assert_eq!(parse(toml).shadow, Some(shadow));
        }
        for anchor in [
            "top-left",
            "top",
            "top-right",
            "left",
            "center",
            "right",
            "bottom-left",
            "bottom",
            "bottom-right",
        ] {
            assert!(
                parse(&format!("anchor = \"{anchor}\"")).anchor.is_some(),
                "{anchor}"
            );
        }
        for display in ["target", "focused", "cursor", "all"] {
            parse(&format!("display = \"{display}\""));
        }
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        for toml in [
            "position = \"center\"",
            "width = \"wide\"",
            "anchor = \"middle\"",
            "style = \"neon\"",
            "font_weight = \"heavy\"",
            "offset = { x = 1, z = 2 }",
            "shadow = { radius = 1, blur = 2 }",
        ] {
            assert!(parse_config(toml).is_err(), "{toml} should not parse");
        }
        let issues = parse(
            "duration_ms = -1\nfont_size = 0\nwidth = 0\nshadow = { opacity = 2 }\n\
             padding = { x = -1 }\nborder_width = -2",
        )
        .validate();
        assert_eq!(issues.len(), 6, "{issues:?}");
    }

    #[test]
    fn hud_settings_round_trip_through_toml() {
        let settings = parse("style = \"toast\"\nwidth = \"auto\"\nheight = 40\nshadow = true");
        let toml = toml::to_string(&settings).expect("serializes");
        let back: WorkspaceHudSettings = toml::from_str(&toml).expect("parses back");
        assert_eq!(back, settings);
    }

    #[test]
    fn presets_resolve_to_their_table_rows() {
        let card = style("");
        assert_eq!(card, HudStylePreset::Card.style());

        let pill = style("style = \"pill\"");
        assert_eq!((pill.width, pill.height), (HudLength::Auto, HudLength::Auto));
        assert_eq!(pill.anchor, HudAnchor::Center);
        assert_eq!(pill.background_color, card.background_color);
        let size = pill.card_size(TEXT, CGSize::new(2000.0, 1000.0));
        assert_eq!(pill.corner_radius_for(size), size.height / 2.0, "fully rounded");
        assert!(
            (size.height - 1.8 * 44.0).abs() < 1.5,
            "pill height {} is about 1.8x the font size",
            size.height
        );

        let minimal = style("style = \"minimal\"");
        assert_eq!(minimal.background_color.a, 0.0);
        assert!(!minimal.has_background());
        assert!(minimal.shadow.is_some(), "the text keeps a soft shadow");

        let toast = style("style = \"toast\"");
        assert_eq!(toast.anchor, HudAnchor::Bottom);
        assert!(toast.offset.y < 0.0, "lifted off the bottom edge");
        assert_eq!(toast.corner_radius, 12.0);
        assert!(toast.font_size < card.font_size);
    }

    #[test]
    fn explicit_keys_override_the_preset() {
        let toast = style(
            "style = \"toast\"\nanchor = \"top\"\nwidth = 300\ncorner_radius = 4\n\
             font_size = 30\nshadow = false\nbackground_color = { r = 1.0, a = 0.5 }",
        );
        assert_eq!(toast.anchor, HudAnchor::Top);
        assert_eq!(toast.width, HudLength::Points(300.0));
        assert_eq!(toast.height, HudLength::Auto, "unset keys keep the preset's");
        assert_eq!(toast.offset, HudVector::new(0.0, -48.0));
        assert_eq!(toast.corner_radius, 4.0);
        assert_eq!(toast.font_size, 30.0);
        assert_eq!(toast.font_weight, HudFontWeight::Medium);
        assert_eq!(toast.shadow, None);
        assert_eq!(toast.background_color, Color::new(1.0, 0.0, 0.0, 0.5));

        // A partial shadow table fills the rest from the preset's shadow.
        let minimal = style("style = \"minimal\"\nshadow = { opacity = 0.2 }");
        assert_eq!(minimal.shadow, Some(HudShadow { radius: 8.0, opacity: 0.2 }));
        // `shadow = true` on a preset without one uses the default shadow.
        let card = style("shadow = false");
        assert_eq!(card.shadow, None);
        let card = style("shadow = true");
        assert_eq!(card.shadow, Some(HudShadow::DEFAULT));
        // Timing keys are not part of any preset.
        let pill = style("style = \"pill\"\nfade_ms = 0\nscale_in = true");
        assert_eq!(pill.fade_ms, 0.0);
        assert!(pill.scale_in);
    }

    #[test]
    fn text_template_fills_placeholders() {
        assert_eq!(render_text("Workspace {name}", "3", 3, "DELL"), "Workspace 3");
        assert_eq!(
            render_text("{index}. {name} @ {display}", "notes", 14, "Built-in"),
            "14. notes @ Built-in"
        );
        assert_eq!(render_text("{name}{name}", "T", 1, ""), "TT");
        assert_eq!(render_text("{unknown} {name", "x", 1, ""), "{unknown} {name");
        assert_eq!(render_text("no placeholders", "x", 1, ""), "no placeholders");
        assert_eq!(render_text("", "x", 1, ""), "");
        assert_eq!(render_text("{{name}}", "x", 1, ""), "{x}");
    }

    #[test]
    fn auto_width_follows_the_text_and_padding() {
        let room = CGSize::new(1920.0, 1055.0);
        let pill = style("style = \"pill\"");
        let size = pill.card_size(TEXT, room);
        assert_eq!(size.width, (300.0_f64 + 64.0).ceil());
        assert_eq!(size.height, (52.5_f64 + 26.0).ceil());

        // Never narrower than tall: a one-letter name stays a circle.
        let narrow = pill.card_size(TextMetrics { width: 10.0, line_height: 52.5 }, room);
        assert_eq!(narrow.width, narrow.height);

        // Wider than the display: capped at the visible frame.
        let wide = pill.card_size(
            TextMetrics {
                width: 5000.0,
                line_height: 52.5,
            },
            room,
        );
        assert_eq!(wide.width, 1920.0);

        // Fixed sizes ignore the text.
        let card = style("");
        assert_eq!(card.card_size(TEXT, room), CGSize::new(520.0, 120.0));
        assert_eq!(
            card.card_size(
                TextMetrics {
                    width: 900.0,
                    line_height: 80.0
                },
                room
            ),
            CGSize::new(520.0, 120.0)
        );
        // Mixed: fixed height, auto width.
        let mixed = style("width = \"auto\"\nheight = 50\npadding = { x = 10 }");
        assert_eq!(mixed.card_size(TEXT, room), CGSize::new(320.0, 50.0));
        assert!(mixed.is_auto_sized());
        assert!(!card.is_auto_sized());

        // Fractional widths round up to whole points.
        let frac = pill.card_size(
            TextMetrics {
                width: 100.2,
                line_height: 50.1,
            },
            room,
        );
        assert_eq!(frac, CGSize::new(165.0, 77.0));
    }

    #[test]
    fn corner_radius_and_shadow_margin() {
        let card = style("");
        assert_eq!(card.corner_radius_for(CGSize::new(520.0, 120.0)), 18.0);
        assert_eq!(card.corner_radius_for(CGSize::new(20.0, 30.0)), 10.0);
        assert_eq!(
            style("corner_radius = 0").corner_radius_for(CGSize::new(9.0, 9.0)),
            0.0
        );

        assert_eq!(card.shadow_margin(), (2.0 * 16.0 + HudShadow::DROP).ceil());
        let flat = style("shadow = false");
        assert_eq!(flat.shadow_margin(), 0.0);
        let visible = landscape();
        let card_rect = rect(100.0, 200.0, 520.0, 120.0);
        assert_eq!(flat.window_frame(card_rect, visible), card_rect);
        let m = card.shadow_margin();
        assert_eq!(
            card.window_frame(card_rect, visible),
            rect(100.0 - m, 200.0 - m, 520.0 + 2.0 * m, 120.0 + 2.0 * m)
        );
    }

    #[test]
    fn an_auto_width_overlay_spans_the_display_so_shows_never_reshape_it() {
        let pill = style("style = \"pill\"");
        let visible = landscape();
        let m = pill.shadow_margin();
        let frames: Vec<CGRect> = [10.0, 300.0, 900.0]
            .into_iter()
            .map(|width| {
                let size = pill.card_size(TextMetrics { width, line_height: 52.5 }, visible.size);
                pill.window_frame(pill.place(size, visible), visible)
            })
            .collect();
        assert!(frames.iter().all(|frame| *frame == frames[0]), "{frames:?}");
        assert_eq!(frames[0].origin.x, -m);
        assert_eq!(frames[0].size.width, 1920.0 + 2.0 * m);
        assert_eq!(frames[0].size.height, 79.0 + 2.0 * m);
        // Every card fits inside it, whatever the name.
        for width in [10.0, 300.0, 5000.0] {
            let size = pill.card_size(TextMetrics { width, line_height: 52.5 }, visible.size);
            let card = pill.place(size, visible);
            assert!(card.origin.x >= frames[0].origin.x + m - 0.001);
            assert!(
                card.origin.x + card.size.width
                    <= frames[0].origin.x + frames[0].size.width - m + 0.001
            );
            assert_eq!(card.origin.y, frames[0].origin.y + m);
        }
    }

    /// A 1920x1080 landscape display with a 25 pt menu bar: visible frame.
    fn landscape() -> CGRect { rect(0.0, 25.0, 1920.0, 1055.0) }
    /// A 1080x1920 portrait display right of it, with the Dock (70 pt) at its bottom.
    fn portrait() -> CGRect { rect(1920.0, 25.0, 1080.0, 1825.0) }

    fn placed(anchor: &str, offset: (f64, f64), visible: CGRect) -> CGRect {
        let style = style(&format!(
            "anchor = \"{anchor}\"\noffset = {{ x = {}, y = {} }}",
            offset.0, offset.1
        ));
        style.place(CGSize::new(520.0, 120.0), visible)
    }

    #[test]
    fn anchors_place_the_card_on_a_landscape_display() {
        let v = landscape();
        // Middle row: 25 + (1055 - 120) / 2 = 492.5, rounded to a whole point.
        let cases = [
            ("top-left", (0.0, 25.0)),
            ("top", (700.0, 25.0)),
            ("top-right", (1400.0, 25.0)),
            ("left", (0.0, 493.0)),
            ("center", (700.0, 493.0)),
            ("right", (1400.0, 493.0)),
            ("bottom-left", (0.0, 960.0)),
            ("bottom", (700.0, 960.0)),
            ("bottom-right", (1400.0, 960.0)),
        ];
        for (anchor, (x, y)) in cases {
            assert_eq!(
                placed(anchor, (0.0, 0.0), v),
                rect(x, y, 520.0, 120.0),
                "{anchor}"
            );
        }
        // The default (center) uses the visible frame, not the whole display.
        assert_eq!(
            style("").place(CGSize::new(520.0, 120.0), v),
            rect(700.0, 493.0, 520.0, 120.0)
        );
    }

    #[test]
    fn anchors_place_the_card_on_a_portrait_display() {
        let v = portrait();
        let cases = [
            ("top-left", (1920.0, 25.0)),
            ("top", (2200.0, 25.0)),
            ("top-right", (2480.0, 25.0)),
            ("left", (1920.0, 878.0)),
            ("center", (2200.0, 878.0)),
            ("right", (2480.0, 878.0)),
            ("bottom-left", (1920.0, 1730.0)),
            ("bottom", (2200.0, 1730.0)),
            ("bottom-right", (2480.0, 1730.0)),
        ];
        for (anchor, (x, y)) in cases {
            assert_eq!(
                placed(anchor, (0.0, 0.0), v),
                rect(x, y, 520.0, 120.0),
                "{anchor}"
            );
        }
    }

    #[test]
    fn offsets_move_right_and_down_from_the_anchor() {
        let v = landscape();
        assert_eq!(placed("top", (30.0, 40.0), v), rect(730.0, 65.0, 520.0, 120.0));
        assert_eq!(
            placed("bottom", (0.0, -48.0), v),
            rect(700.0, 912.0, 520.0, 120.0)
        );
        assert_eq!(
            placed("center", (-100.0, -50.0), v),
            rect(600.0, 443.0, 520.0, 120.0)
        );
        assert_eq!(
            placed("bottom-right", (-24.0, -24.0), v),
            rect(1376.0, 936.0, 520.0, 120.0)
        );
        // Fractional offsets land on whole points.
        assert_eq!(
            placed("top-left", (10.4, 10.6), v),
            rect(10.0, 36.0, 520.0, 120.0)
        );
    }

    #[test]
    fn the_card_stays_fully_on_the_visible_frame() {
        let v = landscape();
        // Pushed past every edge.
        assert_eq!(
            placed("top-left", (-500.0, -500.0), v),
            rect(0.0, 25.0, 520.0, 120.0)
        );
        assert_eq!(
            placed("bottom-right", (900.0, 900.0), v),
            rect(1400.0, 960.0, 520.0, 120.0)
        );
        assert_eq!(
            placed("top", (0.0, -10.0), v),
            rect(700.0, 25.0, 520.0, 120.0),
            "not under the menu bar"
        );
        assert_eq!(
            placed("right", (5000.0, 0.0), v),
            rect(1400.0, 493.0, 520.0, 120.0)
        );
        let p = portrait();
        assert_eq!(
            placed("bottom", (0.0, 300.0), p),
            rect(2200.0, 1730.0, 520.0, 120.0),
            "clear of the Dock"
        );
        assert_eq!(
            placed("left", (-300.0, 0.0), p),
            rect(1920.0, 878.0, 520.0, 120.0),
            "not on the other display"
        );

        // A card as large as the room pins to the origin.
        let tiny = rect(0.0, 25.0, 400.0, 100.0);
        let size = style("").card_size(TEXT, tiny.size);
        assert_eq!(size, CGSize::new(400.0, 100.0));
        assert_eq!(style("anchor = \"bottom-right\"").place(size, tiny), tiny);
    }

    fn displays() -> Vec<HudDisplay> {
        [("left", landscape()), ("right", portrait())]
            .into_iter()
            .map(|(uuid, frame)| HudDisplay {
                uuid: uuid.to_string(),
                frame,
                backing_scale: 2.0,
            })
            .collect()
    }

    #[test]
    fn display_choice_picks_the_target_focused_cursor_or_all() {
        let d = displays();
        let (left, right) = (0b01, 0b10);
        use HudDisplayChoice::*;
        assert_eq!(choose_displays(Target, &d, "right", Some("left"), None), right);
        assert_eq!(choose_displays(Focused, &d, "right", Some("left"), None), left);
        assert_eq!(
            choose_displays(Focused, &d, "right", None, None),
            right,
            "falls back to target"
        );
        assert_eq!(choose_displays(Focused, &d, "right", Some("gone"), None), right);
        assert_eq!(choose_displays(All, &d, "right", None, None), left | right);
        let in_right = CGPoint::new(2500.0, 900.0);
        assert_eq!(choose_displays(Cursor, &d, "left", None, Some(in_right)), right);
        // On the left display's menu bar (outside every visible frame): nearest.
        let on_menu_bar = CGPoint::new(300.0, 10.0);
        assert_eq!(
            choose_displays(Cursor, &d, "right", None, Some(on_menu_bar)),
            left
        );
        assert_eq!(
            choose_displays(Cursor, &d, "right", None, None),
            right,
            "no cursor: target"
        );
        // An unknown target shows nowhere unless another choice finds a display.
        assert_eq!(choose_displays(Target, &d, "gone", None, None), 0);
        assert_eq!(choose_displays(Target, &[], "left", None, None), 0);
        assert_eq!(mask_indices(left | right).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn presenter_shows_fades_and_hides() {
        let mut presenter = HudPresenter::default();
        assert_eq!(presenter.timer_fired(), TimerStep::Idle);

        let plan = presenter.show(0b01);
        assert_eq!(plan, ShowPlan {
            order_out: 0,
            order_in: 0b01,
            update: 0b01
        });
        assert_eq!(presenter.phase(), HudPhase::Shown);

        assert_eq!(presenter.timer_fired(), TimerStep::FadeOut(0b01));
        assert_eq!(presenter.phase(), HudPhase::FadingOut);
        assert_eq!(presenter.timer_fired(), TimerStep::OrderOut(0b01));
        assert_eq!(presenter.phase(), HudPhase::Hidden);
        assert_eq!(presenter.shown(), 0);
        assert_eq!(presenter.timer_fired(), TimerStep::Idle);

        // Back again after hiding: ordered in again.
        assert_eq!(presenter.show(0b01).order_in, 0b01);
    }

    #[test]
    fn rapid_shows_coalesce_on_the_window_already_up() {
        let mut presenter = HudPresenter::default();
        presenter.show(0b01);
        // A second switch while shown: same window, no order out/in.
        let plan = presenter.show(0b01);
        assert_eq!(plan, ShowPlan {
            order_out: 0,
            order_in: 0,
            update: 0b01
        });
        // Mid-fade: back to fully shown without leaving the screen.
        assert_eq!(presenter.timer_fired(), TimerStep::FadeOut(0b01));
        let plan = presenter.show(0b01);
        assert_eq!(plan, ShowPlan {
            order_out: 0,
            order_in: 0,
            update: 0b01
        });
        assert_eq!(presenter.phase(), HudPhase::Shown, "the cycle restarts");
        // The fade that was cut short never orders it out.
        assert_eq!(presenter.timer_fired(), TimerStep::FadeOut(0b01));

        // Another display: the newest replaces the older at once.
        let plan = presenter.show(0b10);
        assert_eq!(plan, ShowPlan {
            order_out: 0b01,
            order_in: 0b10,
            update: 0b10
        });
        assert_eq!(presenter.reset(), 0b10);
        assert_eq!(presenter.phase(), HudPhase::Hidden);
    }
}
