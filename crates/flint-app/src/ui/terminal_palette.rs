//! A shell tab's colors (#14): the 16 ANSI colors from the theme data, the xterm 256-color
//! table, and how a cell's colors and flags become what is drawn.
//!
//! gpui-component has no terminal tokens, so each theme in `themes/flint.json` carries a
//! `terminal` palette next to its `colors` (gpui-component ignores the key). Like `theme.rs`,
//! this module is where colors live (`scripts/check-theme-colors.sh` exempts it): the default
//! foreground, background, cursor and selection are the theme's tokens, colors 16 to 255 are
//! xterm's fixed table, and a program's own RGB colors are drawn as given.

use std::sync::LazyLock;

use flint_core::terminal::{Cell, CellFlags, Color, NamedColor, Rgb};
use gpui_kit::component::{Theme, ThemeMode, try_parse_color};
use gpui_kit::{Hsla, Rgba};
use serde::Deserialize;

use super::theme::THEMES;

/// How much dim (faint) text keeps of its color.
const DIM: f32 = 0.7;

/// A theme's `terminal` palette in `themes/flint.json`, in ANSI order.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ansi {
    black: String,
    red: String,
    green: String,
    yellow: String,
    blue: String,
    magenta: String,
    cyan: String,
    white: String,
    bright_black: String,
    bright_red: String,
    bright_green: String,
    bright_yellow: String,
    bright_blue: String,
    bright_magenta: String,
    bright_cyan: String,
    bright_white: String,
}

impl Ansi {
    fn colors(&self) -> [Hsla; 16] {
        [
            &self.black,
            &self.red,
            &self.green,
            &self.yellow,
            &self.blue,
            &self.magenta,
            &self.cyan,
            &self.white,
            &self.bright_black,
            &self.bright_red,
            &self.bright_green,
            &self.bright_yellow,
            &self.bright_blue,
            &self.bright_magenta,
            &self.bright_cyan,
            &self.bright_white,
        ]
        .map(|hex| try_parse_color(hex).expect("themes/flint.json: terminal colors are colors"))
    }
}

#[derive(Deserialize)]
struct FileTheme {
    mode: String,
    terminal: Ansi,
}

#[derive(Deserialize)]
struct File {
    themes: Vec<FileTheme>,
}

/// The light and dark themes' ANSI colors.
static ANSI: LazyLock<([Hsla; 16], [Hsla; 16])> = LazyLock::new(|| {
    let file: File = serde_json::from_str(THEMES).expect("themes/flint.json has terminal palettes");
    let find = |dark: bool| {
        file.themes
            .iter()
            .find(|t| (t.mode == "dark") == dark)
            .expect("themes/flint.json has a light and a dark theme")
            .terminal
            .colors()
    };
    (find(false), find(true))
});

/// The 16 ANSI colors of the theme shown in `mode`.
pub fn ansi(mode: ThemeMode) -> &'static [Hsla; 16] {
    if mode.is_dark() { &ANSI.1 } else { &ANSI.0 }
}

/// xterm's color `index` for 16 to 255: a 6×6×6 cube, then 24 grays. Below 16 the theme's ANSI
/// colors apply instead; this returns the cube's black for them.
pub fn xterm_256(index: u8) -> Rgb {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        16..=231 => {
            let i = index - 16;
            Rgb {
                r: STEPS[usize::from(i / 36)],
                g: STEPS[usize::from(i / 6 % 6)],
                b: STEPS[usize::from(i % 6)],
            }
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            Rgb { r: v, g: v, b: v }
        }
        _ => Rgb { r: 0, g: 0, b: 0 },
    }
}

fn from_rgb(rgb: Rgb) -> Hsla {
    Rgba {
        r: f32::from(rgb.r) / 255.,
        g: f32::from(rgb.g) / 255.,
        b: f32::from(rgb.b) / 255.,
        a: 1.,
    }
    .into()
}

/// The colors a shell tab draws with, for the theme shown.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalPalette {
    ansi: [Hsla; 16],
    pub foreground: Hsla,
    pub background: Hsla,
    /// The cursor (the accent, as the text fields' caret).
    pub cursor: Hsla,
    /// A character under the block cursor.
    pub cursor_text: Hsla,
    /// Behind selected cells: the theme's selection tint.
    pub selection: Hsla,
}

impl TerminalPalette {
    pub fn new(theme: &Theme) -> Self {
        Self {
            ansi: *ansi(theme.mode),
            foreground: theme.foreground,
            background: theme.background,
            cursor: theme.caret,
            cursor_text: theme.primary_foreground,
            selection: theme.selection,
        }
    }

    /// What `color` looks like in this theme.
    pub fn color(&self, color: Color) -> Hsla {
        match color {
            Color::Named(named) => self.named(named),
            Color::Indexed(index) if index < 16 => self.ansi[usize::from(index)],
            Color::Indexed(index) => from_rgb(xterm_256(index)),
            Color::Spec(rgb) => from_rgb(rgb),
        }
    }

    fn named(&self, named: NamedColor) -> Hsla {
        use NamedColor as N;
        match named {
            N::Foreground | N::BrightForeground => self.foreground,
            N::Background => self.background,
            N::Cursor => self.cursor,
            N::DimForeground => self.foreground.opacity(DIM),
            N::DimBlack
            | N::DimRed
            | N::DimGreen
            | N::DimYellow
            | N::DimBlue
            | N::DimMagenta
            | N::DimCyan
            | N::DimWhite => self.ansi[named as usize - N::DimBlack as usize].opacity(DIM),
            // Black to BrightWhite are 0 to 15.
            ansi => self.ansi[ansi as usize],
        }
    }
}

/// How one cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellStyle {
    pub fg: Hsla,
    /// `None`: the tab's background shows through.
    pub bg: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// Concealed text: only its background is drawn (a color emoji ignores a transparent
    /// foreground, so the glyph is not drawn at all).
    pub hidden: bool,
}

impl CellStyle {
    /// Draws nothing but its background: concealed, or a space without lines through or
    /// under it.
    pub fn is_blank(&self, cell: &Cell) -> bool {
        self.hidden
            || (cell.c == ' ' && cell.zerowidth.is_none() && !self.underline && !self.strikethrough)
    }
}

/// How `cell` is drawn: its colors (swapped when inverse, faded when dim, hidden text in the
/// background color) and its font style and lines.
pub fn cell_style(cell: &Cell, palette: &TerminalPalette) -> CellStyle {
    let flags = cell.flags;
    let default_bg = matches!(cell.bg, Color::Named(NamedColor::Background));
    let mut fg = palette.color(cell.fg);
    let mut bg = (!default_bg).then(|| palette.color(cell.bg));
    if flags.contains(CellFlags::INVERSE) {
        let under = bg.unwrap_or(palette.background);
        bg = Some(fg);
        fg = under;
    }
    if flags.contains(CellFlags::DIM) {
        fg = fg.opacity(DIM);
    }
    if flags.contains(CellFlags::HIDDEN) {
        fg = fg.opacity(0.);
    }
    CellStyle {
        fg,
        bg,
        bold: flags.contains(CellFlags::BOLD),
        italic: flags.contains(CellFlags::ITALIC),
        underline: flags.intersects(CellFlags::ALL_UNDERLINES),
        strikethrough: flags.contains(CellFlags::STRIKEOUT),
        hidden: flags.contains(CellFlags::HIDDEN),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::ThemeMode;

    use super::*;
    use crate::ui::accent::ACCENT;
    use crate::ui::theme::tests::{contrast, resolved};
    use crate::ui::theme::themes;

    fn palettes() -> [(TerminalPalette, Theme); 2] {
        let (light, dark) = themes(&ACCENT);
        [light, dark].map(|config| {
            let theme = resolved(config);
            (TerminalPalette::new(&theme), theme)
        })
    }

    fn cell(fg: Color, bg: Color, flags: CellFlags) -> Cell {
        Cell {
            c: 'x',
            zerowidth: None,
            fg,
            bg,
            flags,
            selected: false,
        }
    }

    #[test]
    fn both_themes_have_their_own_ansi_colors() {
        assert_ne!(ansi(ThemeMode::Light), ansi(ThemeMode::Dark));
        let [(light, light_theme), (dark, _)] = palettes();
        assert_eq!(light.foreground, light_theme.foreground);
        assert_eq!(light.selection, light_theme.selection);
        assert_ne!(
            light.color(Color::Named(NamedColor::Red)),
            dark.color(Color::Named(NamedColor::Red))
        );
    }

    // covers UP-41: `ls --color` and full-screen programs stay readable in both themes.
    #[test]
    fn ansi_text_colors_are_readable_on_the_background() {
        for (palette, theme) in palettes() {
            for (index, color) in palette.ansi.iter().enumerate() {
                // Black in the dark theme (and its dim twin) is a background color, as in every
                // dark terminal theme.
                if theme.mode.is_dark() && index == 0 {
                    continue;
                }
                let ratio = contrast(*color, palette.background, palette.background);
                assert!(
                    ratio >= 3.,
                    "{}: ANSI color {index} is {ratio:.2}:1 on the background",
                    theme.theme_name()
                );
            }
            let cursor = contrast(palette.cursor_text, palette.cursor, palette.background);
            assert!(
                cursor >= 3.,
                "{}: text under the cursor",
                theme.theme_name()
            );
        }
    }

    #[test]
    fn named_indexed_and_rgb_colors_map() {
        let [(p, _), _] = palettes();
        assert_eq!(p.color(Color::Named(NamedColor::Foreground)), p.foreground);
        assert_eq!(p.color(Color::Named(NamedColor::Background)), p.background);
        assert_eq!(p.color(Color::Named(NamedColor::Cursor)), p.cursor);
        assert_eq!(p.color(Color::Named(NamedColor::Blue)), p.ansi[4]);
        assert_eq!(p.color(Color::Named(NamedColor::BrightCyan)), p.ansi[14]);
        assert_eq!(p.color(Color::Indexed(9)), p.ansi[9]);
        assert_eq!(
            p.color(Color::Named(NamedColor::DimRed)),
            p.ansi[1].opacity(DIM)
        );
        assert_eq!(
            p.color(Color::Spec(Rgb { r: 255, g: 0, b: 0 })),
            from_rgb(Rgb { r: 255, g: 0, b: 0 })
        );
        assert_eq!(
            p.color(Color::Indexed(196)),
            from_rgb(Rgb { r: 255, g: 0, b: 0 })
        );
    }

    #[test]
    fn the_256_color_table_is_xterms() {
        assert_eq!(xterm_256(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(xterm_256(21), Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(
            xterm_256(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(
            xterm_256(208),
            Rgb {
                r: 255,
                g: 135,
                b: 0
            }
        );
        assert_eq!(xterm_256(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            xterm_256(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    // covers UP-41: bold, italic, underline and inverse text (`top`'s header, `less`'s status).
    #[test]
    fn flags_change_how_a_cell_is_drawn() {
        let [(p, _), _] = palettes();
        let fg = Color::Named(NamedColor::Foreground);
        let bg = Color::Named(NamedColor::Background);
        let plain = cell_style(&cell(fg, bg, CellFlags::empty()), &p);
        assert_eq!((plain.fg, plain.bg), (p.foreground, None));
        assert!(!plain.bold && !plain.underline);

        let inverse = cell_style(&cell(fg, bg, CellFlags::INVERSE), &p);
        assert_eq!((inverse.fg, inverse.bg), (p.background, Some(p.foreground)));
        let red_on_blue = cell(
            Color::Named(NamedColor::Red),
            Color::Named(NamedColor::Blue),
            CellFlags::INVERSE,
        );
        let swapped = cell_style(&red_on_blue, &p);
        assert_eq!((swapped.fg, swapped.bg), (p.ansi[4], Some(p.ansi[1])));

        let styled = cell_style(
            &cell(
                fg,
                bg,
                CellFlags::BOLD | CellFlags::ITALIC | CellFlags::UNDERLINE | CellFlags::STRIKEOUT,
            ),
            &p,
        );
        assert!(styled.bold && styled.italic && styled.underline && styled.strikethrough);
        assert!(cell_style(&cell(fg, bg, CellFlags::DOUBLE_UNDERLINE), &p).underline);
        assert_eq!(
            cell_style(&cell(fg, bg, CellFlags::DIM), &p).fg,
            p.foreground.opacity(DIM)
        );
        let hidden = cell(fg, bg, CellFlags::HIDDEN | CellFlags::UNDERLINE);
        assert_eq!(cell_style(&hidden, &p).fg.a, 0.);
        assert!(
            cell_style(&hidden, &p).is_blank(&hidden),
            "concealed text is not drawn"
        );

        let mut space = cell(fg, bg, CellFlags::empty());
        space.c = ' ';
        assert!(cell_style(&space, &p).is_blank(&space));
        space.flags = CellFlags::UNDERLINE;
        assert!(!cell_style(&space, &p).is_blank(&space));
    }
}
