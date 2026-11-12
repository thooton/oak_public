//! A theme: colors only. Oak's own is built in; a plugin adds others
//! (`plugin::ThemeDef`), and reads the one in use from `App::theme`.

use crate::abi::OakTheme;
use crate::egui::{Color32, color_out};

/// A theme is colors only.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub bg: Color32,
    pub panel: Color32,
    pub panel2: Color32,
    pub ink: Color32,
    pub ink2: Color32,
    pub ink3: Color32,
    /// The faintest text: the nav's key hints.
    pub ink4: Color32,
    pub accent: Color32,
    pub line: Color32,
    pub error: Color32,
    /// Behind tool calls: the text color at 4.3% over the page.
    pub tool_bg: Color32,
    /// A tool row under the pointer: the same, twice as strong.
    pub tool_hover: Color32,
}

pub fn hex(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

/// `top` laid over `under` at `alpha` opacity.
pub fn mix(under: Color32, top: Color32, alpha: f32) -> Color32 {
    let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * alpha).round() as u8;
    Color32::from_rgb(m(under.r(), top.r()), m(under.g(), top.g()), m(under.b(), top.b()))
}

/// The nine colors a theme is made from, as 0xRRGGBB, and how strong its
/// lines are.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub panel2: u32,
    pub ink: u32,
    pub ink2: u32,
    pub ink3: u32,
    pub ink4: u32,
    pub accent: u32,
    pub error: u32,
    /// `line` is the ink faded out to this alpha.
    pub line_alpha: u8,
}

impl Theme {
    /// The theme as the interface carries it.
    pub fn from_raw(t: &OakTheme) -> Theme {
        Theme {
            bg: color_out(t.bg),
            panel: color_out(t.panel),
            panel2: color_out(t.panel2),
            ink: color_out(t.ink),
            ink2: color_out(t.ink2),
            ink3: color_out(t.ink3),
            ink4: color_out(t.ink4),
            accent: color_out(t.accent),
            line: color_out(t.line),
            error: color_out(t.error),
            tool_bg: color_out(t.tool_bg),
            tool_hover: color_out(t.tool_hover),
        }
    }

    /// The theme of a palette: its colors, the line (the ink, faded), and
    /// the tool rows' backgrounds (the ink over the page).
    pub fn from_palette(p: Palette) -> Theme {
        let (bg, ink) = (hex(p.bg), hex(p.ink));
        Theme {
            bg,
            panel: hex(p.panel),
            panel2: hex(p.panel2),
            ink,
            ink2: hex(p.ink2),
            ink3: hex(p.ink3),
            ink4: hex(p.ink4),
            accent: hex(p.accent),
            line: Color32::from_rgba_unmultiplied(ink.r(), ink.g(), ink.b(), p.line_alpha),
            error: hex(p.error),
            tool_bg: mix(bg, ink, 0.043),
            tool_hover: mix(bg, ink, 0.086),
        }
    }

    /// Oak: jet black, white, oak green.
    pub fn oak() -> Theme {
        Theme::from_palette(Palette {
            bg: 0x000000,
            panel: 0x0c0c0c,
            panel2: 0x161616,
            ink: 0xffffff,
            ink2: 0xc8c8c8,
            ink3: 0x7c7c7c,
            ink4: 0x505050,
            accent: 0x6aa84f,
            error: 0xe06c5a,
            line_alpha: 0x24,
        })
    }
}
