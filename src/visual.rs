//! Visual style definitions for Claude session indicators
//!
//! Centralized visual semantics for panel indicator dots. Consumed by:
//! - richspace-babel (Cairo rendering via RenderDot)
//! - richmon-babel (Pango markup, subset of features)
//!
//! ## Design Philosophy
//!
//! Visual properties are computed from session state here, not in the renderers.
//! This ensures consistent semantics across all panel indicators:
//! - Color conveys activity state (thinking, tool use, idle, etc.)
//! - Ring conveys dialogue state (asking question, awaiting approval)
//! - Texture adds subtle differentiation (stripes for questions)
//! - Pulse/glow conveys urgency or activity level

use scrollparse::claude::ActivityState;

// ═══════════════════════════════════════════════════════════════════════════════
// Core Types
// ═══════════════════════════════════════════════════════════════════════════════

/// Complete visual style for a session indicator dot
#[derive(Debug, Clone)]
pub struct DotStyle {
    /// Base fill color (RGB, 0.0-1.0)
    pub color: Rgb,

    /// Optional texture overlay (stripes, concentric rings, etc.)
    pub texture: DotTexture,

    /// Optional outline ring around the dot
    pub ring: Option<RingStyle>,

    /// Pulse/glow intensity (0.0 = none, 1.0 = full glow)
    pub pulse: f64,

    /// Size multiplier (1.0 = default, 0.5 = half, 2.0 = double)
    pub scale: f64,
}

/// RGB color (0.0-1.0 per channel)
#[derive(Debug, Clone, Copy)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Rgb {
    pub const fn new(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b }
    }

    /// Parse from hex string (#RRGGBB or RRGGBB)
    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return Self::new(0.5, 0.5, 0.5); // Gray fallback
        }

        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(128) as f64 / 255.0;
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(128) as f64 / 255.0;
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(128) as f64 / 255.0;

        Self { r, g, b }
    }

    /// Convert to hex string (#RRGGBB)
    pub fn to_hex(&self) -> String {
        format!(
            "#{:02x}{:02x}{:02x}",
            (self.r * 255.0) as u8,
            (self.g * 255.0) as u8,
            (self.b * 255.0) as u8
        )
    }

    /// Convert to tuple for destructuring
    pub fn to_tuple(&self) -> (f64, f64, f64) {
        (self.r, self.g, self.b)
    }
}

/// Texture pattern for dot fill
#[derive(Debug, Clone, Default)]
pub enum DotTexture {
    /// Solid fill (default)
    #[default]
    Solid,

    /// Parallel stripes
    Stripes {
        /// Rotation angle in radians (0 = horizontal, PI/4 = diagonal)
        angle: f64,
        /// Stripe width as fraction of dot radius
        stripe_width: f64,
        /// Gap width as fraction of dot radius
        gap_width: f64,
        /// Secondary color for gaps (None = transparent)
        secondary: Option<Rgb>,
    },

    /// Concentric rings (bullseye pattern)
    Concentric {
        /// Number of rings
        ring_count: u8,
        /// Secondary color for alternating rings
        secondary: Rgb,
    },
}

/// Outline ring around the dot
#[derive(Debug, Clone)]
pub struct RingStyle {
    /// Ring color
    pub color: Rgb,

    /// Ring thickness as fraction of dot radius (e.g., 0.15 = 15% of radius)
    pub thickness: f64,

    /// Gap between dot edge and ring inner edge (0 = touching)
    pub gap: f64,

    /// Ring pattern
    pub pattern: RingPattern,
}

/// Ring stroke pattern
#[derive(Debug, Clone, Default)]
pub enum RingPattern {
    /// Solid line
    #[default]
    Solid,

    /// Dashed line
    Dashed {
        /// Dash length as fraction of circumference
        dash: f64,
        /// Gap length as fraction of circumference
        gap: f64,
    },

    /// Animated pulse (ring fades in/out)
    Pulsing,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Color Palette
// ═══════════════════════════════════════════════════════════════════════════════

/// Activity state colors — the hue of the worker's breath
pub mod colors {
    use super::Rgb;

    /// Idle: Dim gray — resting, quiet
    pub const IDLE: Rgb = Rgb::new(0.4, 0.4, 0.4);

    /// Thinking: Gold — mind at work, tokens flowing
    pub const THINKING: Rgb = Rgb::new(0.94, 0.75, 0.25);

    /// ToolUse: Cyan — hands moving, commands executing
    pub const TOOL_USE: Rgb = Rgb::new(0.25, 0.75, 0.94);

    /// PlanApproval: Purple — considering the path ahead
    pub const PLAN_APPROVAL: Rgb = Rgb::new(0.75, 0.5, 0.94);

    /// BackgroundTask: Teal — working in the depths
    pub const BACKGROUND_TASK: Rgb = Rgb::new(0.25, 0.94, 0.75);

    /// AwaitingInput: Rose — seeking guidance
    pub const AWAITING_INPUT: Rgb = Rgb::new(0.94, 0.25, 0.5);

    /// Unknown: Darker gray — newly arrived, state uncertain
    pub const UNKNOWN: Rgb = Rgb::new(0.27, 0.27, 0.27);

    /// Ring color for asking_question state
    pub const QUESTION_RING: Rgb = Rgb::new(1.0, 1.0, 1.0);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Style Computation
// ═══════════════════════════════════════════════════════════════════════════════

impl DotStyle {
    /// Compute visual style from activity state and dialogue flags
    ///
    /// # Arguments
    /// - `state`: Current activity state (Idle, Thinking, ToolUse, etc.)
    /// - `asking_question`: True if Claude's last message ended with a question
    ///
    /// # Visual Semantics
    /// - Color: Activity state (what Claude is doing)
    /// - Ring: Dialogue state (asking question → white ring with subtle stripes)
    /// - Pulse: Reserved for urgency/attention
    pub fn from_state(state: ActivityState, asking_question: bool) -> Self {
        let color = match state {
            ActivityState::Idle => colors::IDLE,
            ActivityState::Thinking => colors::THINKING,
            ActivityState::ToolUse => colors::TOOL_USE,
            ActivityState::PlanApproval => colors::PLAN_APPROVAL,
            ActivityState::BackgroundTask => colors::BACKGROUND_TASK,
            ActivityState::AwaitingInput => colors::AWAITING_INPUT,
            ActivityState::Unknown => colors::UNKNOWN,
        };

        // Asking question: add ring + subtle diagonal stripes
        // Only applies when idle (finished responding but waiting for answer)
        let (ring, texture) = if asking_question && state == ActivityState::Idle {
            (
                Some(RingStyle {
                    color: colors::QUESTION_RING,
                    thickness: 0.12,
                    gap: 0.05,
                    pattern: RingPattern::Solid,
                }),
                DotTexture::Stripes {
                    angle: std::f64::consts::FRAC_PI_4, // 45° diagonal
                    stripe_width: 0.15,
                    gap_width: 0.15,
                    secondary: Some(Rgb::new(0.3, 0.3, 0.3)), // Darker gray stripes
                },
            )
        } else {
            (None, DotTexture::Solid)
        };

        Self {
            color,
            texture,
            ring,
            pulse: 0.0,
            scale: 1.0,
        }
    }

    /// Get base color as hex string (for Pango markup compatibility)
    pub fn color_hex(&self) -> String {
        self.color.to_hex()
    }
}

impl Default for DotStyle {
    fn default() -> Self {
        Self {
            color: colors::UNKNOWN,
            texture: DotTexture::Solid,
            ring: None,
            pulse: 0.0,
            scale: 1.0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_hex_roundtrip() {
        let original = Rgb::new(0.94, 0.25, 0.5);
        let hex = original.to_hex();
        let parsed = Rgb::from_hex(&hex);

        // Allow small rounding error
        assert!((original.r - parsed.r).abs() < 0.01);
        assert!((original.g - parsed.g).abs() < 0.01);
        assert!((original.b - parsed.b).abs() < 0.01);
    }

    #[test]
    fn test_idle_no_question() {
        let style = DotStyle::from_state(ActivityState::Idle, false);
        assert!(style.ring.is_none());
        assert!(matches!(style.texture, DotTexture::Solid));
    }

    #[test]
    fn test_idle_with_question() {
        let style = DotStyle::from_state(ActivityState::Idle, true);
        assert!(style.ring.is_some());
        assert!(matches!(style.texture, DotTexture::Stripes { .. }));
    }

    #[test]
    fn test_thinking_ignores_question() {
        // Asking question only applies when idle
        let style = DotStyle::from_state(ActivityState::Thinking, true);
        assert!(style.ring.is_none());
        assert!(matches!(style.texture, DotTexture::Solid));
    }
}
