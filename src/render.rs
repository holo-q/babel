//! Cairo rendering for indicator dots — shared between panel widgets
//!
//! This module provides the canonical rendering implementation for Claude session
//! indicator dots. Both richspace and richmon panels can use these functions
//! to render consistent visuals.
//!
//! ## Rendering Layers
//!
//! Each dot is rendered with up to four layers (back to front):
//! 1. **Ring glow** - Animated aura during token output (ring_intensity > 0)
//! 2. **Outline** - Static border for question state (if DotStyle.outline is set)
//! 3. **Main dot** - Solid or textured fill (DotTexture::Solid, Stripes, Concentric)
//! 4. **Highlight** - Top-left shine for 3D effect
//!
//! ## Usage
//!
//! ```rust,ignore
//! use claude_babel::render::render_dot;
//! use spaceship_std::visual::{DotStyle, Rgb};
//!
//! // In a GTK DrawingArea's connect_draw:
//! drawing_area.connect_draw(|_, ctx| {
//!     let style = DotStyle {
//!         color: Rgb::new(0.94, 0.75, 0.25), // gold
//!         ring_intensity: 0.5,
//!         ..Default::default()
//!     };
//!     render_dot(ctx, 10.0, 10.0, 4.0, &style);
//!     glib::Propagation::Proceed
//! });
//! ```

use gtk::cairo;
use spaceship_std::visual::{DotStyle, DotTexture, OutlinePattern, OutlineStyle, Rgb};
use std::f64::consts::TAU;

/// Render a complete dot with optional ring, outline, and texture
///
/// # Arguments
/// - `ctx`: Cairo context to draw into (already translated to correct position)
/// - `x`, `y`: Center position of the dot in pixels
/// - `radius`: Base radius of the dot in pixels
/// - `style`: Visual style including color, ring_intensity, outline, texture
///
/// # Rendering Order
/// 1. Ring glow (if ring_intensity > 0.01)
/// 2. Outline border (if outline is Some)
/// 3. Main dot body (solid or textured)
/// 4. Highlight shine
pub fn render_dot(ctx: &cairo::Context, x: f64, y: f64, radius: f64, style: &DotStyle) {
    // Apply scale factor
    let radius = radius * style.scale;

    // 1. Ring glow (animated aura during token output)
    if style.ring_intensity > 0.01 {
        render_ring_glow(ctx, x, y, radius, &style.color, style.ring_intensity);
    }

    // 2. Outline border (static, e.g. for question state)
    if let Some(ref outline) = style.outline {
        render_outline(ctx, x, y, radius, outline);
    }

    // 3. Main dot body
    match &style.texture {
        DotTexture::Solid => {
            render_solid_dot(ctx, x, y, radius, &style.color);
        }
        DotTexture::Stripes {
            angle,
            stripe_width,
            gap_width,
            secondary,
        } => {
            render_striped_dot(
                ctx,
                x,
                y,
                radius,
                &style.color,
                *angle,
                *stripe_width,
                *gap_width,
                secondary.as_ref(),
            );
        }
        DotTexture::Concentric {
            ring_count,
            secondary,
        } => {
            render_concentric_dot(ctx, x, y, radius, &style.color, *ring_count, secondary);
        }
    }

    // 4. Highlight shine (top-left)
    render_highlight(ctx, x, y, radius);
}

/// Render the animated ring glow effect
///
/// The ring expands outward from the dot and fades with intensity.
/// Used during token output to show activity.
fn render_ring_glow(ctx: &cairo::Context, x: f64, y: f64, radius: f64, color: &Rgb, intensity: f64) {
    let ring_radius = radius * (1.0 + intensity * 0.5);
    let alpha = intensity * 0.4;

    ctx.set_source_rgba(color.r, color.g, color.b, alpha);
    ctx.arc(x, y, ring_radius, 0.0, TAU);
    let _ = ctx.fill();
}

/// Render the outline border around the dot
///
/// Static decoration for secondary state (e.g., asking question).
fn render_outline(ctx: &cairo::Context, x: f64, y: f64, radius: f64, outline: &OutlineStyle) {
    let outline_radius = radius + outline.gap + (outline.thickness * radius / 2.0);
    let line_width = outline.thickness * radius;

    ctx.set_source_rgb(outline.color.r, outline.color.g, outline.color.b);
    ctx.set_line_width(line_width);

    match &outline.pattern {
        OutlinePattern::Solid => {
            ctx.arc(x, y, outline_radius, 0.0, TAU);
            let _ = ctx.stroke();
        }
        OutlinePattern::Dashed { dash, gap } => {
            // Dash pattern as fraction of circumference
            let circumference = TAU * outline_radius;
            let dash_len = dash * circumference;
            let gap_len = gap * circumference;
            ctx.set_dash(&[dash_len, gap_len], 0.0);
            ctx.arc(x, y, outline_radius, 0.0, TAU);
            let _ = ctx.stroke();
            ctx.set_dash(&[], 0.0); // Reset dash pattern
        }
        OutlinePattern::Pulsing => {
            // Pulsing outline handled by caller adjusting alpha over time
            // For now, render as solid
            ctx.arc(x, y, outline_radius, 0.0, TAU);
            let _ = ctx.stroke();
        }
    }
}

/// Render a solid-filled dot
fn render_solid_dot(ctx: &cairo::Context, x: f64, y: f64, radius: f64, color: &Rgb) {
    ctx.set_source_rgb(color.r, color.g, color.b);
    ctx.arc(x, y, radius, 0.0, TAU);
    let _ = ctx.fill();
}

/// Render a striped dot
///
/// Stripes are drawn at the specified angle across the dot.
fn render_striped_dot(
    ctx: &cairo::Context,
    x: f64,
    y: f64,
    radius: f64,
    primary: &Rgb,
    angle: f64,
    stripe_width: f64,
    gap_width: f64,
    secondary: Option<&Rgb>,
) {
    // Create a clipping region for the dot
    ctx.arc(x, y, radius, 0.0, TAU);
    ctx.save().ok();
    ctx.clip();

    // Fill background with secondary color (or transparent)
    if let Some(sec) = secondary {
        ctx.set_source_rgb(sec.r, sec.g, sec.b);
        ctx.paint().ok();
    }

    // Draw stripes
    ctx.set_source_rgb(primary.r, primary.g, primary.b);

    // Calculate stripe parameters
    let stripe_w = stripe_width * radius;
    let gap_w = gap_width * radius;
    let period = stripe_w + gap_w;

    // Rotate around center
    ctx.translate(x, y);
    ctx.rotate(angle);
    ctx.translate(-x, -y);

    // Draw parallel stripes across the circle
    let extent = radius * 2.0;
    let mut offset = -extent;
    while offset < extent {
        ctx.rectangle(x - extent, y + offset, extent * 2.0, stripe_w);
        let _ = ctx.fill();
        offset += period;
    }

    ctx.restore().ok();
}

/// Render a dot with concentric rings (bullseye pattern)
fn render_concentric_dot(
    ctx: &cairo::Context,
    x: f64,
    y: f64,
    radius: f64,
    primary: &Rgb,
    ring_count: u8,
    secondary: &Rgb,
) {
    let ring_width = radius / (ring_count as f64);

    for i in 0..ring_count {
        let r = radius - (i as f64 * ring_width);
        let color = if i % 2 == 0 { primary } else { secondary };
        ctx.set_source_rgb(color.r, color.g, color.b);
        ctx.arc(x, y, r, 0.0, TAU);
        let _ = ctx.fill();
    }
}

/// Render the highlight shine on top-left
///
/// Adds a subtle 3D effect with a white highlight.
fn render_highlight(ctx: &cairo::Context, x: f64, y: f64, radius: f64) {
    let highlight_offset = radius * 0.3;
    let highlight_radius = radius * 0.2;

    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.3);
    ctx.arc(
        x - highlight_offset,
        y - highlight_offset,
        highlight_radius,
        0.0,
        TAU,
    );
    let _ = ctx.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_style_default() {
        let style = DotStyle::default();
        assert_eq!(style.ring_intensity, 0.0);
        assert!(style.outline.is_none());
        assert!(matches!(style.texture, DotTexture::Solid));
    }
}
