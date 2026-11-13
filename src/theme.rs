//! Colors. Each theme comes from a color scheme whose authors published its
//! palette for others to use, and comes with that scheme's syntax
//! highlighting. A palette doesn't cover everything we draw, such as the
//! backgrounds of added and removed lines. We get those colors by mixing two
//! of the palette's colors, much as an editor draws a color at partial
//! opacity.
//!
//! Everything is 24-bit, since ghostty and most modern terminals support it.

use ratatui::style::Color;
use two_face::theme::EmbeddedThemeName as Syntax;

#[derive(Debug, Clone)]
pub struct Theme {
    /// The name to use in the config file.
    pub id: &'static str,
    pub name: &'static str,
    pub syntax: Syntax,
    pub bg: Color,
    pub panel: Color,
    pub bar: Color,
    pub fg: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub accent: Color,
    pub sel: Color,
    pub sel_unfocused: Color,
    pub hover: Color,
    pub add_bg: Color,
    pub add_emph: Color,
    pub add_fg: Color,
    pub del_bg: Color,
    pub del_emph: Color,
    pub del_fg: Color,
    pub hunk_bg: Color,
    pub file_bg: Color,
    pub line_sel: Color,
    pub button: Color,
    pub button_hover: Color,
    pub warn: Color,
    pub error: Color,
    pub ok: Color,
    /// Backgrounds for labels and colored buttons, with `badge_fg` text.
    pub badge_head: Color,
    pub badge_local: Color,
    pub badge_remote: Color,
    pub badge_tag: Color,
    pub badge_danger: Color,
    pub badge_fg: Color,
    pub tag_fg: Color,
    pub rename_fg: Color,
    pub lanes: [Color; 8],
}

/// The colors we take from a published palette. Each palette below notes
/// the names its authors gave these colors.
struct Palette {
    id: &'static str,
    name: &'static str,
    syntax: Syntax,
    bg: u32,
    /// The sidebar and dialogs.
    panel: u32,
    /// The top and bottom bars.
    bar: u32,
    /// File headers in diffs.
    surface: u32,
    /// Borders, buttons and unfocused selections.
    overlay: u32,
    /// Buttons under the mouse.
    raised: u32,
    fg: u32,
    /// Secondary text, such as authors and dates.
    dim: u32,
    /// Line numbers, placeholders and disabled items.
    faint: u32,
    accent: u32,
    red: u32,
    yellow: u32,
    green: u32,
    magenta: u32,
    /// The branch lines in the history graph.
    lanes: [u32; 8],
}

/// Mix `pct` percent of `b` into `a`.
const fn mix(a: u32, b: u32, pct: u32) -> u32 {
    const fn ch(a: u32, b: u32, pct: u32, shift: u32) -> u32 {
        let x = (a >> shift) & 0xff;
        let y = (b >> shift) & 0xff;
        ((x * (100 - pct) + y * pct + 50) / 100) << shift
    }
    ch(a, b, pct, 16) | ch(a, b, pct, 8) | ch(a, b, pct, 0)
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Relative luminance, as WCAG defines it.
fn luminance(hex: u32) -> f32 {
    let ch = |shift: u32| {
        let c = ((hex >> shift) & 0xff) as f32 / 255.0;
        if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// The WCAG contrast ratio, from 1 to 21.
fn contrast(a: u32, b: u32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// A background in the hue of `color` for `fg` text to sit on. Palette
/// accents are made to be text colors, so text on top of them is often hard
/// to read. We mix in up to `most` percent of the color, but less if the
/// text would get much harder to read than on the plain background. Some
/// palettes, such as Solarized, have little contrast to spare. We never go
/// below `least` percent, so the color still shows.
fn tint(bg: u32, fg: u32, color: u32, most: u32, least: u32) -> u32 {
    let want = (contrast(fg, bg) * 0.8).min(4.5);
    let mut pct = most;
    while pct > least && contrast(fg, mix(bg, color, pct)) < want {
        pct -= 5;
    }
    mix(bg, color, pct)
}

impl Theme {
    /// The theme to use when the config doesn't name one.
    pub const DEFAULT: &str = ONE_HALF_DARK.id;

    /// The id and name of every theme, in the order the menu shows them.
    pub fn list() -> impl Iterator<Item = (&'static str, &'static str)> {
        PALETTES.iter().map(|p| (p.id, p.name))
    }

    /// Find a theme by its id. "dark" and "light" pick the default dark and
    /// light themes.
    pub fn named(id: &str) -> Option<Theme> {
        let id = match id {
            "dark" => ONE_HALF_DARK.id,
            "light" => ONE_HALF_LIGHT.id,
            other => other,
        };
        PALETTES.iter().find(|p| p.id == id).map(Theme::build)
    }

    fn build(p: &Palette) -> Theme {
        let tint = |c, most, least| rgb(tint(p.bg, p.fg, c, most, least));
        let badge = |c| tint(c, 70, 20);
        Theme {
            id: p.id,
            name: p.name,
            syntax: p.syntax,
            bg: rgb(p.bg),
            panel: rgb(p.panel),
            bar: rgb(p.bar),
            fg: rgb(p.fg),
            dim: rgb(p.dim),
            faint: rgb(p.faint),
            border: rgb(p.overlay),
            accent: rgb(p.accent),
            sel: tint(p.accent, 30, 15),
            sel_unfocused: rgb(p.overlay),
            hover: rgb(mix(p.bg, p.surface, 60)),
            add_bg: rgb(mix(p.bg, p.green, 15)),
            add_emph: tint(p.green, 35, 25),
            add_fg: rgb(p.green),
            del_bg: rgb(mix(p.bg, p.red, 15)),
            del_emph: tint(p.red, 35, 25),
            del_fg: rgb(p.red),
            hunk_bg: rgb(mix(p.bg, p.surface, 50)),
            file_bg: rgb(p.surface),
            line_sel: tint(p.accent, 45, 20),
            button: rgb(p.overlay),
            button_hover: rgb(p.raised),
            warn: rgb(p.yellow),
            error: rgb(p.red),
            ok: rgb(p.green),
            badge_head: badge(p.accent),
            badge_local: badge(p.green),
            badge_remote: badge(p.magenta),
            badge_tag: badge(p.yellow),
            badge_danger: badge(p.red),
            badge_fg: rgb(p.fg),
            tag_fg: rgb(p.yellow),
            rename_fg: rgb(p.magenta),
            lanes: p.lanes.map(rgb),
        }
    }

    pub fn lane(&self, i: u8) -> Color {
        self.lanes[usize::from(i) % self.lanes.len()]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::build(&ONE_HALF_DARK)
    }
}

static PALETTES: [Palette; 12] = [
    CATPPUCCIN_LATTE,
    CATPPUCCIN_FRAPPE,
    CATPPUCCIN_MACCHIATO,
    CATPPUCCIN_MOCHA,
    DRACULA,
    GRUVBOX_DARK,
    GRUVBOX_LIGHT,
    NORD,
    ONE_HALF_DARK,
    ONE_HALF_LIGHT,
    SOLARIZED_DARK,
    SOLARIZED_LIGHT,
];

// Catppuccin, https://catppuccin.com/palette. The four flavors share their
// color names, which the first one lists.

const CATPPUCCIN_LATTE: Palette = Palette {
    id: "catppuccin-latte",
    name: "Catppuccin Latte",
    syntax: Syntax::CatppuccinLatte,
    bg: 0xeff1f5,      // base
    panel: 0xe6e9ef,   // mantle
    bar: 0xdce0e8,     // crust
    surface: 0xccd0da, // surface0
    overlay: 0xbcc0cc, // surface1
    raised: 0xacb0be,  // surface2
    fg: 0x4c4f69,      // text
    dim: 0x6c6f85,     // subtext0
    faint: 0x9ca0b0,   // overlay0
    accent: 0x1e66f5,  // blue
    red: 0xd20f39,     // red
    yellow: 0xdf8e1d,  // yellow
    green: 0x40a02b,   // green
    magenta: 0x8839ef, // mauve
    // blue, peach, green, mauve, red, teal, yellow, pink
    lanes: [0x1e66f5, 0xfe640b, 0x40a02b, 0x8839ef, 0xd20f39, 0x179299, 0xdf8e1d, 0xea76cb],
};

const CATPPUCCIN_FRAPPE: Palette = Palette {
    id: "catppuccin-frappe",
    name: "Catppuccin Frappé",
    syntax: Syntax::CatppuccinFrappe,
    bg: 0x303446,
    panel: 0x292c3c,
    bar: 0x232634,
    surface: 0x414559,
    overlay: 0x51576d,
    raised: 0x626880,
    fg: 0xc6d0f5,
    dim: 0xa5adce,
    faint: 0x737994,
    accent: 0x8caaee,
    red: 0xe78284,
    yellow: 0xe5c890,
    green: 0xa6d189,
    magenta: 0xca9ee6,
    lanes: [0x8caaee, 0xef9f76, 0xa6d189, 0xca9ee6, 0xe78284, 0x81c8be, 0xe5c890, 0xf4b8e4],
};

const CATPPUCCIN_MACCHIATO: Palette = Palette {
    id: "catppuccin-macchiato",
    name: "Catppuccin Macchiato",
    syntax: Syntax::CatppuccinMacchiato,
    bg: 0x24273a,
    panel: 0x1e2030,
    bar: 0x181926,
    surface: 0x363a4f,
    overlay: 0x494d64,
    raised: 0x5b6078,
    fg: 0xcad3f5,
    dim: 0xa5adcb,
    faint: 0x6e738d,
    accent: 0x8aadf4,
    red: 0xed8796,
    yellow: 0xeed49f,
    green: 0xa6da95,
    magenta: 0xc6a0f6,
    lanes: [0x8aadf4, 0xf5a97f, 0xa6da95, 0xc6a0f6, 0xed8796, 0x8bd5ca, 0xeed49f, 0xf5bde6],
};

const CATPPUCCIN_MOCHA: Palette = Palette {
    id: "catppuccin-mocha",
    name: "Catppuccin Mocha",
    syntax: Syntax::CatppuccinMocha,
    bg: 0x1e1e2e,
    panel: 0x181825,
    bar: 0x11111b,
    surface: 0x313244,
    overlay: 0x45475a,
    raised: 0x585b70,
    fg: 0xcdd6f4,
    dim: 0xa6adc8,
    faint: 0x6c7086,
    accent: 0x89b4fa,
    red: 0xf38ba8,
    yellow: 0xf9e2af,
    green: 0xa6e3a1,
    magenta: 0xcba6f7,
    lanes: [0x89b4fa, 0xfab387, 0xa6e3a1, 0xcba6f7, 0xf38ba8, 0x94e2d5, 0xf9e2af, 0xf5c2e7],
};

// Dracula, https://draculatheme.com/spec. It has no blue, so its purple is
// the accent and its pink stands in for magenta.
const DRACULA: Palette = Palette {
    id: "dracula",
    name: "Dracula",
    syntax: Syntax::Dracula,
    bg: 0x282a36,                     // background
    panel: 0x21222c,                  // background dark
    bar: 0x191a21,                    // background darker
    surface: 0x343746,                // background light
    overlay: 0x424450,                // background lighter
    raised: 0x44475a,                 // current line
    fg: 0xf8f8f2,                     // foreground
    dim: mix(0xf8f8f2, 0x6272a4, 50), // between foreground and comment
    faint: 0x6272a4,                  // comment
    accent: 0xbd93f9,                 // purple
    red: 0xff5555,                    // red
    yellow: 0xf1fa8c,                 // yellow
    green: 0x50fa7b,                  // green
    magenta: 0xff79c6,                // pink
    // purple, orange, green, pink, red, cyan, yellow, comment
    lanes: [0xbd93f9, 0xffb86c, 0x50fa7b, 0xff79c6, 0xff5555, 0x8be9fd, 0xf1fa8c, 0x6272a4],
};

// Gruvbox, https://github.com/morhetz/gruvbox. The dark theme uses the
// bright accents and the light theme the faded ones, as the original does.
const GRUVBOX_DARK: Palette = Palette {
    id: "gruvbox-dark",
    name: "Gruvbox Dark",
    syntax: Syntax::GruvboxDark,
    bg: 0x282828,      // dark0
    panel: 0x1d2021,   // dark0_hard
    bar: 0x3c3836,     // dark1
    surface: 0x3c3836, // dark1
    overlay: 0x504945, // dark2
    raised: 0x665c54,  // dark3
    fg: 0xebdbb2,      // light1
    dim: 0xa89984,     // light4
    faint: 0x7c6f64,   // dark4
    accent: 0x83a598,  // bright_blue
    red: 0xfb4934,     // bright_red
    yellow: 0xfabd2f,  // bright_yellow
    green: 0xb8bb26,   // bright_green
    magenta: 0xd3869b, // bright_purple
    // blue, orange, green, purple, red, aqua, yellow, gray
    lanes: [0x83a598, 0xfe8019, 0xb8bb26, 0xd3869b, 0xfb4934, 0x8ec07c, 0xfabd2f, 0x928374],
};

const GRUVBOX_LIGHT: Palette = Palette {
    id: "gruvbox-light",
    name: "Gruvbox Light",
    syntax: Syntax::GruvboxLight,
    bg: 0xfbf1c7,      // light0
    panel: 0xf2e5bc,   // light0_soft
    bar: 0xebdbb2,     // light1
    surface: 0xebdbb2, // light1
    overlay: 0xd5c4a1, // light2
    raised: 0xbdae93,  // light3
    fg: 0x3c3836,      // dark1
    dim: 0x7c6f64,     // dark4
    faint: 0xa89984,   // light4
    accent: 0x076678,  // faded_blue
    red: 0x9d0006,     // faded_red
    yellow: 0xb57614,  // faded_yellow
    green: 0x79740e,   // faded_green
    magenta: 0x8f3f71, // faded_purple
    lanes: [0x076678, 0xaf3a03, 0x79740e, 0x8f3f71, 0x9d0006, 0x427b58, 0xb57614, 0x928374],
};

// Nord, https://www.nordtheme.com/docs/colors-and-palettes. Its comment
// color, nord3, is also a background, and text in it can't be read on the
// panels. So the gray text colors are mixes of nord3 and nord4.
const NORD: Palette = Palette {
    id: "nord",
    name: "Nord",
    syntax: Syntax::Nord,
    bg: 0x2e3440,                     // nord0
    panel: 0x3b4252,                  // nord1
    bar: 0x3b4252,                    // nord1
    surface: 0x3b4252,                // nord1
    overlay: 0x434c5e,                // nord2
    raised: 0x4c566a,                 // nord3
    fg: 0xd8dee9,                     // nord4
    dim: mix(0xd8dee9, 0x4c566a, 35), // nord4 and nord3
    faint: mix(0xd8dee9, 0x4c566a, 65),
    accent: 0x88c0d0,                 // nord8
    red: 0xbf616a,                    // nord11
    yellow: 0xebcb8b,                 // nord13
    green: 0xa3be8c,                  // nord14
    magenta: 0xb48ead,                // nord15
    // nord8, 12, 14, 15, 11, 7, 13, 10
    lanes: [0x88c0d0, 0xd08770, 0xa3be8c, 0xb48ead, 0xbf616a, 0x8fbcbb, 0xebcb8b, 0x5e81ac],
};

// One Half, https://github.com/sonph/onehalf. It has no orange, so the
// graph uses its grays for the last lanes.
const ONE_HALF_DARK: Palette = Palette {
    id: "one-half-dark",
    name: "One Half Dark",
    syntax: Syntax::OneHalfDark,
    bg: 0x282c34,      // black
    panel: 0x313640,   // cursor_line
    bar: 0x313640,     // cursor_line
    surface: 0x313640, // cursor_line
    overlay: 0x474e5d, // selection
    raised: 0x5c6370,  // comment_fg
    fg: 0xdcdfe4,      // white
    dim: 0x919baa,     // gutter_fg
    faint: 0x5c6370,   // comment_fg
    accent: 0x61afef,  // blue
    red: 0xe06c75,     // red
    yellow: 0xe5c07b,  // yellow
    green: 0x98c379,   // green
    magenta: 0xc678dd, // purple
    // blue, yellow, green, purple, red, cyan, gutter_fg, white
    lanes: [0x61afef, 0xe5c07b, 0x98c379, 0xc678dd, 0xe06c75, 0x56b6c2, 0x919baa, 0xdcdfe4],
};

const ONE_HALF_LIGHT: Palette = Palette {
    id: "one-half-light",
    name: "One Half Light",
    syntax: Syntax::OneHalfLight,
    bg: 0xfafafa,                     // white
    panel: 0xf0f0f0,                  // cursor_line
    bar: 0xf0f0f0,                    // cursor_line
    surface: 0xf0f0f0,                // cursor_line
    overlay: 0xd4d4d4,                // gutter_fg
    raised: 0xbfceff,                 // selection
    fg: 0x383a42,                     // black
    dim: mix(0x383a42, 0xa0a1a7, 50), // between black and comment_fg
    faint: 0xa0a1a7,                  // comment_fg
    accent: 0x0184bc,                 // blue
    red: 0xe45649,                    // red
    yellow: 0xc18401,                 // yellow
    green: 0x50a14f,                  // green
    magenta: 0xa626a4,                // purple
    // blue, yellow, green, purple, red, cyan, comment_fg, black
    lanes: [0x0184bc, 0xc18401, 0x50a14f, 0xa626a4, 0xe45649, 0x0997b3, 0xa0a1a7, 0x383a42],
};

// Solarized, https://ethanschoonover.com/solarized. It has only two
// background shades for each mode, so buttons and borders are mixes of the
// second background and the secondary text color. The accents are the same
// in both modes.
const SOLARIZED_DARK: Palette = Palette {
    id: "solarized-dark",
    name: "Solarized Dark",
    syntax: Syntax::SolarizedDark,
    bg: 0x002b36,                         // base03
    panel: 0x073642,                      // base02
    bar: 0x073642,                        // base02
    surface: 0x073642,                    // base02
    overlay: mix(0x073642, 0x586e75, 35), // base02 and base01
    raised: mix(0x073642, 0x586e75, 60),
    fg: 0x839496,                         // base0
    dim: 0x586e75,                        // base01
    faint: mix(0x586e75, 0x073642, 35),
    accent: 0x268bd2,                     // blue
    red: 0xdc322f,                        // red
    yellow: 0xb58900,                     // yellow
    green: 0x859900,                      // green
    magenta: 0xd33682,                    // magenta
    // blue, orange, green, violet, red, cyan, yellow, magenta
    lanes: [0x268bd2, 0xcb4b16, 0x859900, 0x6c71c4, 0xdc322f, 0x2aa198, 0xb58900, 0xd33682],
};

const SOLARIZED_LIGHT: Palette = Palette {
    id: "solarized-light",
    name: "Solarized Light",
    syntax: Syntax::SolarizedLight,
    bg: 0xfdf6e3,                         // base3
    panel: 0xeee8d5,                      // base2
    bar: 0xeee8d5,                        // base2
    surface: 0xeee8d5,                    // base2
    overlay: mix(0xeee8d5, 0x93a1a1, 35), // base2 and base1
    raised: mix(0xeee8d5, 0x93a1a1, 60),
    fg: 0x657b83,                         // base00
    dim: 0x93a1a1,                        // base1
    faint: mix(0x93a1a1, 0xeee8d5, 35),
    ..SOLARIZED_DARK
};

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: Color) -> u32 {
        match c {
            Color::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
            other => panic!("not an RGB color: {other:?}"),
        }
    }

    #[test]
    fn mixes_colors() {
        assert_eq!(mix(0x000000, 0xffffff, 0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 100), 0xffffff);
        assert_eq!(mix(0x000000, 0xc864fa, 50), 0x64327d);
    }

    #[test]
    fn every_theme_can_be_found_by_its_id() {
        let ids: Vec<&str> = Theme::list().map(|(id, _)| id).collect();
        for id in &ids {
            assert_eq!(Theme::named(id).map(|t| t.id), Some(*id));
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        assert_eq!(Theme::named("dark").map(|t| t.id), Some(Theme::DEFAULT));
        assert!(Theme::named("light").is_some());
        assert!(Theme::named("sublime").is_none());
    }

    /// Published palettes don't all meet the usual contrast guidelines, and
    /// we keep their colors anyway. This only checks that the backgrounds we
    /// mix keep most of the contrast the text has on the plain background.
    #[test]
    fn text_stays_readable_on_mixed_backgrounds() {
        for (id, _) in Theme::list() {
            let Some(t) = Theme::named(id) else { continue };
            let fg = hex(t.fg);
            let base = contrast(fg, hex(t.bg));
            for (what, bg) in [
                ("sel", t.sel),
                ("line_sel", t.line_sel),
                ("add_bg", t.add_bg),
                ("add_emph", t.add_emph),
                ("del_bg", t.del_bg),
                ("del_emph", t.del_emph),
                ("hover", t.hover),
                ("button", t.button),
            ] {
                let c = contrast(fg, hex(bg));
                assert!(c >= 3.0 || c >= base * 0.65, "{id}: text on {what} has contrast {c:.2}");
            }
            for (what, bg) in [
                ("head", t.badge_head),
                ("local", t.badge_local),
                ("remote", t.badge_remote),
                ("tag", t.badge_tag),
                ("danger", t.badge_danger),
            ] {
                let c = contrast(hex(t.badge_fg), hex(bg));
                assert!(c >= 3.0, "{id}: text on the {what} badge has contrast {c:.2}");
            }
        }
    }
}
