//! Colors. Each theme comes from a color scheme whose authors published its
//! palette for others to use, along with colors for syntax highlighting
//! that follow the scheme's own. A palette doesn't cover everything we draw, such as the
//! backgrounds of added and removed lines. We get those colors by mixing two
//! of the palette's colors, much as an editor draws a color at partial
//! opacity.
//!
//! Everything is 24-bit, since ghostty and most modern terminals support it.

use ratatui::style::Color;

use crate::highlight::Kind;

#[derive(Debug, Clone)]
pub struct Theme {
    /// The name to use in the config file.
    pub id: &'static str,
    pub name: &'static str,
    pub syntax: &'static Syntax,
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
    /// Section, file and hunk headers in the detail pane under the mouse.
    pub bar_hover: Color,
    pub file_hover: Color,
    pub hunk_hover: Color,
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
    /// The colors that identifiers get when the user turns that on. Each
    /// name always gets the same one (see `SyntaxColors`).
    pub identifiers: [Color; IDENTIFIER_COLORS],
}

/// How many colors identifiers can get. With more, two names are less
/// likely to share a color, but colors next to each other are harder to
/// tell apart.
const IDENTIFIER_COLORS: usize = 16;

/// Colors for syntax highlighting, one for each kind of token. Tokens
/// without a color, and plain text, are drawn in the theme's `fg`.
///
/// The colors come from each scheme's TextMate theme, the one bat and
/// Sublime Text use. Each kind gets the color of the scope that comes
/// closest, such as `constant.character.escape` for escapes.
#[derive(Debug)]
pub struct Syntax {
    pub comment: u32,
    pub string: u32,
    pub escape: u32,
    pub number: u32,
    pub constant: u32,
    pub keyword: u32,
    pub ty: u32,
    /// The color of `entity.name.function`. No token gets it, but the
    /// colors for identifiers take their lightness and chroma partly
    /// from it.
    pub function: u32,
    pub attribute: u32,
    pub lifetime: u32,
    pub operator: Option<u32>,
    pub punctuation: Option<u32>,
}

impl Syntax {
    /// The colors that tokens get.
    fn drawn(&self) -> impl Iterator<Item = u32> + '_ {
        [
            self.comment,
            self.string,
            self.escape,
            self.number,
            self.constant,
            self.keyword,
            self.ty,
            self.attribute,
            self.lifetime,
        ]
        .into_iter()
        .chain(self.operator)
        .chain(self.punctuation)
    }

    pub fn color(&self, kind: Kind) -> Option<Color> {
        let hex = match kind {
            Kind::Comment => self.comment,
            Kind::String => self.string,
            Kind::Escape => self.escape,
            Kind::Number => self.number,
            Kind::Constant => self.constant,
            Kind::Keyword => self.keyword,
            Kind::Type => self.ty,
            Kind::Attribute => self.attribute,
            Kind::Lifetime => self.lifetime,
            Kind::Operator => self.operator?,
            Kind::Punctuation => self.punctuation?,
            Kind::Identifier => return None,
        };
        Some(rgb(hex))
    }
}

/// The colors to draw code in. Identifiers get colors only when
/// `identifiers` is set.
#[derive(Clone, Copy)]
pub struct SyntaxColors<'a> {
    syntax: &'a Syntax,
    identifiers: Option<&'a [Color]>,
}

impl SyntaxColors<'_> {
    /// The color of a token of the given kind, whose text is `text`.
    pub fn color(&self, kind: Kind, text: &str) -> Option<Color> {
        match (kind, self.identifiers) {
            (Kind::Identifier, Some(colors)) => {
                let i = name_hash(text) % colors.len() as u64;
                colors.get(i as usize).copied()
            }
            _ => self.syntax.color(kind),
        }
    }
}

/// A hash of a name that stays the same from one run to the next, so
/// that a name keeps its color. FNV-1a alone leaves the low bits of the
/// hash depending only on the low bits of each byte, so we mix the bits
/// afterwards, as MurmurHash3 does.
fn name_hash(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ (h >> 33)
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
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// A color in Oklab, as lightness and two axes of hue. Distances in Oklab
/// match how different colors look much better than distances in sRGB.
/// See https://bottosson.github.io/posts/oklab/.
fn oklab(hex: u32) -> [f32; 3] {
    let ch = |shift: u32| {
        let c = ((hex >> shift) & 0xff) as f32 / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (ch(16), ch(8), ch(0));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// The sRGB color of a color in Oklab, or none if sRGB can't show it.
fn from_oklab([lightness, a, b]: [f32; 3]) -> Option<u32> {
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    let linear = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let mut hex = 0;
    for c in linear {
        // Leave a little room for rounding errors.
        if !(-0.001..=1.001).contains(&c) {
            return None;
        }
        let c = c.clamp(0.0, 1.0);
        let c = if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        hex = hex << 8 | (c * 255.0).round() as u32;
    }
    Some(hex)
}

/// Colors for identifiers. A name's color should stand out from the
/// colors of keywords, strings and other tokens, from the text color,
/// and from the colors of other names.
///
/// We start from the lightness and chroma of the theme's own syntax
/// colors, so that the colors look like they belong to it. The chroma
/// is at most 80% of theirs, so that identifiers stand out less than
/// keywords and strings do. But it is at least 0.08, or the hues would
/// be hard to tell apart in muted themes such as Nord. Around that, we
/// try many hues, lightnesses and chromas. Then we pick colors one at a
/// time, each time taking the one farthest from the syntax colors, the
/// text color and the colors picked so far.
///
/// Identifiers are most of the text in code. So a color that is hard to
/// read on the background moves toward the lightness of the text until
/// it is about as easy to read.
fn identifier_colors(p: &Palette) -> [u32; IDENTIFIER_COLORS] {
    let s = &p.syntax;
    let samples = [s.string, s.number, s.keyword, s.ty, s.function].map(oklab);
    let n = samples.len() as f32;
    let lightness = samples.iter().map(|c| c[0]).sum::<f32>() / n;
    let chroma = (samples.iter().map(|c| c[1].hypot(c[2])).sum::<f32>() / n * 0.8).max(0.08);
    let text = oklab(p.fg)[0];
    let want = (contrast(p.fg, p.bg) * 0.8).min(4.5);
    let readable = |mut l: f32, c: f32, hue: f32| loop {
        let color = from_oklab([l, c * hue.cos(), c * hue.sin()]);
        if let Some(color) = color.filter(|&color| contrast(color, p.bg) >= want) {
            return Some(color);
        }
        if (l - text).abs() < 0.01 {
            return None;
        }
        l += (text - l).clamp(-0.01, 0.01);
    };

    let mut candidates = Vec::new();
    for step in -4..=4 {
        for tenths in 4..=10 {
            for i in 0..72 {
                let l = lightness + 0.02 * step as f32;
                let c = chroma * tenths as f32 / 10.0;
                let hue = std::f32::consts::TAU * i as f32 / 72.0;
                if let Some(color) = readable(l, c, hue) {
                    candidates.push((color, oklab(color)));
                }
            }
        }
    }
    let avoid: Vec<[f32; 3]> = s.drawn().chain([p.fg]).map(oklab).collect();
    // How far each candidate is from the nearest color to avoid.
    let mut room: Vec<f32> = candidates
        .iter()
        .map(|&(_, c)| {
            avoid
                .iter()
                .map(|&a| distance(a, c))
                .fold(f32::INFINITY, f32::min)
        })
        .collect();
    std::array::from_fn(|_| {
        let Some(best) = (0..room.len()).max_by(|&i, &j| room[i].total_cmp(&room[j])) else {
            return p.fg;
        };
        let (color, picked) = candidates[best];
        for (r, &(_, c)) in room.iter_mut().zip(&candidates) {
            *r = r.min(distance(picked, c));
        }
        color
    })
}

/// How different two colors in Oklab look.
fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
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

    fn build(p: &'static Palette) -> Theme {
        let tint = |c, most, least| rgb(tint(p.bg, p.fg, c, most, least));
        let badge = |c| tint(c, 70, 20);
        Theme {
            id: p.id,
            name: p.name,
            syntax: &p.syntax,
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
            bar_hover: rgb(mix(p.bar, p.fg, 5)),
            file_hover: rgb(mix(p.surface, p.fg, 5)),
            hunk_hover: rgb(mix(mix(p.bg, p.surface, 50), p.fg, 5)),
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
            identifiers: identifier_colors(p).map(rgb),
        }
    }

    /// The colors to draw code in, with colors for identifiers if
    /// `identifiers` is true.
    pub fn syntax_colors(&self, identifiers: bool) -> SyntaxColors<'_> {
        SyntaxColors {
            syntax: self.syntax,
            identifiers: identifiers.then_some(&self.identifiers[..]),
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
    syntax: Syntax {
        comment: 0x7c7f93,
        string: 0x40a02b,
        escape: 0xea76cb,
        number: 0xfe640b,
        constant: 0xfe640b,
        keyword: 0x8839ef,
        ty: 0xdf8e1d,
        function: 0x1e66f5,
        attribute: 0xdf8e1d,
        lifetime: 0x179299,
        operator: Some(0x179299),
        punctuation: Some(0x7c7f93),
    },
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
    lanes: [
        0x1e66f5, 0xfe640b, 0x40a02b, 0x8839ef, 0xd20f39, 0x179299, 0xdf8e1d, 0xea76cb,
    ],
};

const CATPPUCCIN_FRAPPE: Palette = Palette {
    id: "catppuccin-frappe",
    name: "Catppuccin Frappé",
    syntax: Syntax {
        comment: 0x949cbb,
        string: 0xa6d189,
        escape: 0xf4b8e4,
        number: 0xef9f76,
        constant: 0xef9f76,
        keyword: 0xca9ee6,
        ty: 0xe5c890,
        function: 0x8caaee,
        attribute: 0xe5c890,
        lifetime: 0x81c8be,
        operator: Some(0x81c8be),
        punctuation: Some(0x949cbb),
    },
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
    lanes: [
        0x8caaee, 0xef9f76, 0xa6d189, 0xca9ee6, 0xe78284, 0x81c8be, 0xe5c890, 0xf4b8e4,
    ],
};

const CATPPUCCIN_MACCHIATO: Palette = Palette {
    id: "catppuccin-macchiato",
    name: "Catppuccin Macchiato",
    syntax: Syntax {
        comment: 0x939ab7,
        string: 0xa6da95,
        escape: 0xf5bde6,
        number: 0xf5a97f,
        constant: 0xf5a97f,
        keyword: 0xc6a0f6,
        ty: 0xeed49f,
        function: 0x8aadf4,
        attribute: 0xeed49f,
        lifetime: 0x8bd5ca,
        operator: Some(0x8bd5ca),
        punctuation: Some(0x939ab7),
    },
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
    lanes: [
        0x8aadf4, 0xf5a97f, 0xa6da95, 0xc6a0f6, 0xed8796, 0x8bd5ca, 0xeed49f, 0xf5bde6,
    ],
};

const CATPPUCCIN_MOCHA: Palette = Palette {
    id: "catppuccin-mocha",
    name: "Catppuccin Mocha",
    syntax: Syntax {
        comment: 0x9399b2,
        string: 0xa6e3a1,
        escape: 0xf5c2e7,
        number: 0xfab387,
        constant: 0xfab387,
        keyword: 0xcba6f7,
        ty: 0xf9e2af,
        function: 0x89b4fa,
        attribute: 0xf9e2af,
        lifetime: 0x94e2d5,
        operator: Some(0x94e2d5),
        punctuation: Some(0x9399b2),
    },
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
    lanes: [
        0x89b4fa, 0xfab387, 0xa6e3a1, 0xcba6f7, 0xf38ba8, 0x94e2d5, 0xf9e2af, 0xf5c2e7,
    ],
};

// Dracula, https://draculatheme.com/spec. It has no blue, so its purple is
// the accent and its pink stands in for magenta.
const DRACULA: Palette = Palette {
    id: "dracula",
    name: "Dracula",
    syntax: Syntax {
        comment: 0x6272a4,
        string: 0xf1fa8c,
        escape: 0xff79c6,
        number: 0xbd93f9,
        constant: 0xbd93f9,
        keyword: 0xff79c6,
        ty: 0x8be9fd,
        function: 0x50fa7b,
        attribute: 0x50fa7b,
        lifetime: 0xff79c6,
        operator: Some(0xff79c6),
        punctuation: None,
    },
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
    lanes: [
        0xbd93f9, 0xffb86c, 0x50fa7b, 0xff79c6, 0xff5555, 0x8be9fd, 0xf1fa8c, 0x6272a4,
    ],
};

// Gruvbox, https://github.com/morhetz/gruvbox. The dark theme uses the
// bright accents and the light theme the faded ones, as the original does.
const GRUVBOX_DARK: Palette = Palette {
    id: "gruvbox-dark",
    name: "Gruvbox Dark",
    syntax: Syntax {
        comment: 0x928374,
        string: 0xb8bb26,
        escape: 0xfb4934,
        number: 0xd3869b,
        constant: 0xd3869b,
        keyword: 0xfb4934,
        ty: 0xfabd2f,
        function: 0xb8bb26,
        attribute: 0xfabd2f,
        lifetime: 0xb8bb26,
        operator: Some(0x8ec07c),
        punctuation: None,
    },
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
    lanes: [
        0x83a598, 0xfe8019, 0xb8bb26, 0xd3869b, 0xfb4934, 0x8ec07c, 0xfabd2f, 0x928374,
    ],
};

const GRUVBOX_LIGHT: Palette = Palette {
    id: "gruvbox-light",
    name: "Gruvbox Light",
    syntax: Syntax {
        comment: 0x928374,
        string: 0x79740e,
        escape: 0x9d0006,
        number: 0x8f3f71,
        constant: 0x8f3f71,
        keyword: 0x9d0006,
        ty: 0xb57614,
        function: 0x79740e,
        attribute: 0xb57614,
        lifetime: 0x79740e,
        operator: Some(0x427b58),
        punctuation: None,
    },
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
    lanes: [
        0x076678, 0xaf3a03, 0x79740e, 0x8f3f71, 0x9d0006, 0x427b58, 0xb57614, 0x928374,
    ],
};

// Nord, https://www.nordtheme.com/docs/colors-and-palettes. Its comment
// color, nord3, is also a background, and text in it can't be read on the
// panels. So the gray text colors are mixes of nord3 and nord4.
const NORD: Palette = Palette {
    id: "nord",
    name: "Nord",
    syntax: Syntax {
        comment: 0x616e88,
        string: 0xa3be8c,
        escape: 0xebcb8b,
        number: 0xb48ead,
        constant: 0x81a1c1,
        keyword: 0x81a1c1,
        ty: 0x8fbcbb,
        function: 0x88c0d0,
        attribute: 0x8fbcbb,
        lifetime: 0x5e81ac,
        operator: Some(0x81a1c1),
        punctuation: None,
    },
    bg: 0x2e3440,                     // nord0
    panel: 0x3b4252,                  // nord1
    bar: 0x3b4252,                    // nord1
    surface: 0x3b4252,                // nord1
    overlay: 0x434c5e,                // nord2
    raised: 0x4c566a,                 // nord3
    fg: 0xd8dee9,                     // nord4
    dim: mix(0xd8dee9, 0x4c566a, 35), // nord4 and nord3
    faint: mix(0xd8dee9, 0x4c566a, 65),
    accent: 0x88c0d0,  // nord8
    red: 0xbf616a,     // nord11
    yellow: 0xebcb8b,  // nord13
    green: 0xa3be8c,   // nord14
    magenta: 0xb48ead, // nord15
    // nord8, 12, 14, 15, 11, 7, 13, 10
    lanes: [
        0x88c0d0, 0xd08770, 0xa3be8c, 0xb48ead, 0xbf616a, 0x8fbcbb, 0xebcb8b, 0x5e81ac,
    ],
};

// One Half, https://github.com/sonph/onehalf. It has no orange, so the
// graph uses its grays for the last lanes.
const ONE_HALF_DARK: Palette = Palette {
    id: "one-half-dark",
    name: "One Half Dark",
    syntax: Syntax {
        comment: 0x5c6370,
        string: 0x98c379,
        escape: 0x56b6c2,
        number: 0xe5c07b,
        constant: 0xe5c07b,
        keyword: 0xc678dd,
        ty: 0xe5c07b,
        function: 0x61afef,
        attribute: 0xe5c07b,
        lifetime: 0xe5c07b,
        operator: Some(0xc678dd),
        punctuation: None,
    },
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
    lanes: [
        0x61afef, 0xe5c07b, 0x98c379, 0xc678dd, 0xe06c75, 0x56b6c2, 0x919baa, 0xdcdfe4,
    ],
};

const ONE_HALF_LIGHT: Palette = Palette {
    id: "one-half-light",
    name: "One Half Light",
    syntax: Syntax {
        comment: 0xa0a1a7,
        string: 0x50a14f,
        escape: 0x0997b3,
        number: 0xc18401,
        constant: 0xc18401,
        keyword: 0xa626a4,
        ty: 0xc18401,
        function: 0x0184bc,
        attribute: 0xc18401,
        lifetime: 0xc18401,
        operator: Some(0xa626a4),
        punctuation: None,
    },
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
    lanes: [
        0x0184bc, 0xc18401, 0x50a14f, 0xa626a4, 0xe45649, 0x0997b3, 0xa0a1a7, 0x383a42,
    ],
};

// Solarized, https://ethanschoonover.com/solarized. It has only two
// background shades for each mode, so buttons and borders are mixes of the
// second background and the secondary text color. The accents are the same
// in both modes.
const SOLARIZED_DARK: Palette = Palette {
    id: "solarized-dark",
    name: "Solarized Dark",
    syntax: Syntax {
        comment: 0x586e75,
        string: 0x2aa198,
        escape: 0xdc322f,
        number: 0x6c71c4,
        constant: 0xcb4b16,
        keyword: 0x859900,
        ty: 0xb58900,
        function: 0xb58900,
        attribute: 0xb58900,
        lifetime: 0xb58900,
        operator: Some(0x859900),
        punctuation: Some(0x657b83),
    },
    bg: 0x002b36,                         // base03
    panel: 0x073642,                      // base02
    bar: 0x073642,                        // base02
    surface: 0x073642,                    // base02
    overlay: mix(0x073642, 0x586e75, 35), // base02 and base01
    raised: mix(0x073642, 0x586e75, 60),
    fg: 0x839496,  // base0
    dim: 0x586e75, // base01
    faint: mix(0x586e75, 0x073642, 35),
    accent: 0x268bd2,  // blue
    red: 0xdc322f,     // red
    yellow: 0xb58900,  // yellow
    green: 0x859900,   // green
    magenta: 0xd33682, // magenta
    // blue, orange, green, violet, red, cyan, yellow, magenta
    lanes: [
        0x268bd2, 0xcb4b16, 0x859900, 0x6c71c4, 0xdc322f, 0x2aa198, 0xb58900, 0xd33682,
    ],
};

const SOLARIZED_LIGHT: Palette = Palette {
    id: "solarized-light",
    name: "Solarized Light",
    syntax: Syntax {
        comment: 0x93a1a1,
        string: 0x2aa198,
        escape: 0xdc322f,
        number: 0x6c71c4,
        constant: 0xcb4b16,
        keyword: 0x859900,
        ty: 0xb58900,
        function: 0xb58900,
        attribute: 0xb58900,
        lifetime: 0xb58900,
        operator: Some(0x859900),
        punctuation: None,
    },
    bg: 0xfdf6e3,                         // base3
    panel: 0xeee8d5,                      // base2
    bar: 0xeee8d5,                        // base2
    surface: 0xeee8d5,                    // base2
    overlay: mix(0xeee8d5, 0x93a1a1, 35), // base2 and base1
    raised: mix(0xeee8d5, 0x93a1a1, 60),
    fg: 0x657b83,  // base00
    dim: 0x93a1a1, // base1
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
                ("bar_hover", t.bar_hover),
                ("file_hover", t.file_hover),
                ("hunk_hover", t.hunk_hover),
                ("button", t.button),
            ] {
                let c = contrast(fg, hex(bg));
                assert!(
                    c >= 3.0 || c >= base * 0.65,
                    "{id}: text on {what} has contrast {c:.2}"
                );
            }
            for (what, bg) in [
                ("head", t.badge_head),
                ("local", t.badge_local),
                ("remote", t.badge_remote),
                ("tag", t.badge_tag),
                ("danger", t.badge_danger),
            ] {
                let c = contrast(hex(t.badge_fg), hex(bg));
                assert!(
                    c >= 3.0,
                    "{id}: text on the {what} badge has contrast {c:.2}"
                );
            }
        }
    }

    /// Identifier colors differ from each other, from the other syntax
    /// colors and from the text color, and they are about as easy to read
    /// as the text.
    #[test]
    fn identifier_colors_are_distinct_and_readable() {
        for (id, _) in Theme::list() {
            let Some(t) = Theme::named(id) else { continue };
            let (fg, bg) = (hex(t.fg), hex(t.bg));
            let want = (contrast(fg, bg) * 0.8).min(4.5);
            let others: Vec<u32> = t.syntax.drawn().chain([fg]).collect();
            let ids: Vec<u32> = t.identifiers.iter().map(|&c| hex(c)).collect();
            for (i, &a) in ids.iter().enumerate() {
                let c = contrast(a, bg);
                assert!(
                    c >= want - 0.01,
                    "{id}: identifier color {i} has contrast {c:.2}"
                );
                for &b in ids[i + 1..].iter().chain(&others) {
                    let d = distance(oklab(a), oklab(b));
                    assert!(
                        d > 0.05,
                        "{id}: identifier color {a:06x} looks like {b:06x}"
                    );
                }
            }
        }
    }

    #[test]
    fn identifiers_get_colors_by_name() {
        let t = Theme::default();
        let off = t.syntax_colors(false);
        let on = t.syntax_colors(true);
        assert_eq!(off.color(Kind::Identifier, "x"), None);
        assert_eq!(
            off.color(Kind::Keyword, "fn"),
            on.color(Kind::Keyword, "fn")
        );
        assert!(on.color(Kind::Identifier, "x").is_some());
        assert_eq!(
            on.color(Kind::Identifier, "count"),
            on.color(Kind::Identifier, "count")
        );
        // Names that differ by one character still spread over the colors.
        let mut used: Vec<_> = (0..100)
            .filter_map(|i| on.color(Kind::Identifier, &format!("x{i}")))
            .collect();
        used.sort_by_key(|&c| hex(c));
        used.dedup();
        assert!(used.len() >= 14, "only {} colors used", used.len());
    }
}
