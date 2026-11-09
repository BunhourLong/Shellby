//! Palette and color resolution: core colors → `Rgba` (T06 "Color resolution").

use crate::frame::Rgba;
use alacritty_terminal::term::color::{COUNT, Colors};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Theme colors. `None` slots fall back as documented per field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    /// ANSI 0-15.
    pub ansi: [Rgba; 16],
    /// Default foreground.
    pub fg: Rgba,
    /// Default background.
    pub bg: Rgba,
    /// Cursor color; `None` = the fg of the cell under the cursor.
    pub cursor: Option<Rgba>,
    /// Selection text color; `None` = inverse (T10).
    pub selection_fg: Option<Rgba>,
    /// Selection background; `None` = inverse (T10).
    pub selection_bg: Option<Rgba>,
}

impl Default for Palette {
    /// Alacritty's default dark theme.
    fn default() -> Self {
        Self {
            ansi: [
                rgb(0x18, 0x18, 0x18),
                rgb(0xac, 0x42, 0x42),
                rgb(0x90, 0xa9, 0x59),
                rgb(0xf4, 0xbf, 0x75),
                rgb(0x6a, 0x9f, 0xb5),
                rgb(0xaa, 0x75, 0x9f),
                rgb(0x75, 0xb5, 0xaa),
                rgb(0xd8, 0xd8, 0xd8),
                rgb(0x6b, 0x6b, 0x6b),
                rgb(0xc5, 0x55, 0x55),
                rgb(0xaa, 0xc4, 0x74),
                rgb(0xfe, 0xca, 0x88),
                rgb(0x82, 0xb8, 0xc8),
                rgb(0xc2, 0x8c, 0xb8),
                rgb(0x93, 0xd3, 0xc3),
                rgb(0xf8, 0xf8, 0xf8),
            ],
            fg: rgb(0xd8, 0xd8, 0xd8),
            bg: rgb(0x18, 0x18, 0x18),
            cursor: None,
            selection_fg: None,
            selection_bg: None,
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba([r, g, b, 255])
}

const fn cube(value: usize) -> u8 {
    match value {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}

const fn build_256() -> [Rgba; 256] {
    let mut table = [rgb(0, 0, 0); 256];
    let mut i = 16;
    while i < 232 {
        let n = i - 16;
        table[i] = rgb(cube(n / 36), cube(n / 6 % 6), cube(n % 6));
        i += 1;
    }
    let mut i = 232;
    while i < 256 {
        let v = 8 + 10 * (i as u8 - 232);
        table[i] = rgb(v, v, v);
        i += 1;
    }
    table
}

static TABLE_256: [Rgba; 256] = build_256();

/// xterm-256 entry `i` (callers pass `16..=255` for the cube/ramp; 0-15 come
/// from [`Palette::ansi`]).
pub(crate) fn table_256(i: usize) -> Rgba {
    TABLE_256[i.min(255)]
}

/// Alacritty's dim factor (integer 2/3).
pub(crate) fn dim(color: Rgba) -> Rgba {
    let d = |v: u8| (u16::from(v) * 2 / 3) as u8;
    Rgba([d(color.0[0]), d(color.0[1]), d(color.0[2]), color.0[3]])
}

fn as_rgb(color: Rgba) -> Rgb {
    Rgb {
        r: color.0[0],
        g: color.0[1],
        b: color.0[2],
    }
}

fn from_rgb(color: Rgb) -> Rgba {
    Rgba([color.r, color.g, color.b, 255])
}

/// Resolve a named color: dynamic `Colors` overrides win, then the palette.
pub(crate) fn named(colors: &Colors, palette: &Palette, name: NamedColor) -> Rgba {
    if let Some(color) = colors[name] {
        return from_rgb(color);
    }
    let index = name as usize;
    if index < 16 {
        return palette.ansi[index];
    }
    match name {
        NamedColor::Background => palette.bg,
        NamedColor::Cursor => palette.cursor.unwrap_or(palette.fg),
        NamedColor::DimForeground => dim(palette.fg),
        NamedColor::DimBlack
        | NamedColor::DimRed
        | NamedColor::DimGreen
        | NamedColor::DimYellow
        | NamedColor::DimBlue
        | NamedColor::DimMagenta
        | NamedColor::DimCyan
        | NamedColor::DimWhite => dim(palette.ansi[index - NamedColor::DimBlack as usize]),
        _ => palette.fg,
    }
}

/// Resolve an indexed (0-255) color.
pub(crate) fn indexed(colors: &Colors, palette: &Palette, index: u8) -> Rgba {
    if let Some(color) = colors[index as usize] {
        return from_rgb(color);
    }
    let index = index as usize;
    if index < 16 {
        palette.ansi[index]
    } else {
        table_256(index)
    }
}

fn ansi_name(index: usize) -> NamedColor {
    use NamedColor::{Black, Blue, Cyan, Green, Magenta, Red, White, Yellow};
    [Black, Red, Green, Yellow, Blue, Magenta, Cyan, White][index.min(7)]
}

fn dim_named(colors: &Colors, palette: &Palette, name: NamedColor) -> Rgba {
    if let Some(color) = colors[name.to_dim()] {
        return from_rgb(color);
    }
    dim(named(colors, palette, name))
}

fn dim_indexed(colors: &Colors, palette: &Palette, index: u8) -> Rgba {
    if index < 8
        && let Some(color) = colors[ansi_name(index as usize).to_dim()]
    {
        return from_rgb(color);
    }
    dim(indexed(colors, palette, index))
}

/// Resolve a foreground color, applying DIM (wins) or bold-as-bright.
pub(crate) fn resolve_fg(
    colors: &Colors,
    palette: &Palette,
    color: Color,
    bold: bool,
    dimmed: bool,
    bold_is_bright: bool,
) -> Rgba {
    match color {
        Color::Named(name) => {
            if dimmed {
                dim_named(colors, palette, name)
            } else if bold && bold_is_bright && (name as usize) < 8 {
                named(colors, palette, name.to_bright())
            } else {
                named(colors, palette, name)
            }
        }
        Color::Indexed(index) => {
            if dimmed {
                dim_indexed(colors, palette, index)
            } else if bold && bold_is_bright && index < 8 {
                indexed(colors, palette, index + 8)
            } else {
                indexed(colors, palette, index)
            }
        }
        Color::Spec(value) => {
            if dimmed {
                dim(from_rgb(value))
            } else {
                from_rgb(value)
            }
        }
    }
}

/// Resolve a background (or any plain) color; bold and dim never apply.
pub(crate) fn resolve_plain(colors: &Colors, palette: &Palette, color: Color) -> Rgba {
    match color {
        Color::Named(name) => named(colors, palette, name),
        Color::Indexed(index) => indexed(colors, palette, index),
        Color::Spec(value) => from_rgb(value),
    }
}

/// Resolve a cell's foreground/background, then apply INVERSE and HIDDEN.
#[expect(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "mirrors the independent SGR attributes of a core cell"
)]
pub(crate) fn resolve(
    colors: &Colors,
    palette: &Palette,
    fg: Color,
    bg: Color,
    bold: bool,
    dimmed: bool,
    inverse: bool,
    hidden: bool,
    bold_is_bright: bool,
) -> (Rgba, Rgba) {
    let mut foreground = resolve_fg(colors, palette, fg, bold, dimmed, bold_is_bright);
    let mut background = resolve_plain(colors, palette, bg);
    if inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    if hidden {
        foreground = background;
    }
    (foreground, background)
}

/// Palette value for a `ColorRequest` index, used when the core has no override.
pub(crate) fn palette_rgb(colors: &Colors, palette: &Palette, index: usize) -> Rgb {
    if index < COUNT
        && let Some(color) = colors[index]
    {
        return color;
    }
    let value = match index {
        0..=15 => palette.ansi[index],
        16..=255 => table_256(index),
        257 => palette.bg,
        258 => palette.cursor.unwrap_or(palette.fg),
        259..=266 => dim(palette.ansi[index - 259]),
        268 => dim(palette.fg),
        _ => palette.fg,
    };
    as_rgb(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors::default()
    }

    #[test]
    fn table_256_corners() {
        assert_eq!(table_256(16), rgb(0, 0, 0));
        assert_eq!(table_256(21), rgb(0, 0, 255));
        assert_eq!(table_256(196), rgb(255, 0, 0));
        assert_eq!(table_256(231), rgb(255, 255, 255));
        assert_eq!(table_256(232), rgb(8, 8, 8));
        assert_eq!(table_256(255), rgb(238, 238, 238));
    }

    #[test]
    fn bold_is_bright_only_for_ansi_0_7() {
        let palette = Palette::default();
        let colors = colors();
        let red = Color::Named(NamedColor::Red);
        assert_eq!(
            resolve_fg(&colors, &palette, red, true, false, true),
            palette.ansi[9]
        );
        assert_eq!(
            resolve_fg(&colors, &palette, red, true, false, false),
            palette.ansi[1]
        );
        let truecolor = Color::Spec(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            resolve_fg(&colors, &palette, truecolor, true, false, true),
            rgb(1, 2, 3)
        );
    }

    #[test]
    fn dim_uses_override_then_two_thirds() {
        let palette = Palette::default();
        let mut colors = colors();
        assert_eq!(
            resolve_fg(
                &colors,
                &palette,
                Color::Named(NamedColor::Red),
                false,
                true,
                true
            ),
            dim(palette.ansi[1])
        );
        colors[NamedColor::DimRed] = Some(Rgb { r: 7, g: 8, b: 9 });
        assert_eq!(
            resolve_fg(
                &colors,
                &palette,
                Color::Named(NamedColor::Red),
                false,
                true,
                true
            ),
            rgb(7, 8, 9)
        );
        assert_eq!(
            resolve_fg(&colors, &palette, Color::Indexed(196), false, true, true),
            dim(rgb(255, 0, 0))
        );
    }

    #[test]
    fn inverse_and_hidden_apply_last() {
        let palette = Palette::default();
        let colors = colors();
        let red = Color::Named(NamedColor::Red);
        let bg = Color::Named(NamedColor::Background);
        let (fg, background) = resolve(&colors, &palette, red, bg, false, false, true, false, true);
        assert_eq!((fg, background), (palette.bg, palette.ansi[1]));
        let (fg, background) = resolve(&colors, &palette, red, bg, false, false, false, true, true);
        assert_eq!(fg, background);
        assert_eq!(background, palette.bg);
    }

    #[test]
    fn core_override_wins_over_palette() {
        let palette = Palette::default();
        let mut colors = colors();
        colors[1] = Some(Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        });
        assert_eq!(
            resolve_fg(&colors, &palette, Color::Indexed(1), false, false, true),
            rgb(0x12, 0x34, 0x56)
        );
    }
}
