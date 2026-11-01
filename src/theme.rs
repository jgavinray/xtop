// SPDX-License-Identifier: GPL-2.0-only
//! Charm-inspired palette and gradient helpers (Lip Gloss / Bubbles look).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

pub const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const PINK: Color = rgb(0xF25D94);
pub const PURPLE: Color = rgb(0x7D56F4);
pub const CYAN: Color = rgb(0x5FD7FF);
pub const GREEN: Color = rgb(0x04B575);
pub const YELLOW: Color = rgb(0xEDFF82);
pub const ORANGE: Color = rgb(0xFF9F43);
pub const RED: Color = rgb(0xFF4672);
pub const CREAM: Color = rgb(0xFFFDF5);
pub const TEXT: Color = rgb(0xDDDDEE);
pub const MUTED: Color = rgb(0x8A8AA3);
pub const DIM: Color = rgb(0x5C5C77);
pub const TRACK: Color = rgb(0x34344A);
pub const BORDER: Color = rgb(0x4B4B6B);
pub const BORDER_HI: Color = rgb(0x874BFD);
pub const SURFACE: Color = rgb(0x24243A);
pub const SELECT_BG: Color = rgb(0x3D2F7A);

pub struct Gradient(&'static [(f64, u32)]);

/// Bubbles' default progress gradient.
pub const UTIL: Gradient = Gradient(&[(0.0, 0x5A56E0), (1.0, 0xEE6FF8)]);
/// Cool-to-hot for temperature and power.
pub const HEAT: Gradient = Gradient(&[
    (0.0, 0x04B575),
    (0.45, 0xA8E05F),
    (0.65, 0xEDFF82),
    (0.82, 0xFF9F43),
    (1.0, 0xFF4672),
]);
pub const MEM: Gradient = Gradient(&[(0.0, 0x00C2FF), (0.6, 0x7D56F4), (1.0, 0xF25D94)]);
pub const FREQ: Gradient = Gradient(&[(0.0, 0x04B575), (1.0, 0x5FD7FF)]);

impl Gradient {
    pub fn at(&self, t: f64) -> Color {
        let t = if t.is_finite() {
            t.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let stops = self.0;
        let mut lo = stops[0];
        let mut hi = stops[stops.len() - 1];
        for w in stops.windows(2) {
            if t >= w[0].0 && t <= w[1].0 {
                lo = w[0];
                hi = w[1];
                break;
            }
        }
        let span = (hi.0 - lo.0).max(f64::EPSILON);
        let f = ((t - lo.0) / span).clamp(0.0, 1.0);
        let ch = |shift: u32| {
            let a = ((lo.1 >> shift) & 0xFF) as f64;
            let b = ((hi.1 >> shift) & 0xFF) as f64;
            (a + (b - a) * f).round() as u8
        };
        Color::Rgb(ch(16), ch(8), ch(0))
    }
}

const EIGHTHS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// A smooth horizontal bar of `width` cells, filled to `ratio`, coloured along
/// `grad` by cell position (so a full bar shows the whole gradient).
pub fn bar(width: usize, ratio: f64, grad: &Gradient) -> Vec<Span<'static>> {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let total_eighths = (ratio * width as f64 * 8.0).round() as usize;
    let full = total_eighths / 8;
    let rem = total_eighths % 8;
    let mut spans = Vec::with_capacity(width);
    for i in 0..width {
        let c = grad.at(if width > 1 {
            i as f64 / (width - 1) as f64
        } else {
            1.0
        });
        if i < full {
            spans.push(Span::styled("█", Style::new().fg(c)));
        } else if i == full && rem > 0 {
            spans.push(Span::styled(EIGHTHS[rem], Style::new().fg(c).bg(TRACK)));
        } else {
            spans.push(Span::styled("━", Style::new().fg(TRACK)));
        }
    }
    spans
}

/// Lip Gloss style "pill" title.
pub fn pill(text: impl Into<String>, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::new().fg(CREAM).bg(bg).add_modifier(Modifier::BOLD),
    )
}

pub fn temp_color(c: f64) -> Color {
    HEAT.at((c - 30.0) / 70.0)
}

pub fn pct_color(p: f64) -> Color {
    if p < 0.5 { MUTED } else { UTIL.at(p / 100.0) }
}

pub fn muted() -> Style {
    Style::new().fg(MUTED)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn text() -> Style {
    Style::new().fg(TEXT)
}

pub fn label() -> Style {
    Style::new().fg(MUTED)
}

pub fn bold(c: Color) -> Style {
    Style::new().fg(c).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_endpoints_and_clamping() {
        assert_eq!(UTIL.at(0.0), rgb(0x5A56E0));
        assert_eq!(UTIL.at(1.0), rgb(0xEE6FF8));
        assert_eq!(UTIL.at(-5.0), UTIL.at(0.0));
        assert_eq!(UTIL.at(f64::NAN), UTIL.at(0.0));
        assert_eq!(HEAT.at(2.0), rgb(0xFF4672));
    }

    #[test]
    fn bar_fills_requested_width() {
        assert_eq!(bar(10, 0.5, &UTIL).len(), 10);
        assert_eq!(
            bar(10, 1.0, &UTIL)
                .iter()
                .filter(|s| s.content == "█")
                .count(),
            10
        );
        assert_eq!(
            bar(10, 0.0, &UTIL)
                .iter()
                .filter(|s| s.content == "█")
                .count(),
            0
        );
    }
}
